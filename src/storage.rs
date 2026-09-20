use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Paths(pub PathBuf);

impl Paths {
    pub fn discover() -> Result<Self> {
        let path = match std::env::var_os("XLURM_HOME") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from("/var/lib/xlurm"),
        };
        Ok(Self(if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        }))
    }

    /// Only the daemon creates state. Clients need traverse/socket access, not
    /// write access to job metadata or logs.
    pub fn initialize(&self) -> Result<()> {
        if unsafe { libc::geteuid() } == 0 {
            for ancestor in self.0.ancestors().skip(1) {
                let meta = match fs::symlink_metadata(ancestor) {
                    Ok(meta) => meta,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                ensure!(
                    meta.is_dir()
                        && meta.uid() == 0
                        && (meta.mode() & 0o022 == 0 || meta.mode() & libc::S_ISVTX != 0),
                    "root daemon state must be under trusted root-owned directories: {}",
                    ancestor.display()
                );
            }
        }
        fs::create_dir_all(&self.0)
            .context("cannot create state directory; start the shared daemon with sudo")?;
        let meta = fs::symlink_metadata(&self.0)?;
        ensure!(
            meta.is_dir() && meta.uid() == unsafe { libc::geteuid() },
            "XLURM_HOME must be a directory owned by the daemon account"
        );
        ensure!(
            meta.mode() & 0o022 == 0,
            "XLURM_HOME must not be writable by other users"
        );
        fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755))?;
        let jobs = self.0.join("jobs");
        fs::create_dir_all(&jobs)?;
        let meta = fs::symlink_metadata(&jobs)?;
        ensure!(
            meta.is_dir() && meta.uid() == unsafe { libc::geteuid() },
            "invalid jobs directory owner"
        );
        fs::set_permissions(jobs, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    pub fn socket(&self) -> PathBuf {
        self.0.join("xlurm.sock")
    }

    pub fn job(&self, id: u64, extension: &str) -> PathBuf {
        self.0.join("jobs").join(format!("{id}.{extension}"))
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).with_context(|| path.display().to_string())?)
        .with_context(|| format!("invalid JSON: {}", path.display()))
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().context("missing parent directory")?)?.sync_all()?;
    Ok(())
}

/// A flock survives exec when explicitly inherited by the worker. Its lifetime
/// closes the spawn/recovery race without using PID files as a liveness oracle.
pub fn try_lock(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(error.into())
    }
}
