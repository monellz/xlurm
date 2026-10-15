use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
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
            .env("XLURM_DCMI_LIBRARY", self.dir.path().join("libdcmi.so"))
            .env("XLURM_MXSML_LIBRARY", self.dir.path().join("libmxsml.so"))
            .env("XLURM_HGML_LIBRARY", self.dir.path().join("libhgml.so"))
            .env(
                "XLURM_HCU_SMI_LIBRARY",
                self.dir.path().join("libhcu_smi.so"),
            )
            .env(
                "XLURM_HCU_HEALTH_TOOL",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hy-smi"),
            )
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
        self.start_with_args(executable, &[]);
    }

    fn start_with_args(&mut self, executable: impl AsRef<Path>, args: &[&str]) {
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
                .args(args)
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
        self.dcmi();
        // Do not write executables while other test threads spawn processes:
        // fork can inherit the writer before CLOEXEC takes effect, making a
        // just-written script fail with ETXTBSY even after fs::write returns.
        // Links also work when the test's temporary directory is mounted noexec.
        symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nvidia-smi"),
            self.dir.path().join("bin/nvidia-smi"),
        )
        .unwrap();
    }

    fn mxsml(&self, flags: &[&str]) {
        let output = Command::new("cc")
            .args(flags)
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(self.dir.path().join("libmxsml.so"))
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mxsml.c"))
            .output()
            .expect("MXSML fixture requires cc");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn hgml(&self, flags: &[&str]) {
        let output = Command::new("cc")
            .args(flags)
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(self.dir.path().join("libhgml.so"))
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hgml.c"))
            .output()
            .expect("HGML fixture requires cc");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn hcu(&self, flags: &[&str]) {
        let output = Command::new("cc")
            .args(flags)
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(self.dir.path().join("libhcu_smi.so"))
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hcu_smi.c"))
            .output()
            .expect("HCU fixture requires cc");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn dcmi(&self) {
        self.dcmi_with_flags(&[]);
    }

    fn dcmi_with_flags(&self, flags: &[&str]) {
        let output = Command::new("cc")
            .args(flags)
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(self.dir.path().join("libdcmi.so"))
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dcmi.c"))
            .output()
            .expect("DCMI fixture requires a C compiler (cc)");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn wait_device_status(&self, status: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let devices: Vec<Value> =
                serde_json::from_slice(&self.run("xinfo", &["--json"]).stdout).unwrap();
            if !devices.is_empty() && devices.iter().all(|device| device["status"] == status) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "wanted {status}, got {devices:?}"
            );
            sleep(Duration::from_millis(30));
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
        let _ = self.command("xlurm").arg("stop").output();
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
    }
}

#[test]
fn large_queue_responses_preserve_history_and_privacy() {
    let mut h = Harness::new("none", 1);
    h.start();
    let id = h.submit(&["-g", "0", "--wrap", "true"]);
    h.wait_state(id, "COMPLETED");
    h.run("xlurm", &["stop"]);
    h.daemon.take().unwrap().wait().unwrap();

    let state_path = h.dir.path().join("state/state.json");
    let mut store: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let template = store["jobs"][0].clone();
    store["jobs"] = (1..=2000)
        .map(|id| {
            let mut job = template.clone();
            job["id"] = id.into();
            job["spec"]["name"] = format!("history-{id}").into();
            job
        })
        .collect::<Vec<_>>()
        .into();
    store["next_id"] = 2000.into();
    fs::write(&state_path, serde_json::to_vec(&store).unwrap()).unwrap();
    h.start();

    // Read the full existing wire response, including its final newline, across
    // many buffer flushes; the CLI still limits display to the latest 100 jobs.
    let mut socket = UnixStream::connect(h.dir.path().join("state/xlurm.sock")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(b"\"Queue\"\n").unwrap();
    let mut response = String::new();
    BufReader::new(socket).read_line(&mut response).unwrap();
    assert!(response.ends_with('\n'));
    let queue: Value = serde_json::from_str(&response).unwrap();
    let jobs = queue["Queue"].as_array().unwrap();
    assert_eq!(jobs.len(), 2000);
    assert_eq!(jobs[0]["id"], 1);
    assert_eq!(jobs[1999]["name"], "history-2000");
    assert!(jobs.iter().all(|job| job.get("spec").is_none()
        && job.get("command").is_none()
        && job.get("env").is_none()));
    let visible: Vec<Value> =
        serde_json::from_slice(&h.run("xqueue", &["--all", "--json"]).stdout).unwrap();
    assert_eq!(visible.len(), 100);
    assert_eq!(visible[0]["id"], 1901);
    assert_eq!(visible[99]["id"], 2000);
    assert_eq!(h.run("xqueue", &["--json"]).stdout, b"[]\n");
    let next = h.submit(&["-g", "0", "--wrap", "true"]);
    assert_eq!(next, 2001);
    h.wait_state(next, "COMPLETED");
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
        fs::read_to_string(h.dir.path().join("dcmi-queries"))
            .unwrap()
            .contains("process 3")
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
fn ascend_dcmi_queries_fail_closed_and_recover() {
    let mut h = Harness::new("ascend", 2);
    h.drivers();
    fs::write(h.dir.path().join("npu-busy"), "").unwrap();
    h.start();
    h.wait_device_status("busy");
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices[0]["device"]["id"], 2);
    assert_eq!(devices[1]["device"]["visible"], "3");
    assert!(devices[0]["device"]["npu"].is_null());
    assert!(devices[1]["device"]["chip"].is_null());
    assert_eq!(devices[0]["device"]["name"], "Ascend950PR");

    let job = h.submit(&[
        "-i",
        "2",
        "--wrap",
        "while [ ! -f release ]; do sleep 0.05; done",
    ]);
    assert_eq!(h.job(job)["state"], "PENDING");
    for marker in [
        "npu-health-error",
        "probe-error",
        "dcmi-invalid-process-count",
        "dcmi-missing-device",
    ] {
        fs::write(h.dir.path().join(marker), "").unwrap();
        h.wait_device_status("unknown");
        assert_eq!(h.job(job)["state"], "PENDING");
        fs::remove_file(h.dir.path().join(marker)).unwrap();
        h.wait_device_status("busy");
    }
    fs::write(h.dir.path().join("npu-health"), "Alarm").unwrap();
    h.wait_device_status("unhealthy");
    assert_eq!(h.job(job)["state"], "PENDING");
    fs::remove_file(h.dir.path().join("npu-health")).unwrap();
    fs::remove_file(h.dir.path().join("npu-busy")).unwrap();
    let running = h.wait_state(job, "RUNNING");
    assert_eq!(running["devices"][0]["visible"], "2");
    let pending = h.submit(&["-i", "2", "--wrap", "true"]);
    assert_eq!(h.job(pending)["state"], "PENDING");
    fs::write(h.dir.path().join("release"), "").unwrap();
    h.wait_state(job, "COMPLETED");
    h.wait_state(pending, "COMPLETED");
    let queries = fs::read_to_string(h.dir.path().join("dcmi-queries")).unwrap();
    assert_eq!(
        queries
            .lines()
            .filter(|line| line.starts_with("init "))
            .count(),
        1
    );
    assert!(queries.contains("process 2") && queries.contains("process 3"));
    assert!(
        queries
            .lines()
            .filter(|line| !line.starts_with("init "))
            .all(|line| line.ends_with(" 2") || line.ends_with(" 3"))
    );
}

#[test]
fn ascend_dcmi_startup_errors_fail_closed() {
    let h = Harness::new("ascend", 1);
    h.drivers();
    for marker in [
        "dcmi-init-error",
        "dcmi-device-count-error",
        "dcmi-invalid-id",
        "dcmi-chip-info-error",
        "dcmi-duplicate-id",
    ] {
        fs::write(h.dir.path().join(marker), "").unwrap();
        let output = h
            .command("xlurm")
            .args(["daemon", "--backend", "ascend"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!h.dir.path().join("state/xlurm.sock").exists());
        fs::remove_file(h.dir.path().join(marker)).unwrap();
    }
    fs::remove_file(h.dir.path().join("libdcmi.so")).unwrap();
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "ascend"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot load DCMI"));
    h.dcmi_with_flags(&["-DOMIT_PROCESS_API"]);
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "ascend"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing dcmiv2_get_device_resource_info")
    );
    assert!(!h.dir.path().join("state/xlurm.sock").exists());
}

#[test]
fn ascend_dcmi_health_is_per_chip_and_skips_unhealthy_process_queries() {
    let mut h = Harness::new("ascend", 2);
    h.drivers();
    fs::write(h.dir.path().join("npu-chip0-unhealthy"), "").unwrap();
    h.start();
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices[0]["status"], "unhealthy");
    assert_eq!(devices[1]["status"], "idle");
    let queries = fs::read_to_string(h.dir.path().join("dcmi-queries")).unwrap();
    assert!(!queries.contains("process 2"));
    assert!(queries.contains("process 3"));
    let blocked = h.submit(&["-i", "2", "--wrap", "true"]);
    h.wait_state(h.submit(&["-i", "3", "--wrap", "true"]), "COMPLETED");
    assert_eq!(h.job(blocked)["state"], "PENDING");
    fs::remove_file(h.dir.path().join("npu-chip0-unhealthy")).unwrap();
    h.wait_state(blocked, "COMPLETED");
}

#[test]
fn ascend_alarm_and_critical_devices_wait_until_health_is_ok() {
    for health in ["Alarm", "Critical"] {
        let mut h = Harness::new("ascend", 1);
        h.drivers();
        fs::write(h.dir.path().join("npu-health"), health).unwrap();
        h.start();

        let devices: Vec<Value> =
            serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
        assert_eq!(devices.len(), 2);
        assert!(devices.iter().all(|device| device["status"] == "unhealthy"));

        let job = h.submit(&["--device", "ascend", "-i", "2", "--wrap", "true"]);
        assert_eq!(h.job(job)["state"], "PENDING");

        fs::remove_file(h.dir.path().join("npu-health")).unwrap();
        let completed = h.wait_state(job, "COMPLETED");
        assert_eq!(completed["devices"][0]["visible"], "2");
    }
}

#[test]
fn ascend_health_query_failure_is_unknown_and_waits() {
    let mut h = Harness::new("ascend", 1);
    h.drivers();
    fs::write(h.dir.path().join("npu-health-error"), "").unwrap();
    fs::write(h.dir.path().join("npu-busy"), "").unwrap();
    h.start();

    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert!(devices.iter().all(|device| device["status"] == "unknown"));
    let job = h.submit(&["--device", "ascend", "-i", "2", "--wrap", "true"]);
    assert_eq!(h.job(job)["state"], "PENDING");

    fs::remove_file(h.dir.path().join("npu-health-error")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let devices: Vec<Value> =
            serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
        if devices.iter().all(|device| device["status"] == "busy") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "health recovery was not observed: {devices:?}"
        );
        sleep(Duration::from_millis(50));
    }

    fs::remove_file(h.dir.path().join("npu-busy")).unwrap();
    assert_eq!(h.wait_state(job, "COMPLETED")["state"], "COMPLETED");
}

#[test]
fn explicit_device_ids_select_exact_devices_and_validate_counts() {
    let mut h = Harness::new("auto", 2);
    h.drivers();
    h.start();

    let output = h.run(
        "xrun",
        &[
            "-i",
            "3,2",
            "-g",
            "2",
            "--",
            "sh",
            "-c",
            "printf '%s|%s' \"$ASCEND_RT_VISIBLE_DEVICES\" \"$ASCEND_DEVICE_ID\"",
        ],
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "3,2|0");

    let batch = h.run(
        "xbatch",
        &[
            "-i",
            "2,3",
            "-g",
            "2",
            "--wrap",
            "printf '%s' \"$ASCEND_RT_VISIBLE_DEVICES\"",
        ],
    );
    let batch_id = String::from_utf8(batch.stdout)
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    h.wait_state(batch_id, "COMPLETED");
    assert_eq!(h.log(batch_id), "2,3");

    let mismatch = h
        .command("xrun")
        .args(["-i", "2,3", "-g", "1", "--", "true"])
        .output()
        .unwrap();
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("must match"));

    let duplicate = h
        .command("xrun")
        .args(["-i", "2,2", "--", "true"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("duplicates"));
}

#[test]
fn excluded_device_ids_are_skipped_during_automatic_allocation() {
    let mut h = Harness::new("ascend", 1);
    h.drivers();
    h.start();

    let output = h.run(
        "xrun",
        &[
            "-x",
            "2",
            "-g",
            "1",
            "--",
            "sh",
            "-c",
            "printf '%s|%s' \"$ASCEND_RT_VISIBLE_DEVICES\" \"$ASCEND_DEVICE_ID\"",
        ],
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "3|0");

    let conflict = h
        .command("xrun")
        .args(["-x", "2", "-i", "3", "--", "true"])
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be combined"));
}

#[test]
fn scheduler_exclusions_block_exact_requests_and_survive_refresh() {
    let mut h = Harness::new("ascend", 1);
    h.drivers();
    h.start_with_args(env!("CARGO_BIN_EXE_xlurm"), &["--exclude-device-ids", "2"]);
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices[0]["device"]["id"], 2);
    assert_eq!(devices[0]["status"], "disabled");

    let output = h.run(
        "xrun",
        &["sh", "-c", "printf '%s' \"$ASCEND_RT_VISIBLE_DEVICES\""],
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "3");
    for args in [
        vec!["-i", "2", "--wrap", "true"],
        vec!["-g", "2", "--wrap", "true"],
    ] {
        let rejected = h.command("xbatch").args(args).output().unwrap();
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("disable")
                || String::from_utf8_lossy(&rejected.stderr).contains("enabled")
        );
    }

    // Persisted pending jobs must also respect exclusions after a daemon restart.
    let running = h.submit(&[
        "-i",
        "3",
        "--wrap",
        "while [ ! -f release ]; do sleep 0.05; done",
    ]);
    h.wait_state(running, "RUNNING");
    let pending = h.submit(&["-i", "3", "--wrap", "echo resumed"]);
    assert_eq!(h.job(pending)["state"], "PENDING");
    h.run("xlurm", &["stop"]);
    h.daemon.take().unwrap().wait().unwrap();
    h.start_with_args(
        env!("CARGO_BIN_EXE_xlurm"),
        &["--exclude-device-ids", "2,3"],
    );
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices[1]["status"], "disabled");
    assert_eq!(devices[1]["job"], running);
    fs::write(h.dir.path().join("release"), "").unwrap();
    h.wait_state(running, "COMPLETED");
    let cpu = h.submit(&["-g", "0", "--wrap", "true"]);
    h.wait_state(cpu, "COMPLETED");
    // Observe another probe instead of assuming the periodic refresh has run.
    let queries = h.dir.path().join("dcmi-queries");
    let previous_length = fs::metadata(&queries).unwrap().len();
    let deadline = Instant::now() + Duration::from_secs(8);
    while fs::metadata(&queries).unwrap().len() == previous_length {
        assert!(
            Instant::now() < deadline,
            "device monitoring did not refresh"
        );
        sleep(Duration::from_millis(50));
    }
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert!(devices.iter().all(|device| device["status"] == "disabled"));
    assert_eq!(h.job(pending)["state"], "PENDING");
    h.run("xlurm", &["stop"]);
    h.daemon.take().unwrap().wait().unwrap();
    h.start();
    h.wait_state(pending, "COMPLETED");
}

#[test]
fn live_exclusions_are_additive_atomic_and_do_not_interrupt_running_jobs() {
    let mut h = Harness::new("ascend", 2);
    h.drivers();
    h.start_with_args(env!("CARGO_BIN_EXE_xlurm"), &["--exclude-device-ids", "2"]);
    let running = h.submit(&[
        "-i",
        "3",
        "--wrap",
        "while [ ! -f release ]; do sleep 0.05; done",
    ]);
    h.wait_state(running, "RUNNING");
    let pending = h.submit(&["-i", "3", "--wrap", "echo resumed"]);
    assert_eq!(h.job(pending)["state"], "PENDING");

    h.run("xlurm", &["exclude", "3"]);
    h.run("xlurm", &["exclude", "3"]); // Repeating an operation is harmless.
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert!(devices.iter().all(|device| device["status"] == "disabled"));
    assert_eq!(devices[1]["job"], running);
    assert_eq!(h.job(running)["state"], "RUNNING");
    for ids in ["2", "3"] {
        assert!(
            !h.command("xbatch")
                .args(["-i", ids, "--wrap", "true"])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fs::write(h.dir.path().join("release"), "").unwrap();
    h.wait_state(running, "COMPLETED");
    let cpu = h.submit(&["-g", "0", "--wrap", "true"]);
    h.wait_state(cpu, "COMPLETED");
    assert_eq!(h.job(pending)["state"], "PENDING");

    // Invalid lists must not partially re-enable or exclude valid IDs.
    for action in ["exclude", "include"] {
        for ids in ["2,99", "2,2"] {
            assert!(
                !h.command("xlurm")
                    .args([action, ids])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
    }
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert!(devices.iter().all(|device| device["status"] == "disabled"));
    h.run("xlurm", &["include", "3"]);
    h.wait_state(pending, "COMPLETED");
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices[0]["status"], "disabled");
    assert_ne!(devices[1]["status"], "disabled");
    let invalid = h
        .command("xlurm")
        .args(["exclude", "3,99"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    h.run("xrun", &["-i", "3", "true"]);
    h.run("xlurm", &["include", "2,3"]);
    h.run("xlurm", &["include", "2,3"]);
    h.run("xrun", &["-i", "2,3", "true"]);
    h.run("xlurm", &["exclude", "2,3"]);
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert!(devices.iter().all(|device| device["status"] == "disabled"));
}

#[test]
fn background_start_and_restart_apply_temporary_device_exclusions() {
    let h = Harness::new("ascend", 1);
    h.drivers();
    for ids in ["99", "2,2"] {
        let invalid = h
            .command("xlurm")
            .args(["daemon", "--backend", "ascend", "--exclude-device-ids", ids])
            .output()
            .unwrap();
        assert!(!invalid.status.success());
        let error = String::from_utf8_lossy(&invalid.stderr);
        assert!(error.contains("does not exist") || error.contains("duplicates"));
    }
    h.run(
        "xlurm",
        &[
            "start",
            "--backend",
            "ascend",
            "--exclude-device-ids",
            "2",
            "--exclude-device-ids",
            "3",
        ],
    );
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert!(devices.iter().all(|device| device["status"] == "disabled"));
    h.run(
        "xlurm",
        &[
            "restart",
            "--backend",
            "ascend",
            "--exclude-device-ids",
            "3",
        ],
    );
    let output = h.run(
        "xrun",
        &["sh", "-c", "printf '%s' \"$ASCEND_RT_VISIBLE_DEVICES\""],
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "2");
    h.run("xlurm", &["restart", "--backend", "ascend"]);
    h.run("xrun", &["-i", "2,3", "true"]);
}

#[test]
fn metax_mxsml_discovers_and_monitors_without_cli() {
    let mut h = Harness::new("metax", 2);
    h.mxsml(&[]);
    fs::write(h.dir.path().join("mx-busy"), "").unwrap();
    h.start();
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0]["device"]["kind"], "metax");
    assert_eq!(devices[0]["device"]["visible"], "GPU-metax-0");
    assert_eq!(devices[0]["device"]["name"], "MXC550");
    assert_eq!(devices[0]["status"], "idle");
    assert_eq!(devices[1]["status"], "busy");
    let pending = h.submit(&["--device", "metax", "-i", "1", "--wrap", "true"]);
    assert_eq!(h.job(pending)["state"], "PENDING");
    fs::remove_file(h.dir.path().join("mx-busy")).unwrap();
    h.wait_state(pending, "COMPLETED");
    for (ids, expected) in [("1", "1|1"), ("1,0", "1,0|1,0")] {
        let run = h.run(
            "xrun",
            &[
                "--device",
                "metax",
                "-i",
                ids,
                "sh",
                "-c",
                "printf '%s|%s' \"$MACA_VISIBLE_DEVICES\" \"$CUDA_VISIBLE_DEVICES\"",
            ],
        );
        assert_eq!(String::from_utf8(run.stdout).unwrap(), expected);
    }
    assert!(!h.dir.path().join("bin/mx-smi").exists());
}

#[test]
fn metax_mxsml_failures_are_per_device_and_recover() {
    let mut h = Harness::new("metax", 2);
    h.mxsml(&[]);
    h.start();
    for marker in [
        "mx-handle-error",
        "mx-null-handle",
        "mx-uuid-error",
        "mx-uuid-changed",
        "mx-availability-error",
        "mx-missing-availability",
        "mx-probe-error",
        "mx-insufficient-capacity",
        "mx-invalid-process-count",
        "mx-invalid-pid",
        "mx-unhealthy",
    ] {
        if marker == "mx-invalid-pid" {
            fs::write(h.dir.path().join("mx-busy"), "").unwrap();
        }
        fs::write(h.dir.path().join(marker), "").unwrap();
        let expected = if marker == "mx-unhealthy" {
            "unhealthy"
        } else {
            "unknown"
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let devices: Vec<Value> =
                serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
            if devices[1]["status"] == expected {
                assert_eq!(devices[0]["status"], "idle");
                break;
            }
            assert!(Instant::now() < deadline, "{marker}: {devices:?}");
            sleep(Duration::from_millis(30));
        }
        let pending = h.submit(&["--device", "metax", "-i", "1", "--wrap", "true"]);
        assert_eq!(h.job(pending)["state"], "PENDING");
        h.wait_state(
            h.submit(&["--device", "metax", "-i", "0", "--wrap", "true"]),
            "COMPLETED",
        );
        fs::remove_file(h.dir.path().join(marker)).unwrap();
        if marker == "mx-invalid-pid" {
            fs::remove_file(h.dir.path().join("mx-busy")).unwrap();
        }
        h.wait_state(pending, "COMPLETED");
    }
}

#[test]
fn metax_mxsml_unhealthy_devices_skip_process_queries() {
    let mut h = Harness::new("metax", 1);
    h.mxsml(&[]);
    fs::write(h.dir.path().join("mx-unhealthy"), "").unwrap();
    h.start();
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices[1]["status"], "unhealthy");
    let queries = fs::read_to_string(h.dir.path().join("mxsml-queries")).unwrap();
    assert!(queries.contains("availability 1"));
    assert!(!queries.contains("process 1"));
}

#[test]
fn metax_mxsml_startup_errors_fail_closed() {
    let h = Harness::new("metax", 1);
    h.mxsml(&[]);
    for marker in [
        "mx-init-error",
        "mx-count-error",
        "mx-invalid-count",
        "mx-zero-count",
        "mx-handle-error",
        "mx-null-handle",
        "mx-uuid-error",
        "mx-invalid-uuid",
        "mx-duplicate-id",
        "mx-name-error",
        "mx-unterminated-name",
        "mx-invalid-utf8",
    ] {
        fs::write(h.dir.path().join(marker), "").unwrap();
        let output = h
            .command("xlurm")
            .args(["daemon", "--backend", "metax"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{marker}");
        assert!(!h.dir.path().join("state/xlurm.sock").exists());
        fs::remove_file(h.dir.path().join(marker)).unwrap();
    }
    h.mxsml(&["-DOMIT_PROCESS_API"]);
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "metax"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("missing mxSmlExDeviceGetComputeRunningProcesses")
    );
    fs::remove_file(h.dir.path().join("libmxsml.so")).unwrap();
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "metax"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot load MetaX MXSML"));
}

#[test]
fn ppu_hgml_discovers_and_monitors_without_cli() {
    let mut h = Harness::new("ppu", 2);
    h.hgml(&[]);
    fs::write(h.dir.path().join("ppu-busy"), "").unwrap();
    h.start();
    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0]["device"]["kind"], "ppu");
    assert_eq!(devices[0]["device"]["visible"], "GPU-ppu-0");
    assert_eq!(devices[0]["device"]["name"], "PPU-ZW810E");
    assert_eq!(devices[0]["status"], "idle");
    assert_eq!(devices[1]["status"], "busy");
    let pending = h.submit(&["--device", "ppu", "-i", "1", "--wrap", "true"]);
    assert_eq!(h.job(pending)["state"], "PENDING");
    fs::remove_file(h.dir.path().join("ppu-busy")).unwrap();
    h.wait_state(pending, "COMPLETED");
    for (ids, expected) in [("1", "GPU-ppu-1"), ("1,0", "GPU-ppu-1,GPU-ppu-0")] {
        let run = h.run(
            "xrun",
            &[
                "--device",
                "ppu",
                "-i",
                ids,
                "sh",
                "-c",
                "printf '%s' \"$CUDA_VISIBLE_DEVICES\"",
            ],
        );
        assert_eq!(String::from_utf8(run.stdout).unwrap(), expected);
    }
    assert!(!h.dir.path().join("bin/ppu-smi").exists());
    // Both vendor UUID prefixes remain valid for runtime visibility.
    let mut daemon = h.daemon.take().unwrap();
    unsafe {
        libc::kill(daemon.id() as i32, libc::SIGTERM);
    }
    daemon.wait().unwrap();
    fs::write(h.dir.path().join("ppu-prefix"), "").unwrap();
    h.start();
    let run = h.run(
        "xrun",
        &[
            "--device",
            "ppu",
            "-i",
            "1",
            "sh",
            "-c",
            "printf '%s' \"$CUDA_VISIBLE_DEVICES\"",
        ],
    );
    assert_eq!(String::from_utf8(run.stdout).unwrap(), "PPU-ppu-1");
}

#[test]
fn ppu_hgml_failures_are_per_device_and_recover() {
    let mut h = Harness::new("ppu", 2);
    h.hgml(&[]);
    h.start();
    for marker in [
        "ppu-handle-error",
        "ppu-null-handle",
        "ppu-uuid-error",
        "ppu-uuid-changed",
        "ppu-probe-error",
        "ppu-insufficient-capacity",
        "ppu-invalid-process-count",
        "ppu-invalid-pid",
    ] {
        if marker == "ppu-invalid-pid" {
            fs::write(h.dir.path().join("ppu-busy"), "").unwrap();
        }
        fs::write(h.dir.path().join(marker), "").unwrap();
        let expected = "unknown";
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let devices: Vec<Value> =
                serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
            if devices[1]["status"] == expected {
                assert_eq!(devices[0]["status"], "idle");
                break;
            }
            assert!(Instant::now() < deadline, "{marker}: {devices:?}");
            sleep(Duration::from_millis(30));
        }
        let pending = h.submit(&["--device", "ppu", "-i", "1", "--wrap", "true"]);
        assert_eq!(h.job(pending)["state"], "PENDING");
        h.wait_state(
            h.submit(&["--device", "ppu", "-i", "0", "--wrap", "true"]),
            "COMPLETED",
        );
        fs::remove_file(h.dir.path().join(marker)).unwrap();
        if marker == "ppu-invalid-pid" {
            fs::remove_file(h.dir.path().join("ppu-busy")).unwrap();
        }
        h.wait_state(pending, "COMPLETED");
    }
}

#[test]
fn ppu_hgml_startup_errors_fail_closed() {
    let h = Harness::new("ppu", 1);
    h.hgml(&[]);
    for marker in [
        "ppu-init-error",
        "ppu-count-error",
        "ppu-invalid-count",
        "ppu-zero-count",
        "ppu-handle-error",
        "ppu-null-handle",
        "ppu-uuid-error",
        "ppu-invalid-uuid",
        "ppu-duplicate-id",
        "ppu-name-error",
        "ppu-unterminated-name",
        "ppu-invalid-utf8",
    ] {
        fs::write(h.dir.path().join(marker), "").unwrap();
        let output = h
            .command("xlurm")
            .args(["daemon", "--backend", "ppu"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{marker}");
        assert!(!h.dir.path().join("state/xlurm.sock").exists());
        fs::remove_file(h.dir.path().join(marker)).unwrap();
    }
    h.hgml(&["-DOMIT_PROCESS_API"]);
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "ppu"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("missing hgmlDeviceGetComputeRunningProcesses_v2")
    );
    fs::remove_file(h.dir.path().join("libhgml.so")).unwrap();
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "ppu"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot load PPU HGML"));
}

#[test]
fn hcu_devices_are_discovered_monitored_and_exposed_by_logical_id() {
    let mut h = Harness::new("hcu", 1);
    h.drivers();
    h.hcu(&[]);
    fs::write(h.dir.path().join("hcu-busy"), "").unwrap();
    h.start();

    let devices: Vec<Value> = serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0]["device"]["visible"], "0000000000000001");
    assert_eq!(devices[0]["status"], "idle");
    assert_eq!(devices[1]["status"], "busy");

    let run = h.run(
        "xrun",
        &[
            "--device", "hcu", "-i", "0", "--", "sh", "-c",
            "printf '%s|%s|%s' \"$HIP_VISIBLE_DEVICES\" \"$ROCR_VISIBLE_DEVICES\" \"$CUDA_VISIBLE_DEVICES\"",
        ],
    );
    assert_eq!(String::from_utf8(run.stdout).unwrap(), "0|0|0");

    fs::remove_file(h.dir.path().join("hcu-busy")).unwrap();
    let run = h.run("xrun", &["--device", "hcu", "-i", "1", "sh", "-c",
        "printf '%s|%s|%s' \"$HIP_VISIBLE_DEVICES\" \"$ROCR_VISIBLE_DEVICES\" \"$CUDA_VISIBLE_DEVICES\""]);
    assert_eq!(String::from_utf8(run.stdout).unwrap(), "0|1|0");
    let run = h.run("xrun", &["--device", "hcu", "-i", "1,0", "sh", "-c",
        "printf '%s|%s|%s' \"$HIP_VISIBLE_DEVICES\" \"$ROCR_VISIBLE_DEVICES\" \"$CUDA_VISIBLE_DEVICES\""]);
    assert_eq!(String::from_utf8(run.stdout).unwrap(), "0,1|1,0|0,1");

    fs::write(h.dir.path().join("hcu-unhealthy"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let devices: Vec<Value> =
            serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
        if devices[1]["status"] == "unhealthy" {
            break;
        }
        assert!(Instant::now() < deadline, "unhealthy HCU not observed");
        sleep(Duration::from_millis(50));
    }
    let waiting = h.submit(&["--device", "hcu", "-i", "1", "--wrap", "true"]);
    assert_eq!(h.job(waiting)["state"], "PENDING");

    fs::remove_file(h.dir.path().join("hcu-unhealthy")).unwrap();
    fs::write(h.dir.path().join("hcu-health-error"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let devices: Vec<Value> =
            serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
        if devices.iter().all(|device| device["status"] == "unknown") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "health probe failure not observed: {devices:?}"
        );
        sleep(Duration::from_millis(50));
    }
    assert_eq!(h.job(waiting)["state"], "PENDING");
}

#[test]
fn hcu_smi_failures_block_allocation_and_recover() {
    let mut h = Harness::new("hcu", 1);
    h.hcu(&[]);
    fs::write(h.dir.path().join("hcu-busy"), "").unwrap();
    h.start();
    for marker in [
        "hcu-probe-error",
        "hcu-invalid-process-count",
        "hcu-process-gpus-error",
        "hcu-invalid-gpu-count",
        "hcu-invalid-gpu-id",
        "hcu-count-error",
    ] {
        fs::write(h.dir.path().join(marker), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let devices: Vec<Value> =
                serde_json::from_slice(&h.run("xinfo", &["--json"]).stdout).unwrap();
            if devices.iter().all(|d| d["status"] == "unknown") {
                break;
            }
            assert!(Instant::now() < deadline, "{marker}: {devices:?}");
            sleep(Duration::from_millis(50));
        }
        let id = h.submit(&["--device", "hcu", "-i", "0", "--wrap", "true"]);
        assert_eq!(h.job(id)["state"], "PENDING");
        fs::remove_file(h.dir.path().join(marker)).unwrap();
        h.wait_state(id, "COMPLETED");
    }
}

#[test]
fn hcu_smi_startup_errors_fail_closed() {
    let h = Harness::new("hcu", 1);
    h.hcu(&[]);
    for marker in [
        "hcu-init-error",
        "hcu-count-error",
        "hcu-discovery-error",
        "hcu-duplicate-id",
    ] {
        fs::write(h.dir.path().join(marker), "").unwrap();
        let output = h
            .command("xlurm")
            .args(["daemon", "--backend", "hcu"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{marker}");
        assert!(!h.dir.path().join("state/xlurm.sock").exists());
        fs::remove_file(h.dir.path().join(marker)).unwrap();
    }
    h.hcu(&["-DOMIT_PROCESS_API"]);
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "hcu"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing rsmi_compute_process_info_get")
    );
    fs::remove_file(h.dir.path().join("libhcu_smi.so")).unwrap();
    let output = h
        .command("xlurm")
        .args(["daemon", "--backend", "hcu"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot load HCU SMI"));
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
