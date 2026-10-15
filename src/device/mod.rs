mod ascend;
mod hcu;

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
    mthreads: PathBuf,
    metax: PathBuf,
    ppu: PathBuf,
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
            mthreads: locate("mthreads-gmi", &[]),
            metax: locate("mx-smi", &["/opt/mxdriver/bin/mx-smi"]),
            ppu: locate("ppu-smi", &["/usr/local/PPU_SDK/ppu-smi/bin/ppu-smi"]),
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
                    eprintln!("Ascend: using DCMI v2 API");
                    Ok(devices)
                }),
                Kind::Mthreads => output(&inventory.mthreads, &["--list-gpus"])
                    .and_then(|text| parse_mthreads(&text)),
                Kind::Metax => {
                    output(&inventory.metax, &["-L"]).and_then(|text| parse_metax(&text))
                }
                Kind::Ppu => output(
                    &inventory.ppu,
                    &["--query-ppu=index,uuid,name", "--format=csv"],
                )
                .and_then(|text| parse_ppu(&text)),
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
        let mthreads_busy = self
            .devices
            .iter()
            .any(|d| d.kind == Kind::Mthreads)
            .then(|| output(&self.mthreads, &[]));
        let metax_busy = self
            .devices
            .iter()
            .any(|d| d.kind == Kind::Metax)
            .then(|| output(&self.metax, &["--show-all-process"]));
        let ppu_busy = self.devices.iter().any(|d| d.kind == Kind::Ppu).then(|| {
            output(
                &self.ppu,
                &["--query-compute-apps=uuid,pid", "--format=csv"],
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
                Kind::Mthreads => mthreads_busy
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .and_then(|text| mthreads_busy_devices(text))
                    .map(|busy| !busy.contains(&device.id)),
                Kind::Metax => metax_busy
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .and_then(|text| metax_busy_devices(text))
                    .map(|busy| !busy.contains(&device.id)),
                Kind::Ppu => ppu_busy
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .and_then(|text| ppu_busy_devices(text))
                    .map(|busy| !busy.contains(&device.visible)),
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

fn parse_mthreads(text: &str) -> Result<Vec<Device>> {
    let mut devices = Vec::new();
    for line in text.lines().filter(|line| line.contains("UUID")) {
        let (left, right) = line
            .split_once(":")
            .context("invalid mthreads-gmi device row")?;
        let id = left
            .trim()
            .strip_prefix("GPU ")
            .context("invalid mthreads-gmi device ID")?
            .parse::<u32>()?;
        let name = right
            .split("(UUID")
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let uuid = right
            .split_once("UUID :")
            .context("missing mthreads GPU UUID")?
            .1
            .trim()
            .trim_end_matches(')')
            .trim();
        ensure!(!uuid.is_empty(), "missing mthreads GPU UUID");
        devices.push(Device {
            kind: Kind::Mthreads,
            id,
            visible: uuid.into(),
            name,
            npu: None,
            chip: None,
        });
    }
    validate(devices)
}

fn mthreads_busy_devices(text: &str) -> Option<HashSet<u32>> {
    let mut in_process_table = false;
    let mut saw_process_header = false;
    let mut busy = HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "Processes:" {
            in_process_table = true;
            saw_process_header = true;
            continue;
        }
        if !in_process_table
            || trimmed.is_empty()
            || trimmed.starts_with('+')
            || trimmed.starts_with("ID ")
            || trimmed == "Usage"
            || trimmed.eq_ignore_ascii_case("No running processes found")
            || trimmed.chars().all(|ch| ch == '-')
        {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let id = fields.next()?.parse::<u32>().ok()?;
        let pid = fields.next()?.parse::<u32>().ok()?;
        if pid > 0 {
            busy.insert(id);
        }
    }
    saw_process_header.then_some(busy)
}

fn parse_metax(text: &str) -> Result<Vec<Device>> {
    let mut devices = Vec::new();
    for line in text
        .lines()
        .filter(|line| line.trim_start().starts_with("GPU#"))
    {
        let line = line.trim();
        let (id, rest) = line
            .strip_prefix("GPU#")
            .context("invalid mx-smi device ID")?
            .split_once(char::is_whitespace)
            .context("invalid mx-smi device row")?;
        let id = id.parse::<u32>().context("invalid mx-smi device ID")?;
        ensure!(
            !rest.contains("Not Available"),
            "MetaX GPU {id} is not available to this process"
        );
        let (name, uuid) = rest
            .split_once("(UUID:")
            .context("missing MetaX GPU UUID")?;
        let uuid = uuid.trim().trim_end_matches(')').trim();
        ensure!(
            uuid.starts_with("GPU-") && !uuid.contains(char::is_whitespace),
            "invalid MetaX GPU UUID"
        );
        devices.push(Device {
            kind: Kind::Metax,
            id,
            visible: uuid.into(),
            name: name.split_whitespace().next().unwrap_or("MetaX").into(),
            npu: None,
            chip: None,
        });
    }
    validate(devices)
}

fn parse_ppu(text: &str) -> Result<Vec<Device>> {
    let mut lines = text.lines();
    let header = lines.next().context("missing PPU-SMI CSV header")?;
    ensure!(
        header
            .split(',')
            .map(str::trim)
            .eq(["index", "uuid", "name"]),
        "invalid PPU-SMI CSV header"
    );
    let mut devices = Vec::new();
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.splitn(3, ',').map(str::trim).collect();
        ensure!(
            fields.len() == 3 && ppu_uuid(fields[1]) && !fields[2].is_empty(),
            "invalid PPU-SMI device row"
        );
        devices.push(Device {
            kind: Kind::Ppu,
            id: fields[0].parse().context("invalid PPU device index")?,
            visible: fields[1].into(),
            name: fields[2].into(),
            npu: None,
            chip: None,
        });
    }
    validate(devices)
}

fn ppu_busy_devices(text: &str) -> Option<HashSet<String>> {
    let mut lines = text.lines();
    let header = lines.next()?;
    let columns: Vec<_> = header.split(',').map(str::trim).collect();
    let uuid_column = columns.iter().position(|column| *column == "uuid")?;
    let pid_column = columns.iter().position(|column| *column == "pid")?;
    let mut busy = HashSet::new();
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        let uuid = *fields.get(uuid_column)?;
        let pid = fields.get(pid_column)?.parse::<u32>().ok()?;
        if !ppu_uuid(uuid) {
            return None;
        }
        if pid > 0 {
            busy.insert(uuid.to_string());
        }
    }
    Some(busy)
}

fn ppu_uuid(value: &str) -> bool {
    value.starts_with("GPU-") || value.starts_with("PPU-")
}

fn metax_busy_devices(text: &str) -> Option<HashSet<u32>> {
    if text.lines().any(|line| line.contains("no process found")) {
        return Some(HashSet::new());
    }
    let mut in_process_table = false;
    let mut saw_process_header = false;
    let mut busy = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("| Process:") {
            in_process_table = true;
            continue;
        }
        if !in_process_table || line.is_empty() || line.starts_with('|') && line.contains("GPU ") {
            continue;
        }
        let fields = line
            .trim_matches('|')
            .split_whitespace()
            .collect::<Vec<_>>();
        if fields.len() >= 2
            && let (Ok(id), Ok(pid)) = (fields[0].parse::<u32>(), fields[1].parse::<u32>())
        {
            saw_process_header = true;
            if pid > 0 {
                busy.insert(id);
            }
        }
    }
    saw_process_header.then_some(busy)
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
    fn ppu_supports_driver_uuid_prefixes_and_tracks_busy_uuid() {
        let devices =
            parse_ppu("index, uuid, name\n0, GPU-abc, PPU-ZW810E\n1, PPU-def, PPU-ZW810E\n")
                .unwrap();
        assert_eq!(devices[0].kind, Kind::Ppu);
        assert_eq!(devices[1].visible, "PPU-def");
        assert_eq!(
            ppu_busy_devices("uuid, pid\nPPU-def, 1234\n"),
            Some(HashSet::from(["PPU-def".into()]))
        );
        assert!(parse_ppu("index, uuid, name\n0, invalid, PPU\n").is_err());
        assert_eq!(ppu_busy_devices("invalid query response"), None);
    }

    #[test]
    fn hcu_health_fails_closed_on_missing_or_malformed_data() {
        let health = hcu_health_status(
            "System Management Interface\nHCU[0] : Bus Id : 0000:01:00.0 : Healthy\nHCU[1] : Bus Id : 0000:02:00.0 : Critical\n",
        );
        assert_eq!(health, Some(HashMap::from([(0, true), (1, false)])));
        assert_eq!(hcu_health_status("probe failed"), None);
    }

    #[test]
    fn metax_uses_uuid_for_identity_and_fails_on_partial_inventory() {
        let devices = parse_metax(
            "mx-smi  version: 2.3.1\nGPU#0    MXC550      0000:2b:00.0   Available (UUID: GPU-abc-123)\n",
        )
        .unwrap();
        assert_eq!(devices[0].kind, Kind::Metax);
        assert_eq!(devices[0].visible, "GPU-abc-123");
        assert_eq!(devices[0].name, "MXC550");
        assert!(parse_metax(
            "GPU#0 MXC550 bus Available (UUID: GPU-abc)\nGPU#1 MXC550 bus Not Available(MetaX Sysfs File Unaccessible)\n"
        )
        .is_err());
    }

    #[test]
    fn metax_process_table_fails_closed_without_known_empty_or_rows() {
        assert_eq!(
            metax_busy_devices("| Process: |\n| no process found |\n"),
            Some(HashSet::new())
        );
        assert_eq!(
            metax_busy_devices("| Process: |\n| GPU PID Process Name |\n| 3 1234 train 100MiB |\n"),
            Some(HashSet::from([3]))
        );
        assert_eq!(metax_busy_devices("| Process: |\n| unavailable |\n"), None);
    }
}
