use crate::auth;
use crate::device::{Backend, Inventory};
use crate::executor::ProcessExecutor;
use crate::model::{JobSummary, Request, Response, now};
use crate::scheduler::Scheduler;
use crate::storage::{Paths, try_lock};
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

pub fn request(paths: &Paths, request: &Request) -> Result<Response> {
    let mut socket = UnixStream::connect(paths.socket())
        .context("xlurm is not running; ask the administrator to run `sudo xlurm start`")?;
    let (server_uid, _) = auth::peer(&socket)?;
    ensure!(
        server_uid == fs::metadata(&paths.0)?.uid(),
        "socket peer does not own the state directory"
    );
    socket.set_read_timeout(Some(Duration::from_secs(60)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    send(&mut socket, request)?;
    match receive(&mut socket, 64 * 1024 * 1024)? {
        Response::Error(error) => anyhow::bail!("{error}"),
        response => Ok(response),
    }
}

fn send(socket: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *socket, value)?;
    socket.write_all(b"\n")?;
    Ok(())
}

fn receive<T: DeserializeOwned>(socket: &mut UnixStream, limit: usize) -> Result<T> {
    let mut reader = BufReader::new(socket);
    let mut bytes = Vec::new();
    loop {
        let chunk = match reader.fill_buf() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        ensure!(
            !chunk.is_empty(),
            "connection closed before complete message"
        );
        let end = chunk.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let length = end.unwrap_or(chunk.len());
        ensure!(bytes.len() + length <= limit, "message exceeds size limit");
        bytes.extend_from_slice(&chunk[..length]);
        reader.consume(length);
        if end.is_some() {
            return Ok(serde_json::from_slice(&bytes)?);
        }
    }
}

pub fn serve(paths: Paths, backend: Backend, max_running: usize) -> Result<()> {
    paths.initialize()?;
    let _lock =
        try_lock(&paths.0.join("daemon.lock"))?.context("xlurm daemon is already running")?;
    crate::install_signals()?;
    let inventory = Inventory::discover(backend)?;
    let executor = ProcessExecutor::new(paths.clone());
    let mut scheduler = Scheduler::new(paths.clone(), inventory, executor, max_running)?;
    scheduler.tick()?;
    if let Err(error) = scheduler.cleanup(now()) {
        eprintln!("history cleanup: {error:#}");
    }
    if paths.socket().exists() {
        fs::remove_file(paths.socket())?;
    }
    let listener = UnixListener::bind(paths.socket())
        .context("cannot bind local socket (XLURM_HOME may be too long)")?;
    fs::set_permissions(
        paths.socket(),
        fs::Permissions::from_mode(if unsafe { libc::geteuid() } == 0 {
            0o666
        } else {
            0o600
        }),
    )?;
    listener.set_nonblocking(true)?;
    let _cleanup = SocketCleanup(paths.socket());
    let mut refreshed = Instant::now();
    let mut cleaned = Instant::now();
    while !crate::interrupted() {
        // Bound client work so a stream of submissions cannot starve execution.
        for _ in 0..16 {
            let (mut socket, _) = match listener.accept() {
                Ok(client) => client,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            };
            socket.set_read_timeout(Some(Duration::from_millis(250)))?;
            socket.set_write_timeout(Some(Duration::from_secs(2)))?;
            let (uid, gid) = match auth::peer(&socket) {
                Ok(peer) => peer,
                Err(error) => {
                    let _ = send(&mut socket, &Response::Error(error.to_string()));
                    continue;
                }
            };
            let request: Request = match receive(&mut socket, 4 * 1024 * 1024) {
                Ok(request) => request,
                Err(error) => {
                    let _ = send(&mut socket, &Response::Error(error.to_string()));
                    continue;
                }
            };
            let stop = matches!(request, Request::Stop | Request::Restart);
            let response = handle(&mut scheduler, &paths, uid, gid, request)
                .unwrap_or_else(|error| Response::Error(format!("{error:#}")));
            // A client disconnecting does not roll back an accepted submission.
            let _ = send(&mut socket, &response);
            if stop && matches!(response, Response::Ok) {
                return Ok(());
            }
        }
        if refreshed.elapsed() >= Duration::from_secs(2) {
            scheduler.inventory.refresh();
            refreshed = Instant::now();
        }
        scheduler.tick()?;
        if cleaned.elapsed() >= Duration::from_secs(60) {
            if let Err(error) = scheduler.cleanup(now()) {
                eprintln!("history cleanup: {error:#}");
            }
            cleaned = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn handle(
    scheduler: &mut Scheduler<ProcessExecutor>,
    paths: &Paths,
    uid: u32,
    gid: u32,
    request: Request,
) -> Result<Response> {
    match request {
        Request::Submit(spec) => {
            ensure!(
                unsafe { libc::geteuid() } == 0 || uid == unsafe { libc::geteuid() },
                "only a root daemon can run jobs for other users"
            );
            scheduler
                .submit(spec, auth::account(uid, gid)?)
                .map(|job| Response::Job(Box::new(job)))
        }
        Request::Queue => Ok(Response::Queue(
            scheduler.store.jobs.iter().map(JobSummary::from).collect(),
        )),
        Request::Get(id) => {
            let job = scheduler.get(id)?;
            auth::authorize(uid, &job.owner)?;
            Ok(Response::Job(Box::new(job)))
        }
        Request::Cancel(id) => {
            auth::authorize(uid, &scheduler.get(id)?.owner)?;
            scheduler.cancel(id)?;
            Ok(Response::Ok)
        }
        Request::Log { id, offset } => {
            auth::authorize(uid, &scheduler.get(id)?.owner)?;
            let mut bytes = Vec::new();
            match fs::File::open(paths.job(id, "log")) {
                Ok(mut file) => {
                    file.seek(SeekFrom::Start(offset))?;
                    file.take(64 * 1024).read_to_end(&mut bytes)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            Ok(Response::Log {
                offset: offset + bytes.len() as u64,
                bytes,
            })
        }
        Request::Info => Ok(Response::Info(scheduler.info())),
        Request::Stop | Request::Restart => {
            ensure!(
                uid == 0 || uid == unsafe { libc::geteuid() },
                "only the administrator may stop or restart the scheduler"
            );
            if matches!(request, Request::Restart) {
                let running: Vec<_> = scheduler
                    .store
                    .jobs
                    .iter()
                    .filter(|job| job.state == crate::model::State::Running)
                    .map(|job| job.id.to_string())
                    .collect();
                ensure!(
                    running.is_empty(),
                    "cannot restart while jobs are running: {}",
                    running.join(", ")
                );
            }
            Ok(Response::Ok)
        }
    }
}

struct SocketCleanup(std::path::PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Owner, State, Submission};

    #[test]
    fn api_keeps_details_logs_cancellation_and_shutdown_private() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths(directory.path().join("state"));
        paths.initialize().unwrap();
        let mut scheduler = Scheduler::new(
            paths.clone(),
            Inventory::discover(Backend::None).unwrap(),
            ProcessExecutor::new(paths.clone()),
            1,
        )
        .unwrap();
        let owner = Owner {
            uid: 23456,
            gid: 23456,
            name: "alice".into(),
        };
        let spec = Submission {
            command: vec!["private-command".into()],
            cwd: directory.path().into(),
            env: [("TOKEN".into(), "private-token".into())].into(),
            name: "public-name".into(),
            count: 0,
            kind: None,
            time_limit: None,
            script: None,
        };
        let id = scheduler.submit(spec.clone(), owner.clone()).unwrap().id;
        let other_owner = Owner {
            uid: 23457,
            gid: 23457,
            name: "bob".into(),
        };
        let other_id = scheduler.submit(spec, other_owner.clone()).unwrap().id;
        fs::write(paths.job(id, "log"), b"private-output\x00\xff").unwrap();

        for request in [
            Request::Get(id),
            Request::Log { id, offset: 0 },
            Request::Cancel(id),
            Request::Stop,
            Request::Restart,
        ] {
            assert!(handle(&mut scheduler, &paths, 23457, 23457, request).is_err());
        }
        assert_eq!(scheduler.get(id).unwrap().state, State::Pending);
        let queue = handle(
            &mut scheduler,
            &paths,
            other_owner.uid,
            other_owner.gid,
            Request::Queue,
        )
        .unwrap();
        let Response::Queue(jobs) = queue else {
            panic!("unexpected response");
        };
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|job| job.id == other_id));
        assert!(jobs.iter().any(|job| job.id == id));
        let json = serde_json::to_string(&jobs).unwrap();
        assert!(json.contains("bob") && json.contains("public-name"));
        assert!(json.contains("alice"));
        assert!(!json.contains("private-command") && !json.contains("private-token"));
        let root_queue = handle(&mut scheduler, &paths, 0, 0, Request::Queue).unwrap();
        assert!(matches!(root_queue, Response::Queue(jobs) if jobs.len() == 2));
        assert!(
            handle(
                &mut scheduler,
                &paths,
                owner.uid,
                owner.gid,
                Request::Get(id)
            )
            .is_ok()
        );
        let log = handle(
            &mut scheduler,
            &paths,
            owner.uid,
            owner.gid,
            Request::Log { id, offset: 8 },
        )
        .unwrap();
        assert!(matches!(log, Response::Log { bytes, offset: 16 } if bytes == b"output\x00\xff"));
        assert!(handle(&mut scheduler, &paths, 0, 0, Request::Cancel(id)).is_ok());
        assert_eq!(scheduler.get(id).unwrap().state, State::Cancelled);
        assert!(handle(&mut scheduler, &paths, 0, 0, Request::Stop).is_ok());
    }

    #[test]
    fn a_submission_cannot_forge_its_owner() {
        let json = r#"{"Submit":{"command":["id"],"cwd":"/","env":{},"name":"forged","count":0,"kind":null,"time_limit":null,"script":null,"owner":{"uid":0,"gid":0,"name":"root"}}}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }
}
