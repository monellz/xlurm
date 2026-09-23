use crate::daemon::request;
use crate::device::Backend;
use crate::model::*;
use crate::storage::{Paths, read_json, try_lock};
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_QUEUE_LIMIT: usize = 100;

#[derive(Parser)]
#[command(
    name = "xlurm",
    version,
    propagate_version = true,
    about = "Minimal multi-user, single-machine GPU / Ascend scheduler"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Start the local scheduler in the background.
    Start(DaemonArgs),
    /// Run the scheduler in the foreground (for systemd or debugging).
    Daemon(DaemonArgs),
    /// Stop scheduling; running jobs survive and are adopted on restart.
    Stop,
    /// Remove all logs while the scheduler is stopped and no job is running.
    Clean,
    /// Run a command, stream its log, and return its exit code.
    #[command(alias = "xrun")]
    Run(RunArgs),
    /// Submit a bash script (or --wrap command) and print the job ID.
    #[command(alias = "xbatch")]
    Batch(BatchArgs),
    /// Show jobs, inspect a job, or cancel a job.
    #[command(alias = "xqueue")]
    Queue(QueueArgs),
    /// Cancel a pending or running job (including its process group).
    #[command(alias = "xcancel")]
    Cancel(CancelArgs),
    /// Show device availability.
    #[command(alias = "xinfo")]
    Info(InfoArgs),
}

#[derive(Args)]
struct DaemonArgs {
    #[arg(long, value_enum, default_value = "auto")]
    backend: Backend,
    /// Maximum concurrent jobs, including CPU-only jobs.
    #[arg(long, default_value_t = 32)]
    max_running: usize,
}

#[derive(Args)]
struct Resources {
    /// Number of exclusive accelerator devices; 0 runs a CPU-only job.
    #[arg(short = 'g', long, visible_alias = "devices", default_value_t = 1)]
    gpus: usize,
    /// Restrict the vendor; otherwise use the first pool that fits.
    #[arg(long, value_enum)]
    device: Option<Kind>,
    #[arg(short = 'n', long)]
    name: Option<String>,
    /// Wall-clock limit in seconds, measured from process launch.
    #[arg(short = 't', long)]
    time_limit: Option<u64>,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    resources: Resources,
    /// Command and its arguments; put scheduler options before the command.
    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

#[derive(Args)]
struct BatchArgs {
    #[command(flatten)]
    resources: Resources,
    /// Execute this command using bash -c instead of a script.
    #[arg(long, conflicts_with_all = ["script", "args"], required_unless_present = "script")]
    wrap: Option<String>,
    /// Bash script, copied into the job at submission time.
    #[arg(required_unless_present = "wrap")]
    script: Option<PathBuf>,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct QueueArgs {
    /// Include finished jobs.
    #[arg(short, long)]
    all: bool,
    /// Inspect an owned job, including its owner and exit result.
    #[arg(value_name = "JOB_ID", conflicts_with = "cancel")]
    id: Option<u64>,
    /// Cancel a pending or running job (including its process group).
    #[arg(long)]
    cancel: Option<u64>,
    /// Read this job's output through the authenticated local socket.
    #[arg(long, requires = "id")]
    log: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CancelArgs {
    #[arg(value_name = "JOB_ID")]
    id: u64,
}

#[derive(Args)]
struct InfoArgs {
    #[arg(long)]
    json: bool,
}

pub fn run(wrapper: Option<&str>) -> Result<i32> {
    let original: Vec<OsString> = std::env::args_os().collect();
    if original.get(1).is_some_and(|arg| arg == "__worker") {
        ensure!(original.len() == 4, "invalid worker arguments");
        let id = original[2].to_str().context("invalid job ID")?.parse()?;
        let fd = original[3]
            .to_str()
            .context("invalid descriptor")?
            .parse()?;
        crate::executor::worker(&Paths::discover()?, id, fd)?;
        return Ok(0);
    }
    let mut argv = original;
    let cli = if argv.get(1).is_some_and(|arg| arg == "__daemon") {
        argv[1] = "daemon".into();
        Cli::parse_from(argv)
    } else if let Some(wrapper) = wrapper {
        let name = match wrapper {
            "run" => "xrun",
            "batch" => "xbatch",
            "queue" => "xqueue",
            "cancel" => "xcancel",
            "info" => "xinfo",
            _ => bail!("unknown command wrapper"),
        };
        let parser = Cli::command()
            .find_subcommand(wrapper)
            .unwrap()
            .clone()
            .name(name)
            .bin_name(name)
            .version(env!("CARGO_PKG_VERSION"));
        let matches = parser.get_matches_from(argv);
        let command = match wrapper {
            "run" => Action::Run(RunArgs::from_arg_matches(&matches)?),
            "batch" => Action::Batch(BatchArgs::from_arg_matches(&matches)?),
            "queue" => Action::Queue(QueueArgs::from_arg_matches(&matches)?),
            "cancel" => Action::Cancel(CancelArgs::from_arg_matches(&matches)?),
            "info" => Action::Info(InfoArgs::from_arg_matches(&matches)?),
            _ => unreachable!(),
        };
        Cli { command }
    } else {
        Cli::parse_from(argv)
    };
    let paths = Paths::discover()?;
    match cli.command {
        Action::Start(args) => start(&paths, args)?,
        Action::Daemon(args) => crate::daemon::serve(paths, args.backend, args.max_running)?,
        Action::Stop => {
            request(&paths, &Request::Stop)?;
            println!("Scheduler stopped; running jobs continue.");
        }
        Action::Clean => clean(&paths)?,
        Action::Run(args) => {
            crate::install_signals()?;
            let spec = submission(args.resources, args.command, None)?;
            let job = submit(&paths, spec)?;
            return follow(&paths, job.id);
        }
        Action::Batch(args) => {
            let (command, script, default_name) = if let Some(wrap) = args.wrap {
                (vec!["bash".into(), "-c".into(), wrap], None, None)
            } else {
                let path = args.script.context("script is required")?;
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                (
                    args.args,
                    Some(text),
                    path.file_name().map(|n| n.to_string_lossy().into_owned()),
                )
            };
            let mut resources = args.resources;
            if resources.name.is_none() {
                resources.name = default_name;
            }
            let job = submit(&paths, submission(resources, command, script)?)?;
            println!("{}", job.id);
        }
        Action::Queue(args) => queue(&paths, args)?,
        Action::Cancel(args) => cancel(&paths, args.id)?,
        Action::Info(args) => {
            let Response::Info(devices) = request(&paths, &Request::Info)? else {
                bail!("unexpected response");
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else {
                println!("DEVICE       STATUS      JOB    NAME");
                for view in devices {
                    println!(
                        "{:<12} {:<11} {:<6} {}",
                        format!("{}:{}", view.device.kind, view.device.id),
                        view.status,
                        view.job
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "-".into()),
                        view.device.name
                    );
                }
            }
        }
    }
    Ok(0)
}

fn clean(paths: &Paths) -> Result<()> {
    paths.initialize()?;
    let _lock = try_lock(&paths.0.join("daemon.lock"))?
        .context("xlurm is running; stop the scheduler before cleaning logs")?;
    let state_path = paths.0.join("state.json");
    let store = if state_path.exists() {
        read_json::<Store>(&state_path)?
    } else {
        Store::default()
    };
    let running: Vec<_> = store
        .jobs
        .iter()
        .filter(|job| job.state == State::Running)
        .map(|job| job.id.to_string())
        .collect();
    ensure!(
        running.is_empty(),
        "cannot clean logs while jobs are running: {}",
        running.join(", ")
    );

    let mut job_logs = 0;
    let jobs_path = paths.0.join("jobs");
    for entry in
        fs::read_dir(&jobs_path).with_context(|| format!("cannot read {}", jobs_path.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let is_job_log = name
            .to_str()
            .and_then(|name| name.split_once('.'))
            .is_some_and(|(id, extension)| id.parse::<u64>().is_ok() && extension == "log");
        if is_job_log {
            match fs::remove_file(entry.path()) {
                Ok(()) => job_logs += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot remove {}", entry.path().display()));
                }
            }
        }
    }
    let daemon_log_path = paths.0.join("daemon.log");
    let daemon_log = match fs::remove_file(&daemon_log_path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot remove {}", daemon_log_path.display()));
        }
    };
    println!(
        "Removed {job_logs} job log(s){}.",
        if daemon_log {
            " and daemon.log"
        } else {
            "; daemon.log was already absent"
        }
    );
    Ok(())
}

fn submission(
    resources: Resources,
    command: Vec<String>,
    script: Option<String>,
) -> Result<Submission> {
    ensure!(
        resources.time_limit != Some(0),
        "time-limit must be positive"
    );
    Ok(Submission {
        name: resources
            .name
            .unwrap_or_else(|| command.first().cloned().unwrap_or_else(|| "batch".into())),
        command,
        cwd: std::env::current_dir()?,
        env: std::env::vars().collect(),
        count: resources.gpus,
        kind: resources.device,
        time_limit: resources.time_limit,
        script,
    })
}

fn submit(paths: &Paths, spec: Submission) -> Result<Job> {
    let Response::Job(job) = request(paths, &Request::Submit(spec))? else {
        bail!("unexpected response");
    };
    Ok(*job)
}

fn start(paths: &Paths, args: DaemonArgs) -> Result<()> {
    if request(paths, &Request::Info).is_ok() {
        println!("xlurm is already running");
        return Ok(());
    }
    ensure!(args.max_running > 0, "max-running must be positive");
    paths.initialize()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.0.join("daemon.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__daemon")
        .arg("--backend")
        .arg(format!("{:?}", args.backend).to_lowercase())
        .arg("--max-running")
        .arg(args.max_running.to_string())
        .env("XLURM_HOME", &paths.0)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if request(paths, &Request::Info).is_ok() {
            println!("xlurm started");
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "daemon exited ({status}); see {}",
                paths.0.join("daemon.log").display()
            );
        }
        if Instant::now() >= deadline {
            // Never leave an unannounced daemon starting after reporting failure.
            child.kill()?;
            child.wait()?;
            bail!(
                "daemon startup timed out; see {}",
                paths.0.join("daemon.log").display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn follow(paths: &Paths, id: u64) -> Result<i32> {
    let mut offset = 0;
    let mut cancelled = false;
    let waiting_since = Instant::now();
    let mut waiting_reported = false;
    let mut started_reported = false;
    loop {
        if crate::interrupted() && !cancelled {
            request(paths, &Request::Cancel(id))?;
            cancelled = true;
        }
        let Response::Job(job) = request(paths, &Request::Get(id))? else {
            bail!("unexpected response");
        };
        // Every submission starts pending; allow the scheduler to run first.
        if job.state == State::Pending
            && !waiting_reported
            && waiting_since.elapsed() >= Duration::from_secs(1)
        {
            eprintln!("[xlurm] {} Job {id}: Waiting for resources...", timestamp());
            waiting_reported = true;
        }
        // Fast jobs can finish between polls, so check whether they ever started.
        if !started_reported && job.started_at.is_some() {
            eprintln!("[xlurm] {} Job {id}: Task started.", timestamp());
            started_reported = true;
        }
        let mut drained = false;
        for _ in 0..16 {
            if !print_log_chunk(paths, id, &mut offset)? {
                drained = true;
                break;
            }
        }
        if let Some(result) = &job.result
            && drained
        {
            if let Some(error) = &result.error {
                eprintln!("xlurm: {error}");
            }
            return Ok(result.exit_code);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn timestamp() -> String {
    let seconds = now() as libc::time_t;
    let mut time: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::gmtime_r(&seconds, &mut time) }.is_null() {
        return format!("{seconds} Unix");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        time.tm_year + 1900,
        time.tm_mon + 1,
        time.tm_mday,
        time.tm_hour,
        time.tm_min,
        time.tm_sec,
    )
}

fn print_log_chunk(paths: &Paths, id: u64, offset: &mut u64) -> Result<bool> {
    let Response::Log {
        bytes,
        offset: next,
    } = request(
        paths,
        &Request::Log {
            id,
            offset: *offset,
        },
    )?
    else {
        bail!("unexpected log response");
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    *offset = next;
    Ok(!bytes.is_empty())
}

fn cancel(paths: &Paths, id: u64) -> Result<()> {
    request(paths, &Request::Cancel(id))?;
    println!("Cancellation requested for job {id}");
    Ok(())
}

fn queue(paths: &Paths, args: QueueArgs) -> Result<()> {
    if let Some(id) = args.cancel {
        return cancel(paths, id);
    }
    if let Some(id) = args.id {
        let Response::Job(job) = request(paths, &Request::Get(id))? else {
            bail!("unexpected response");
        };
        if args.log {
            let mut offset = 0;
            while print_log_chunk(paths, id, &mut offset)? {}
            return Ok(());
        }
        if args.json {
            println!("{}", serde_json::to_string_pretty(&job)?);
        } else {
            let (wait_time, run_time) = job_times(
                job.submitted_at,
                job.started_at,
                job.result.as_ref().map(|result| result.finished_at),
                now(),
            );
            println!(
                "Job {}: {:?}\nName: {}\nUser: {} (UID {})\nDirectory: {}\nLog: xqueue {} --log\nDevices: {}\nWait time: {}\nRun time: {}",
                job.id,
                job.state,
                job.spec.name,
                job.owner.name,
                job.owner.uid,
                job.spec.cwd.display(),
                id,
                device_names(&job.devices),
                format_duration(wait_time),
                run_time.map_or_else(|| "-".into(), format_duration),
            );
            if let Some(result) = job.result {
                println!("Exit code: {}", result.exit_code);
                if let Some(error) = result.error {
                    println!("Error: {error}");
                }
            }
        }
        return Ok(());
    }
    let Response::Queue(jobs) = request(paths, &Request::Queue)? else {
        bail!("unexpected response");
    };
    let jobs = visible_queue_jobs(jobs, args.all);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
    } else {
        println!(
            "JOB    USER         STATE       COUNT DEVICES          WAIT         RUN          NAME"
        );
        let timestamp = now();
        for job in jobs {
            let (wait_time, run_time) =
                job_times(job.submitted_at, job.started_at, job.finished_at, timestamp);
            println!(
                "{:<6} {:<12} {:<11} {:<5} {:<16} {:<12} {:<12} {}",
                job.id,
                job.owner.name,
                format!("{:?}", job.state).to_uppercase(),
                job.count,
                device_names(&job.devices),
                format_duration(wait_time),
                run_time.map_or_else(|| "-".into(), format_duration),
                job.name.escape_debug()
            );
        }
    }
    Ok(())
}

fn visible_queue_jobs(jobs: Vec<JobSummary>, all: bool) -> Vec<JobSummary> {
    let mut jobs: Vec<_> = jobs
        .into_iter()
        .rev()
        .filter(|job| all || !job.state.terminal())
        .take(DEFAULT_QUEUE_LIMIT)
        .collect();
    jobs.reverse();
    jobs
}

fn job_times(
    submitted_at: u64,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    timestamp: u64,
) -> (u64, Option<u64>) {
    match started_at {
        Some(started_at) => (
            started_at.saturating_sub(submitted_at),
            Some(finished_at.unwrap_or(timestamp).saturating_sub(started_at)),
        ),
        None => (
            finished_at
                .unwrap_or(timestamp)
                .saturating_sub(submitted_at),
            None,
        ),
    }
}

fn format_duration(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}-{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn device_names(devices: &[Device]) -> String {
    if devices.is_empty() {
        "-".into()
    } else {
        devices
            .iter()
            .map(|d| format!("{}:{}", d.kind, d.id))
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::{format_duration, job_times, visible_queue_jobs};
    use crate::model::{JobSummary, Owner, State};

    fn summary(id: u64, state: State) -> JobSummary {
        JobSummary {
            id,
            owner: Owner {
                uid: 1,
                gid: 1,
                name: "user".into(),
            },
            name: format!("job-{id}"),
            state,
            count: 0,
            devices: vec![],
            submitted_at: id,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn job_times_follow_the_job_lifecycle() {
        assert_eq!(job_times(100, None, None, 130), (30, None));
        assert_eq!(job_times(100, Some(110), None, 135), (10, Some(25)));
        assert_eq!(job_times(100, Some(110), Some(150), 999), (10, Some(40)));
        assert_eq!(job_times(100, None, Some(125), 999), (25, None));
    }

    #[test]
    fn durations_use_fixed_clock_fields_and_days() {
        assert_eq!(format_duration(0), "00:00:00");
        assert_eq!(format_duration(3_661), "01:01:01");
        assert_eq!(format_duration(183_845), "2-03:04:05");
    }

    #[test]
    fn queue_keeps_the_latest_hundred_matching_jobs_in_display_order() {
        let jobs = (1..=110).map(|id| summary(id, State::Pending)).collect();
        let visible = visible_queue_jobs(jobs, false);
        assert_eq!(visible.len(), 100);
        assert_eq!(visible.first().unwrap().id, 11);
        assert_eq!(visible.last().unwrap().id, 110);

        let jobs = (1..=202)
            .map(|id| {
                summary(
                    id,
                    if id % 2 == 0 {
                        State::Completed
                    } else {
                        State::Running
                    },
                )
            })
            .collect();
        let active = visible_queue_jobs(jobs, false);
        assert_eq!(active.len(), 100);
        assert_eq!(active.first().unwrap().id, 3);
        assert_eq!(active.last().unwrap().id, 201);

        let history = (1..=110).map(|id| summary(id, State::Completed)).collect();
        let visible = visible_queue_jobs(history, true);
        assert_eq!(visible.len(), 100);
        assert_eq!(visible.first().unwrap().id, 11);
        assert_eq!(visible.last().unwrap().id, 110);
    }
}
