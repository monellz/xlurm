use crate::device::Inventory;
use crate::executor::{Executor, failure};
use crate::model::*;
use crate::storage::{Paths, read_json, write_json};
use anyhow::{Context, Result, ensure};
use std::collections::HashSet;
use std::fs;

const HISTORY_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

pub struct Scheduler<E> {
    pub store: Store,
    pub inventory: Inventory,
    executor: E,
    paths: Paths,
    max_running: usize,
}

impl<E: Executor> Scheduler<E> {
    pub fn new(
        paths: Paths,
        inventory: Inventory,
        executor: E,
        max_running: usize,
    ) -> Result<Self> {
        ensure!(max_running > 0, "max-running must be positive");
        let path = paths.0.join("state.json");
        let store = if path.exists() {
            read_json(&path)?
        } else {
            Store::default()
        };
        Ok(Self {
            store,
            inventory,
            executor,
            paths,
            max_running,
        })
    }

    fn save(&self) -> Result<()> {
        write_json(&self.paths.0.join("state.json"), &self.store)
    }

    pub fn cleanup(&mut self, timestamp: u64) -> Result<()> {
        let expired: HashSet<_> = self
            .store
            .jobs
            .iter()
            .filter(|job| {
                job.state.terminal()
                    && job.result.as_ref().is_some_and(|result| {
                        timestamp.saturating_sub(result.finished_at) >= HISTORY_RETENTION_SECS
                    })
            })
            .map(|job| job.id)
            .collect();
        if expired.is_empty() {
            return Ok(());
        }

        // Delete artifacts before forgetting their IDs. If cleanup or saving is
        // interrupted, the persisted history lets the next pass retry safely.
        // Terminal jobs no longer have a worker writing into their files.
        for entry in fs::read_dir(self.paths.0.join("jobs"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some((id, extension)) = name.to_str().and_then(|name| name.split_once('.')) else {
                continue;
            };
            let Ok(id) = id.parse::<u64>() else {
                continue;
            };
            let is_job_file = matches!(extension, "json" | "log" | "lock" | "result" | "cancel")
                || extension
                    .strip_prefix("tmp.")
                    .is_some_and(|pid| pid.parse::<u32>().is_ok());
            if expired.contains(&id) && is_job_file {
                match fs::remove_file(entry.path()) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let retained = Store {
            next_id: self.store.next_id,
            jobs: self
                .store
                .jobs
                .iter()
                .filter(|job| !expired.contains(&job.id))
                .cloned()
                .collect(),
        };
        write_json(&self.paths.0.join("state.json"), &retained)?;
        self.store = retained;
        Ok(())
    }

    pub fn submit(&mut self, spec: Submission, owner: Owner) -> Result<Job> {
        ensure!(
            spec.script.is_some() || !spec.command.is_empty(),
            "empty command"
        );
        ensure!(spec.cwd.is_dir(), "working directory does not exist");
        ensure!(spec.time_limit != Some(0), "time-limit must be positive");
        ensure!(
            allocate(&spec, &self.inventory.devices, &HashSet::new()).is_some(),
            "requested {} device(s), but no matching pool has enough devices",
            spec.count
        );
        let id = self
            .store
            .next_id
            .checked_add(1)
            .context("job ID exhausted")?;
        let job = Job {
            id,
            owner,
            spec,
            state: State::Pending,
            devices: vec![],
            submitted_at: now(),
            started_at: None,
            result: None,
        };
        self.store.next_id = id;
        self.store.jobs.push(job.clone());
        if let Err(error) = self.save() {
            self.store.jobs.pop();
            return Err(error);
        }
        Ok(job)
    }

    pub fn get(&self, id: u64) -> Result<Job> {
        self.store
            .jobs
            .iter()
            .find(|j| j.id == id)
            .cloned()
            .context("job not found")
    }

    pub fn cancel(&mut self, id: u64) -> Result<()> {
        let job = self
            .store
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .context("job not found")?;
        match job.state {
            State::Pending => {
                job.state = State::Cancelled;
                job.result = Some(Outcome {
                    state: State::Cancelled,
                    exit_code: 130,
                    finished_at: now(),
                    error: None,
                });
                self.save()?;
            }
            State::Running => self.executor.cancel(id)?,
            _ => {}
        }
        Ok(())
    }

    pub fn tick(&mut self) -> Result<()> {
        let mut changed = false;
        for job in &mut self.store.jobs {
            if job.state == State::Running
                && let Some(outcome) = self.executor.poll(job.id)?
            {
                job.state = outcome.state;
                job.result = Some(outcome);
                changed = true;
            }
        }
        if changed {
            self.save()?;
        }
        let mut reserved: HashSet<_> = self
            .store
            .jobs
            .iter()
            .filter(|j| j.state == State::Running)
            .flat_map(|j| j.devices.iter().map(Device::key))
            .collect();
        reserved.extend(
            self.inventory
                .devices
                .iter()
                .filter(|d| {
                    self.inventory
                        .status
                        .get(&d.key())
                        .is_none_or(|s| s != "idle")
                })
                .map(Device::key),
        );
        let mut running = self
            .store
            .jobs
            .iter()
            .filter(|j| j.state == State::Running)
            .count();
        for i in 0..self.store.jobs.len() {
            if running >= self.max_running {
                break;
            }
            let job = &self.store.jobs[i];
            if job.state != State::Pending {
                continue;
            }
            let Some(devices) = allocate(&job.spec, &self.inventory.devices, &reserved) else {
                continue;
            };
            let job = &mut self.store.jobs[i];
            job.devices = devices;
            job.state = State::Running;
            job.started_at = Some(now());
            // Persist intent before spawning: restart never submits a running
            // job twice, including a crash between fork and the first poll.
            self.save()?;
            let job = &mut self.store.jobs[i];
            if let Err(error) = self.executor.start(job) {
                job.state = State::Failed;
                job.result = Some(failure(format!("{error:#}")));
                self.save()?;
                continue;
            }
            reserved.extend(job.devices.iter().map(Device::key));
            running += 1;
        }
        Ok(())
    }

    pub fn info(&self) -> Vec<DeviceView> {
        self.inventory
            .devices
            .iter()
            .map(|device| {
                let job = self
                    .store
                    .jobs
                    .iter()
                    .find(|job| {
                        job.state == State::Running
                            && job.devices.iter().any(|d| d.key() == device.key())
                    })
                    .map(|j| j.id);
                DeviceView {
                    device: device.clone(),
                    job,
                    status: if job.is_some() {
                        "allocated".into()
                    } else {
                        self.inventory
                            .status
                            .get(&device.key())
                            .cloned()
                            .unwrap_or("unknown".into())
                    },
                }
            })
            .collect()
    }
}

fn allocate(
    spec: &Submission,
    devices: &[Device],
    reserved: &HashSet<String>,
) -> Option<Vec<Device>> {
    if spec.count == 0 {
        return Some(vec![]);
    }
    for kind in [Kind::Nvidia, Kind::Ascend] {
        if spec.kind.is_some_and(|wanted| wanted != kind) {
            continue;
        }
        let selected: Vec<_> = devices
            .iter()
            .filter(|d| d.kind == kind && !reserved.contains(&d.key()))
            .take(spec.count)
            .cloned()
            .collect();
        if selected.len() == spec.count {
            return Some(selected);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Backend;
    use crate::executor::ProcessExecutor;

    fn history_scheduler(paths: &Paths) -> Scheduler<ProcessExecutor> {
        paths.initialize().unwrap();
        Scheduler::new(
            paths.clone(),
            Inventory::discover(Backend::None).unwrap(),
            ProcessExecutor::new(paths.clone()),
            1,
        )
        .unwrap()
    }

    fn history_job(id: u64, state: State, finished_at: u64) -> Job {
        Job {
            id,
            owner: Owner {
                uid: 1234,
                gid: 1234,
                name: "alice".into(),
            },
            spec: Submission {
                command: vec!["true".into()],
                cwd: "/".into(),
                env: Default::default(),
                name: "history".into(),
                count: 0,
                kind: None,
                time_limit: None,
                script: None,
            },
            state,
            devices: vec![],
            submitted_at: 0,
            started_at: Some(0),
            result: Some(Outcome {
                state,
                exit_code: 0,
                finished_at,
                error: None,
            }),
        }
    }

    #[test]
    fn cleanup_expires_only_finished_jobs_and_preserves_ids_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths(directory.path().join("state"));
        let mut scheduler = history_scheduler(&paths);
        let timestamp = 2 * HISTORY_RETENTION_SECS;
        for (index, state) in [
            State::Completed,
            State::Failed,
            State::Cancelled,
            State::TimedOut,
            State::Pending,
            State::Running,
            State::Completed,
            State::Failed,
            State::Completed,
        ]
        .into_iter()
        .enumerate()
        {
            let id = index as u64 + 1;
            let finished_at = match id {
                7 => timestamp - HISTORY_RETENTION_SECS + 1,
                8 => timestamp + 100,
                9 => timestamp - 24 * 60 * 60,
                _ => timestamp - HISTORY_RETENTION_SECS,
            };
            scheduler
                .store
                .jobs
                .push(history_job(id, state, finished_at));
            for extension in ["json", "log", "lock", "result", "cancel", "tmp.123"] {
                fs::write(paths.job(id, extension), "artifact").unwrap();
            }
        }
        scheduler.store.next_id = 9;
        scheduler.save().unwrap();
        fs::write(paths.job(11, "log"), "unrelated").unwrap();
        // A prior interrupted cleanup may have already removed some files.
        fs::remove_file(paths.job(1, "log")).unwrap();
        scheduler.cleanup(timestamp).unwrap();
        assert_eq!(
            scheduler
                .store
                .jobs
                .iter()
                .map(|job| job.id)
                .collect::<Vec<_>>(),
            vec![5, 6, 7, 8, 9]
        );
        for id in 1..=9 {
            for extension in ["json", "log", "lock", "result", "cancel", "tmp.123"] {
                assert_eq!(paths.job(id, extension).exists(), id > 4);
            }
        }
        assert_eq!(fs::read(paths.job(11, "log")).unwrap(), b"unrelated");
        let mut restarted = history_scheduler(&paths);
        assert!(restarted.get(1).is_err());
        assert!(restarted.get(7).is_ok());
        restarted.cleanup(timestamp + 1).unwrap();
        assert!(restarted.get(7).is_err());
        assert!(restarted.get(8).is_ok());
        let job = history_job(0, State::Pending, 0);
        assert_eq!(restarted.submit(job.spec, job.owner).unwrap().id, 10);
    }

    #[test]
    fn cleanup_can_retry_after_a_file_cannot_be_deleted() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths(directory.path().join("state"));
        let mut scheduler = history_scheduler(&paths);
        scheduler
            .store
            .jobs
            .push(history_job(1, State::Completed, 0));
        scheduler.store.next_id = 1;
        scheduler.save().unwrap();
        fs::create_dir(paths.job(1, "log")).unwrap();
        assert!(scheduler.cleanup(HISTORY_RETENTION_SECS).is_err());
        assert!(scheduler.get(1).is_ok());
        assert!(history_scheduler(&paths).get(1).is_ok());
        fs::remove_dir(paths.job(1, "log")).unwrap();
        scheduler.cleanup(HISTORY_RETENTION_SECS).unwrap();
        assert!(history_scheduler(&paths).get(1).is_err());
    }

    #[test]
    fn allocation_is_exclusive_and_never_mixes_vendors() {
        let devices = vec![
            Device {
                kind: Kind::Nvidia,
                id: 0,
                name: "gpu".into(),
                visible: "GPU-a".into(),
                npu: None,
                chip: None,
            },
            Device {
                kind: Kind::Ascend,
                id: 0,
                name: "npu".into(),
                visible: "0".into(),
                npu: Some(0),
                chip: None,
            },
        ];
        let mut spec = Submission {
            command: vec!["true".into()],
            cwd: "/".into(),
            env: Default::default(),
            name: "test".into(),
            count: 2,
            kind: None,
            time_limit: None,
            script: None,
        };
        assert!(allocate(&spec, &devices, &HashSet::new()).is_none());
        spec.count = 1;
        let reserved = HashSet::from([devices[0].key()]);
        assert_eq!(
            allocate(&spec, &devices, &reserved).unwrap()[0].kind,
            Kind::Ascend
        );
        spec.kind = Some(Kind::Nvidia);
        assert!(allocate(&spec, &devices, &reserved).is_none());
        spec.count = 0;
        assert!(allocate(&spec, &[], &reserved).unwrap().is_empty());
    }
}
