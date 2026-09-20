use crate::device::Inventory;
use crate::executor::{Executor, failure};
use crate::model::*;
use crate::storage::{Paths, read_json, write_json};
use anyhow::{Context, Result, ensure};
use std::collections::HashSet;

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
