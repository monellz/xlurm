use serde_json::Value;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    dir: TempDir,
    daemon: Option<Child>,
    backend: &'static str,
    max_running: usize,
}

impl Harness {
    fn new(backend: &'static str, max_running: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        Self {
            dir,
            daemon: None,
            backend,
            max_running,
        }
    }

    fn command(&self, binary: &str) -> Command {
        let path = match binary {
            "xlurm" => env!("CARGO_BIN_EXE_xlurm"),
            "xrun" => env!("CARGO_BIN_EXE_xrun"),
            "xbatch" => env!("CARGO_BIN_EXE_xbatch"),
            "xqueue" => env!("CARGO_BIN_EXE_xqueue"),
            "xcancel" => env!("CARGO_BIN_EXE_xcancel"),
            "xinfo" => env!("CARGO_BIN_EXE_xinfo"),
            _ => panic!("unknown binary"),
        };
        self.command_path(path)
    }

    fn command_path(&self, path: impl AsRef<Path>) -> Command {
        let mut command = Command::new(path.as_ref());
        command
            .current_dir(self.dir.path())
            .env("XLURM_HOME", self.dir.path().join("state"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.path().join("bin").display()),
            )
            .env("TEST_ROOT", self.dir.path());
        command
    }

    fn run(&self, binary: &str, args: &[&str]) -> Output {
        let output = self.command(binary).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{binary} {args:?}: {}\nDaemon log:\n{}",
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(self.dir.path().join("daemon.log")).unwrap_or_default()
        );
        output
    }

    fn start(&mut self) {
        self.start_with(env!("CARGO_BIN_EXE_xlurm"));
    }

    fn start_with(&mut self, executable: impl AsRef<Path>) {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.path().join("daemon.log"))
            .unwrap();
        self.daemon = Some(
            self.command_path(executable)
                .args([
                    "daemon",
                    "--backend",
                    self.backend,
                    "--max-running",
                    &self.max_running.to_string(),
                ])
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self
                .command("xinfo")
                .arg("--json")
                .output()
                .unwrap()
                .status
                .success()
            {
                break;
            }
            assert!(
                self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon died: {}",
                fs::read_to_string(self.dir.path().join("daemon.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "daemon startup timeout");
            sleep(Duration::from_millis(25));
        }
    }

    fn submit(&self, args: &[&str]) -> u64 {
        String::from_utf8(self.run("xbatch", args).stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn job(&self, id: u64) -> Value {
        serde_json::from_slice(&self.run("xqueue", &[&id.to_string(), "--json"]).stdout).unwrap()
    }

    fn wait_state(&self, id: u64, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let job = self.job(id);
            if job["state"] == state {
                return job;
            }
            assert!(
                Instant::now() < deadline,
                "job {id}: wanted {state}, got {job}"
            );
            sleep(Duration::from_millis(30));
        }
    }

    fn log(&self, id: u64) -> String {
        fs::read_to_string(self.dir.path().join(format!("state/jobs/{id}.log"))).unwrap()
    }

    fn drivers(&self) {
        // Do not write executables while other test threads spawn processes:
        // fork can inherit the writer before CLOEXEC takes effect, making a
        // just-written script fail with ETXTBSY even after fs::write returns.
        // Links also work when the test's temporary directory is mounted noexec.
        for name in ["nvidia-smi", "npu-smi"] {
            symlink(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures")
                    .join(name),
                self.dir.path().join("bin").join(name),
            )
            .unwrap();
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Best effort cleanup also runs when assertions panic.
        if let Ok(output) = self.command("xqueue").args(["--all", "--json"]).output()
            && let Ok(jobs) = serde_json::from_slice::<Vec<Value>>(&output.stdout)
        {
            for job in jobs {
                if job["state"] == "RUNNING" || job["state"] == "PENDING" {
                    let _ = self
                        .command("xqueue")
                        .args(["--cancel", &job["id"].to_string()])
                        .output();
                }
            }
        }
        if let Some(mut daemon) = self.daemon.take() {
            let _ = self.command("xlurm").arg("stop").output();
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

#[test]
fn running_daemon_starts_workers_after_its_executable_is_replaced() {
    let mut h = Harness::new("none", 1);
    let daemon = h.dir.path().join("xlurm-daemon");
    fs::copy(env!("CARGO_BIN_EXE_xlurm"), &daemon).unwrap();
    h.start_with(&daemon);

    // Reproduce an in-place package upgrade: /proc/<pid>/exe now names a
    // deleted inode even though a new executable exists at the original path.
    fs::remove_file(&daemon).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_xlurm"), &daemon).unwrap();

    let output = h
        .command("xrun")
        .args(["-g", "0", "-t", "5", "/bin/true"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\nDaemon log:\n{}",
        String::from_utf8_lossy(&output.stdout),
        fs::read_to_string(h.dir.path().join("daemon.log")).unwrap_or_default()
    );
    assert_eq!(h.wait_state(1, "COMPLETED")["result"]["exit_code"], 0);
}

#[test]
fn foreground_preserves_arguments_environment_logs_and_exit_status() {
    let mut h = Harness::new("none", 4);
    h.start();
    let output = h.command("xrun").env("SUBMIT_ONLY", "hello from submitter")
        .args(["-g", "0", "sh", "-c",
            "printf '%s|%s|%s|%s' \"$1\" \"$SUBMIT_ONLY\" \"$CUDA_VISIBLE_DEVICES\" \"$ASCEND_RT_VISIBLE_DEVICES\"; shift; printf '<%s>' \"$@\"; echo err >&2; exit 7",
            "name", "a 'quote' $(touch injected); with spaces", "--gpus", "2", "--name", "payload", "--help", "--", "-7"])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("a 'quote' $(touch injected); with spaces|hello from submitter|-1|-1"));
    assert!(text.contains("<--gpus><2><--name><payload><--help><--><-7>"));
    assert!(text.contains("err"));
    assert!(!h.dir.path().join("injected").exists());
    let failed = h.wait_state(1, "FAILED");
    assert_eq!(failed["result"]["exit_code"], 7);
    let missing = h
        .command("xrun")
        .args(["-g", "0", "--", "/does-not-exist-xlurm"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(
        h.job(2)["result"]["error"]
            .as_str()
            .unwrap()
            .contains("cannot execute")
    );
    assert!(
        !h.command("xrun")
            .args(["--", "true"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn batch_snapshot_pending_cancel_and_restart_recovery() {
    let mut h = Harness::new("none", 1);
    h.start();
    let first = h.submit(&[
        "-g",
        "0",
        "--wrap",
        "echo once >> starts; while [ ! -f release ]; do sleep 0.05; done; echo survived",
    ]);
    h.wait_state(first, "RUNNING");
    while !h.dir.path().join("starts").exists() {
        sleep(Duration::from_millis(20));
    }
    fs::write(
        h.dir.path().join("script with spaces.sh"),
        "printf 'original:%s:%s' \"$1\" \"$2\"",
    )
    .unwrap();
    let second = h.submit(&[
        "-g",
        "0",
        "script with spaces.sh",
        "--label",
        "arg with space",
    ]);
    fs::write(h.dir.path().join("script with spaces.sh"), "echo modified").unwrap();
    assert_eq!(h.job(second)["state"], "PENDING");
    let queue = String::from_utf8(h.run("xqueue", &[]).stdout).unwrap();
    assert!(queue.contains("STARTED (UTC+8)"));
    let running_detail = String::from_utf8(h.run("xqueue", &[&first.to_string()]).stdout).unwrap();
    let started_at = running_detail
        .lines()
        .find_map(|line| line.strip_prefix("Started at: "))
        .unwrap()
        .strip_suffix(" UTC+8")
        .unwrap();
    let (_, short_started_at) = started_at.split_once('-').unwrap();
    assert!(queue.contains(short_started_at));
    assert!(!queue.contains(started_at));
    let all_queue = String::from_utf8(h.run("xqueue", &["--all"]).stdout).unwrap();
    assert!(all_queue.contains(started_at));
    assert!(
        queue
            .lines()
            .any(|line| line.contains("PENDING") && line.contains("-"))
    );
    let detail = String::from_utf8(h.run("xqueue", &[&second.to_string()]).stdout).unwrap();
    assert!(detail.contains("Started at: -"));
    assert!(detail.contains("Wait time: "));
    assert!(detail.contains("Run time: -"));
    let summaries: Vec<Value> =
        serde_json::from_slice(&h.run("xqueue", &["--json"]).stdout).unwrap();
    assert!(summaries.iter().all(|job| job["submitted_at"].is_u64()));
    assert!(summaries.iter().all(|job| job.get("started_at").is_some()));
    assert!(summaries.iter().all(|job| job.get("finished_at").is_some()));
    let cancelled = h.submit(&["-g", "0", "--wrap", "touch must-not-run"]);
    h.run("xcancel", &[&cancelled.to_string()]);
    h.wait_state(cancelled, "CANCELLED");

    let mut daemon = h.daemon.take().unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    // Existing work survives abrupt scheduler death; a restarted daemon adopts it.
    h.start();
    assert_eq!(h.job(first)["state"], "RUNNING");
    assert_eq!(h.job(second)["state"], "PENDING");
    fs::write(h.dir.path().join("release"), "").unwrap();
    h.wait_state(first, "COMPLETED");
    h.wait_state(second, "COMPLETED");
    assert_eq!(
        fs::read_to_string(h.dir.path().join("starts")).unwrap(),
        "once\n"
    );
    assert_eq!(h.log(second), "original:--label:arg with space");
    assert!(!h.dir.path().join("must-not-run").exists());
    // Completed results and monotonically increasing IDs survive a clean restart.
    h.run("xlurm", &["stop"]);
    h.daemon.as_mut().unwrap().wait().unwrap();
    h.daemon = None;
    h.start();
    assert_eq!(h.job(first)["state"], "COMPLETED");
    assert!(h.submit(&["-g", "0", "--wrap", "true"]) > cancelled);
}

#[test]
fn both_vendors_are_exclusive_and_external_busy_or_unknown_devices_wait() {
    let mut h = Harness::new("auto", 4);
    h.drivers();
    fs::write(h.dir.path().join("busy"), "").unwrap();
    h.start();
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(
        devices.len(),
        3,
        "mock inventory was not discovered: {devices:?}\n{}",
        fs::read_to_string(h.dir.path().join("daemon.log")).unwrap()
    );
    assert_eq!(devices[0]["device"]["visible"], "GPU-test-uuid");
    assert_eq!(devices[1]["device"]["visible"], "2");
    assert_eq!(devices[2]["device"]["visible"], "3");
    let gpu = h.submit(&[
        "--device",
        "nvidia",
        "--wrap",
        "echo \"$CUDA_VISIBLE_DEVICES\"; while [ ! -f release ]; do sleep 0.05; done",
    ]);
    assert_eq!(h.job(gpu)["state"], "PENDING");
    let npu = h.run(
        "xrun",
        &[
            "--device",
            "ascend",
            "-g",
            "2",
            "--",
            "sh",
            "-c",
            "printf '%s|%s' \"$ASCEND_RT_VISIBLE_DEVICES\" \"$ASCEND_DEVICE_ID\"",
        ],
    );
    assert_eq!(String::from_utf8(npu.stdout).unwrap(), "2,3|0");
    assert!(
        fs::read_to_string(h.dir.path().join("npu-queries"))
            .unwrap()
            .contains("-i 4 -c 1")
    );
    fs::write(h.dir.path().join("probe-error"), "").unwrap();
    fs::remove_file(h.dir.path().join("busy")).unwrap();
    // Wait until xinfo observes failed monitoring instead of guessing idle.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let devices: Vec<Value> =
            serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
        if devices[0]["status"] == "unknown" {
            break;
        }
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(100));
    }
    assert_eq!(h.job(gpu)["state"], "PENDING");
    fs::remove_file(h.dir.path().join("probe-error")).unwrap();
    h.wait_state(gpu, "RUNNING");
    let next = h.submit(&["--device", "nvidia", "--wrap", "echo second"]);
    assert_eq!(h.job(next)["state"], "PENDING");
    assert!(
        !h.command("xbatch")
            .args(["-g", "3", "--wrap", "true"])
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(h.dir.path().join("release"), "").unwrap();
    h.wait_state(gpu, "COMPLETED");
    h.wait_state(next, "COMPLETED");
    assert_eq!(h.log(gpu), "GPU-test-uuid\n");
}

#[test]
fn cancellation_and_timeout_kill_process_groups() {
    let mut h = Harness::new("none", 2);
    h.start();
    let mut run = h
        .command("xrun")
        .args([
            "-g",
            "0",
            "--",
            "sh",
            "-c",
            "trap '' TERM; sleep 60 & echo $! > child.pid; wait",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !h.dir.path().join("child.pid").exists() {
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(25));
    }
    let pid: i32 = fs::read_to_string(h.dir.path().join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe {
        libc::kill(run.id() as i32, libc::SIGINT);
    }
    loop {
        if let Some(status) = run.try_wait().unwrap() {
            assert_eq!(status.code(), Some(130));
            break;
        }
        assert!(Instant::now() < deadline, "xrun did not cancel");
        sleep(Duration::from_millis(25));
    }
    h.wait_state(1, "CANCELLED");
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "grandchild survived cancellation"
    );
    let timed = h.submit(&["-g", "0", "--time-limit", "1", "--wrap", "sleep 60"]);
    assert_eq!(h.wait_state(timed, "TIMED_OUT")["result"]["exit_code"], 124);
}

#[test]
fn background_start_and_single_daemon_lock() {
    let h = Harness::new("none", 2);
    h.run("xlurm", &["start", "--backend", "none"]);
    h.run("xlurm", &["start", "--backend", "none"]);
    let duplicate = h
        .command("xlurm")
        .args(["daemon", "--backend", "none"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already running"));
    h.run("xrun", &["-g", "0", "--", "true"]);
    h.run("xlurm", &["stop"]);
}

#[test]
fn restart_replaces_an_idle_daemon_and_refuses_running_jobs() {
    let mut h = Harness::new("none", 1);
    h.start();
    h.run("xlurm", &["restart", "--backend", "none"]);
    h.daemon.as_mut().unwrap().wait().unwrap();

    let job = h.submit(&[
        "-g",
        "0",
        "--wrap",
        "while [ ! -f release ]; do sleep 0.05; done",
    ]);
    h.wait_state(job, "RUNNING");
    let refused = h
        .command("xlurm")
        .args(["restart", "--backend", "none"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("cannot restart while jobs are running: 1")
    );
    assert_eq!(h.job(job)["state"], "RUNNING");

    fs::write(h.dir.path().join("release"), "").unwrap();
    h.wait_state(job, "COMPLETED");
    h.run("xlurm", &["stop"]);
}

#[test]
fn clean_requires_an_idle_stopped_scheduler_and_removes_only_logs() {
    let mut h = Harness::new("none", 1);
    h.start();
    let job = h.submit(&[
        "-g",
        "0",
        "--wrap",
        "echo existing-output; while [ ! -f release ]; do sleep 0.05; done",
    ]);
    h.wait_state(job, "RUNNING");
    fs::write(h.dir.path().join("state/daemon.log"), "diagnostic\n").unwrap();

    let online = h.command("xlurm").arg("clean").output().unwrap();
    assert!(!online.status.success());
    assert!(String::from_utf8_lossy(&online.stderr).contains("xlurm is running"));

    h.run("xlurm", &["stop"]);
    h.daemon.as_mut().unwrap().wait().unwrap();
    h.daemon = None;
    let active = h.command("xlurm").arg("clean").output().unwrap();
    assert!(!active.status.success());
    assert!(
        String::from_utf8_lossy(&active.stderr)
            .contains("cannot clean logs while jobs are running: 1")
    );
    assert!(h.dir.path().join("state/daemon.log").exists());
    assert!(h.dir.path().join("state/jobs/1.log").exists());

    fs::write(h.dir.path().join("release"), "").unwrap();
    h.start();
    h.wait_state(job, "COMPLETED");
    h.run("xlurm", &["stop"]);
    h.daemon.as_mut().unwrap().wait().unwrap();
    h.daemon = None;

    let output = String::from_utf8(h.run("xlurm", &["clean"]).stdout).unwrap();
    assert!(output.contains("Removed 1 job log(s) and daemon.log."));
    assert!(!h.dir.path().join("state/daemon.log").exists());
    assert!(!h.dir.path().join("state/jobs/1.log").exists());
    let state: Value =
        serde_json::from_slice(&fs::read(h.dir.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(state["jobs"][0]["state"], "COMPLETED");
}

#[test]
fn restart_cleans_expired_history_without_disrupting_live_jobs() {
    let mut h = Harness::new("none", 1);
    h.start();
    let old = h.submit(&["-g", "0", "--wrap", "echo old-output"]);
    h.wait_state(old, "COMPLETED");
    let running = h.submit(&[
        "-g",
        "0",
        "--wrap",
        "while [ ! -f release ]; do sleep 0.05; done; echo survived",
    ]);
    h.wait_state(running, "RUNNING");
    let pending = h.submit(&["-g", "0", "--wrap", "echo pending-output"]);
    h.wait_state(pending, "PENDING");
    h.run("xlurm", &["stop"]);
    h.daemon.take().unwrap().wait().unwrap();

    let state_path = h.dir.path().join("state/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["jobs"][0]["result"]["finished_at"] =
        Value::from(xlurm::model::now() - 7 * 24 * 60 * 60 - 1);
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    h.start();

    let missing = h
        .command("xqueue")
        .args([&old.to_string(), "--log"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("job not found"));
    for entry in fs::read_dir(h.dir.path().join("state/jobs")).unwrap() {
        assert!(
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{old}."))
        );
    }
    let history: Vec<Value> =
        serde_json::from_slice(&h.run("xqueue", &["--all", "--json"]).stdout).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(h.job(running)["state"], "RUNNING");
    assert_eq!(h.job(pending)["state"], "PENDING");
    fs::write(h.dir.path().join("release"), "").unwrap();
    h.wait_state(running, "COMPLETED");
    h.wait_state(pending, "COMPLETED");
    assert_eq!(h.log(running), "survived\n");
    assert_eq!(h.log(pending), "pending-output\n");
    assert!(h.submit(&["-g", "0", "--wrap", "true"]) > pending);
}
