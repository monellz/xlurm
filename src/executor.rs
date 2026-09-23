//! The scheduler knows only Executor; process ownership lives here.
use crate::model::{Job, Kind, Outcome, State, now};
use crate::storage::{Paths, read_json, try_lock, write_json};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub trait Executor {
    fn start(&mut self, job: &Job) -> Result<()>;
    fn poll(&mut self, id: u64) -> Result<Option<Outcome>>;
    fn cancel(&mut self, id: u64) -> Result<()>;
}

pub struct ProcessExecutor {
    paths: Paths,
    children: HashMap<u64, Child>,
}

impl ProcessExecutor {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            children: HashMap::new(),
        }
    }
}

impl Executor for ProcessExecutor {
    fn start(&mut self, job: &Job) -> Result<()> {
        let lock = try_lock(&self.paths.job(job.id, "lock"))?.context("worker already running")?;
        write_json(&self.paths.job(job.id, "json"), job)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.paths.job(job.id, "log"))?;
        let fd = lock.as_raw_fd();
        // Keep workers on the daemon's exact binary. current_exe() resolves
        // this link and appends " (deleted)" after an in-place upgrade, which
        // leaves subsequent worker spawns failing with ENOENT.
        let mut command = Command::new("/proc/self/exe");
        command
            .arg("__worker")
            .arg(job.id.to_string())
            .arg(fd.to_string())
            .env("XLURM_HOME", &self.paths.0)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // Only this lock crosses exec. The daemon's lock/socket remain CLOEXEC.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("cannot start process executor worker")?;
        self.children.insert(job.id, child);
        Ok(())
    }

    fn poll(&mut self, id: u64) -> Result<Option<Outcome>> {
        if let Some(child) = self.children.get_mut(&id)
            && child.try_wait()?.is_some()
        {
            self.children.remove(&id);
        }
        // Do not release allocation until the worker has finished cleanup and
        // closed its lock, even if the durable result is already visible.
        let Some(_lock) = try_lock(&self.paths.job(id, "lock"))? else {
            return Ok(None);
        };
        if let Some(mut child) = self.children.remove(&id) {
            child.wait()?;
        }
        let result_path = self.paths.job(id, "result");
        if result_path.exists() {
            return Ok(Some(read_json(&result_path)?));
        }
        Ok(Some(failure(
            "executor worker disappeared without an exit result",
        )))
    }

    fn cancel(&mut self, id: u64) -> Result<()> {
        write_json(&self.paths.job(id, "cancel"), &true)
    }
}

pub fn failure(message: impl Into<String>) -> Outcome {
    Outcome {
        state: State::Failed,
        exit_code: 1,
        finished_at: now(),
        error: Some(message.into()),
    }
}

pub fn worker(paths: &Paths, id: u64, fd: i32) -> Result<()> {
    ensure!(fd >= 3, "invalid worker lock descriptor");
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != -1,
        "missing inherited worker lock"
    );
    // The lock must not leak into the user's processes.
    let _lock = unsafe { File::from_raw_fd(fd) };
    let job: Job = read_json(&paths.job(id, "json"))?;
    crate::install_signals()?;
    // Adopt orphaned grandchildren so ordinary training process trees can be
    // reaped before their devices are returned to the scheduler.
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } == 0,
        "cannot enable child subreaper"
    );
    let result = execute(paths, &job).unwrap_or_else(|error| failure(format!("{error:#}")));
    write_json(&paths.job(id, "result"), &result)
}

fn execute(paths: &Paths, job: &Job) -> Result<Outcome> {
    let mut argv = job.spec.command.clone();
    if let Some(script) = &job.spec.script {
        // The user cannot read the private spool. bash -c receives the immutable
        // submitted text, with the script name as $0 and all arguments intact.
        argv.splice(
            0..0,
            [
                "bash".into(),
                "-c".into(),
                script.clone(),
                job.spec.name.clone(),
            ],
        );
    }
    let owner = crate::auth::account(job.owner.uid, job.owner.gid)?;
    ensure!(
        owner.name == job.owner.name,
        "job owner's account changed since submission"
    );
    let privileged = unsafe { libc::geteuid() } == 0;
    ensure!(
        privileged
            || (owner.uid == unsafe { libc::geteuid() } && owner.gid == unsafe { libc::getegid() }),
        "executor cannot assume the submitting user's identity"
    );
    let groups = if privileged {
        crate::auth::groups(&owner)?
    } else {
        vec![]
    };
    let cwd = CString::new(job.spec.cwd.as_os_str().as_bytes())?;
    let mut command = Command::new(argv.first().context("empty command")?);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(&job.spec.env)
        .env("USER", &owner.name)
        .env("LOGNAME", &owner.name)
        .env("XLURM_JOB_ID", job.id.to_string())
        .env("XLURM_SUBMIT_DIR", &job.spec.cwd)
        .env("CUDA_VISIBLE_DEVICES", visible(job, Kind::Nvidia))
        .env("ASCEND_RT_VISIBLE_DEVICES", visible(job, Kind::Ascend))
        .env_remove("ASCEND_DEVICE_ID")
        .stdin(Stdio::null())
        .process_group(0);
    if job.devices.iter().any(|d| d.kind == Kind::Ascend) {
        command.env("ASCEND_DEVICE_ID", "0");
    }
    let parent = std::process::id() as i32;
    unsafe {
        command.pre_exec(move || {
            // Only async-signal-safe syscalls after fork. Drop supplementary,
            // real/effective/saved IDs before entering any user-selected path
            // or executing code with the submitted environment.
            if privileged
                && (libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setresgid(owner.gid, owner.gid, owner.gid) != 0
                    || libc::setresuid(owner.uid, owner.uid, owner.uid) != 0)
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::chdir(cwd.as_ptr()) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(125);
            }
            Ok(())
        });
    }
    // Cancellation may have arrived before this worker finished starting.
    if paths.job(job.id, "cancel").exists() {
        return Ok(Outcome {
            state: State::Cancelled,
            exit_code: 130,
            finished_at: now(),
            error: None,
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("cannot execute {}", argv[0]))?;
    let mut running = Running(child);
    let started = Instant::now();
    let mut termination: Option<(Instant, State)> = None;
    let status = loop {
        if let Some(status) = running.0.try_wait()? {
            break status;
        }
        if termination.is_none() {
            let reason = if paths.job(job.id, "cancel").exists() || crate::interrupted() {
                Some(State::Cancelled)
            } else if job
                .spec
                .time_limit
                .is_some_and(|limit| started.elapsed().as_secs() >= limit)
            {
                Some(State::TimedOut)
            } else {
                None
            };
            if let Some(reason) = reason {
                running.signal(libc::SIGTERM);
                termination = Some((Instant::now(), reason));
            }
        }
        if termination.is_some_and(|(when, _)| when.elapsed() >= Duration::from_secs(1)) {
            running.signal(libc::SIGKILL);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // A parent exiting does not give its background children ownership of a GPU.
    drop(running);
    let state = termination
        .map(|(_, state)| state)
        .unwrap_or(if status.success() {
            State::Completed
        } else {
            State::Failed
        });
    let exit_code = match state {
        State::Cancelled => 130,
        State::TimedOut => 124,
        _ => status.code().unwrap_or(128 + status.signal().unwrap_or(0)),
    };
    Ok(Outcome {
        state,
        exit_code,
        finished_at: now(),
        error: None,
    })
}

fn visible(job: &Job, kind: Kind) -> String {
    let values: Vec<_> = job
        .devices
        .iter()
        .filter(|d| d.kind == kind)
        .map(|d| d.visible.as_str())
        .collect();
    if values.is_empty() {
        "-1".into()
    } else {
        values.join(",")
    }
}

/// All error paths clean up the payload group as well as the happy path.
struct Running(Child);

impl Running {
    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(-(self.0.id() as i32), signal);
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
        let _ = self.0.wait();
        // Reap adopted descendants in the payload group. Jobs that deliberately
        // escape their group are outside this cooperative scheduler's contract.
        loop {
            let pid = unsafe { libc::waitpid(-(self.0.id() as i32), std::ptr::null_mut(), 0) };
            if pid > 0 {
                continue;
            }
            if pid == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
    }
}
