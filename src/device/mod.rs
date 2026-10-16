mod ascend;
mod hcu;
mod metax;
mod mthreads;
mod ppu;

use crate::model::{Device, Kind};
use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    #[default]
    Auto,
    #[value(alias = "nv")]
    Nvidia,
    #[value(alias = "asc")]
    Ascend,
    #[value(alias = "mtt")]
    Mthreads,
    #[value(alias = "mx")]
    Metax,
    Ppu,
    Hcu,
    None,
}

pub struct Inventory {
    pub devices: Vec<Device>,
    pub status: HashMap<String, String>,
    nvidia: PathBuf,
    ascend: Option<ascend::Dcmi>,
    mthreads: Option<mthreads::Mtml>,
    metax: Option<metax::Mxsml>,
    ppu: Option<ppu::Hgml>,
    hcu: Option<hcu::Smi>,
    hcu_health: PathBuf,
}

impl Inventory {
    pub fn discover(backend: Backend) -> Result<Self> {
        let mut inventory = Self {
            devices: vec![],
            status: HashMap::new(),
            nvidia: locate("nvidia-smi", &[]),
            ascend: None,
            mthreads: None,
            metax: None,
            ppu: None,
            hcu: None,
            hcu_health: hcu::health_tool(),
        };
        for kind in Kind::PRIORITY {
            if backend == Backend::None
                || (backend == Backend::Nvidia && kind != Kind::Nvidia)
                || (backend == Backend::Ascend && kind != Kind::Ascend)
                || (backend == Backend::Mthreads && kind != Kind::Mthreads)
                || (backend == Backend::Metax && kind != Kind::Metax)
                || (backend == Backend::Ppu && kind != Kind::Ppu)
                || (backend == Backend::Hcu && kind != Kind::Hcu)
            {
                continue;
            }
            let found = match kind {
                Kind::Nvidia => output(
                    &inventory.nvidia,
                    &[
                        "--query-gpu=index,uuid,name",
                        "--format=csv,noheader,nounits",
                    ],
                )
                .and_then(|text| parse_nvidia(&text)),
                Kind::Ascend => ascend::Dcmi::load().and_then(|driver| {
                    let devices = driver.discover()?;
                    inventory.ascend = Some(driver);
                    eprintln!("Ascend: using DCMI API");
                    Ok(devices)
                }),
                Kind::Mthreads => mthreads::Mtml::load().and_then(|driver| {
                    let devices = driver.discover()?;
                    inventory.mthreads = Some(driver);
                    eprintln!("MThreads: using MTML API");
                    Ok(devices)
                }),
                Kind::Metax => metax::Mxsml::load().and_then(|driver| {
                    let devices = driver.discover()?;
                    inventory.metax = Some(driver);
                    eprintln!("MetaX: using MXSML API");
                    Ok(devices)
                }),
                Kind::Ppu => ppu::Hgml::load().and_then(|driver| {
                    let devices = driver.discover()?;
                    inventory.ppu = Some(driver);
                    eprintln!("PPU: using HGML API");
                    Ok(devices)
                }),
                Kind::Hcu => hcu::Smi::load().and_then(|driver| {
                    let devices = driver.discover()?;
                    inventory.hcu = Some(driver);
                    eprintln!("HCU: using SMI API (hy-smi for health only)");
                    Ok(devices)
                }),
            };
            match found {
                Ok(devices) => inventory.devices.extend(devices),
                Err(error) if backend == Backend::Auto => eprintln!("{kind} discovery: {error:#}"),
                Err(error) => return Err(error),
            }
        }
        inventory.refresh();
        Ok(inventory)
    }

    /// Failed monitoring is unknown/busy, never permission to allocate.
    pub fn refresh(&mut self) {
        let nvidia_busy = self
            .devices
            .iter()
            .any(|d| d.kind == Kind::Nvidia)
            .then(|| {
                output(
                    &self.nvidia,
                    &[
                        "--query-compute-apps=gpu_uuid",
                        "--format=csv,noheader,nounits",
                    ],
                )
            });
        let hcu_busy = self.devices.iter().any(|d| d.kind == Kind::Hcu).then(|| {
            self.hcu
                .as_ref()
                .context("HCU SMI unavailable")?
                .busy_devices()
        });
        let hcu_health = self
            .devices
            .iter()
            .any(|d| d.kind == Kind::Hcu)
            .then(|| output(&self.hcu_health, &["--healthcheck"]));
        let hcu_healthy = hcu_health
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .and_then(|text| hcu_health_status(text));
        for device in &self.devices {
            let mut unhealthy = false;
            let idle = match device.kind {
                Kind::Nvidia => nvidia_busy
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .filter(|text| {
                        text.lines()
                            .all(|line| line.trim().is_empty() || line.trim().starts_with("GPU-"))
                    })
                    .map(|text| !text.lines().any(|line| line.trim() == device.visible)),
                Kind::Ascend => {
                    let status = self
                        .ascend
                        .as_ref()
                        .map_or("unknown", |driver| driver.status(device));
                    self.status.insert(device.key(), status.into());
                    continue;
                }
                Kind::Mthreads => {
                    let status = self
                        .mthreads
                        .as_ref()
                        .map_or("unknown", |driver| driver.status(device));
                    self.status.insert(device.key(), status.into());
                    continue;
                }
                Kind::Metax => {
                    let status = self
                        .metax
                        .as_ref()
                        .map_or("unknown", |driver| driver.status(device));
                    self.status.insert(device.key(), status.into());
                    continue;
                }
                Kind::Ppu => {
                    let status = self
                        .ppu
                        .as_ref()
                        .map_or("unknown", |driver| driver.status(device));
                    self.status.insert(device.key(), status.into());
                    continue;
                }
                Kind::Hcu => {
                    let healthy = hcu_healthy
                        .as_ref()
                        .and_then(|health| health.get(&device.id))
                        .copied();
                    unhealthy = healthy == Some(false);
                    let process_idle = hcu_busy
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .map(|busy| !busy.contains(&device.id));
                    if healthy == Some(true) {
                        process_idle
                    } else {
                        None
                    }
                }
            };
            let status = if unhealthy {
                "unhealthy"
            } else {
                match idle {
                    Some(true) => "idle",
                    Some(false) => "busy",
                    None => "unknown",
                }
            };
            self.status.insert(device.key(), status.into());
        }
    }
}

fn hcu_health_status(text: &str) -> Option<HashMap<u32, bool>> {
    let mut health = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((card, status)) = line.split_once(':') else {
            continue;
        };
        let Some(card) = card.trim().strip_prefix("HCU[") else {
            continue;
        };
        let id = card.trim_end_matches(']').parse::<u32>().ok()?;
        let status = status.rsplit(':').next()?.trim();
        if status.is_empty() {
            return None;
        }
        health.insert(id, status.eq_ignore_ascii_case("healthy"));
    }
    (!health.is_empty()).then_some(health)
}

fn locate(program: &str, fallbacks: &[&str]) -> PathBuf {
    if let Some(path) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())
    }) {
        return path;
    }
    fallbacks
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| program.into())
}

/// Driver utilities must not be able to hang the scheduler indefinitely.
fn output(program: &PathBuf, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot execute {}", program.display()))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let readers: Vec<_> = [Box::new(stdout) as Box<dyn Read + Send>, Box::new(stderr)]
        .into_iter()
        .map(|mut pipe| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                pipe.read_to_end(&mut bytes).map(|_| bytes)
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    child.wait()?;
    let bytes: Vec<_> = readers
        .into_iter()
        .map(|r| {
            r.join()
                .map_err(|_| anyhow::anyhow!("driver output reader panicked"))?
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<_>>()?;
    let status = status.context("device probe timed out after 3 seconds")?;
    ensure!(
        status.success(),
        "{} failed: {} {}",
        program.display(),
        String::from_utf8_lossy(&bytes[0]).trim(),
        String::from_utf8_lossy(&bytes[1]).trim()
    );
    Ok(String::from_utf8(bytes[0].clone())?)
}

fn parse_nvidia(text: &str) -> Result<Vec<Device>> {
    let mut devices = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.splitn(3, ',').map(str::trim).collect();
        ensure!(
            fields.len() == 3 && fields[1].starts_with("GPU-"),
            "invalid nvidia-smi inventory"
        );
        devices.push(Device {
            kind: Kind::Nvidia,
            id: fields[0].parse()?,
            visible: fields[1].into(),
            name: fields[2].into(),
            npu: None,
            chip: None,
        });
    }
    validate(devices)
}

fn validate(devices: Vec<Device>) -> Result<Vec<Device>> {
    if devices.is_empty() {
        bail!("no compute devices discovered");
    }
    let mut keys = HashSet::new();
    ensure!(
        devices.iter().all(|device| keys.insert(device.key())),
        "duplicate device IDs in inventory"
    );
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_uses_uuid_instead_of_unstable_ordinal() {
        let devices = parse_nvidia("3, GPU-abc, NVIDIA H100\n").unwrap();
        assert_eq!(devices[0].visible, "GPU-abc");
        assert!(parse_nvidia("driver failure").is_err());
    }

    #[test]
    fn hcu_health_fails_closed_on_missing_or_malformed_data() {
        let health = hcu_health_status(
            "System Management Interface\nHCU[0] : Bus Id : 0000:01:00.0 : Healthy\nHCU[1] : Bus Id : 0000:02:00.0 : Critical\n",
        );
        assert_eq!(health, Some(HashMap::from([(0, true), (1, false)])));
        assert_eq!(hcu_health_status("probe failed"), None);
    }
}
