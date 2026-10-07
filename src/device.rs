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
    ascend: PathBuf,
    mthreads: PathBuf,
    metax: PathBuf,
    ppu: PathBuf,
    hcu: PathBuf,
}

impl Inventory {
    pub fn discover(backend: Backend) -> Result<Self> {
        let mut inventory = Self {
            devices: vec![],
            status: HashMap::new(),
            nvidia: locate("nvidia-smi", &[]),
            ascend: locate(
                "npu-smi",
                &[
                    "/usr/local/sbin/npu-smi",
                    "/usr/local/Ascend/driver/tools/npu-smi",
                ],
            ),
            mthreads: locate("mthreads-gmi", &[]),
            metax: locate("mx-smi", &["/opt/mxdriver/bin/mx-smi"]),
            ppu: locate("ppu-smi", &["/usr/local/PPU_SDK/ppu-smi/bin/ppu-smi"]),
            hcu: locate("hy-smi", &["/opt/dtk/.hyhal/bin/hy-smi"]),
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
                Kind::Ascend => {
                    output(&inventory.ascend, &["info", "-m"]).and_then(|text| parse_ascend(&text))
                }
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
                Kind::Hcu => output(&inventory.hcu, &["--json", "--showuniqueid"])
                    .and_then(|text| parse_hcu(&text)),
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
        let hcu_busy = self
            .devices
            .iter()
            .any(|d| d.kind == Kind::Hcu)
            .then(|| output(&self.hcu, &["--json", "--showpids"]));
        let hcu_health = self
            .devices
            .iter()
            .any(|d| d.kind == Kind::Hcu)
            .then(|| output(&self.hcu, &["--healthcheck"]));
        let hcu_healthy = hcu_health
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .and_then(|text| hcu_health_status(text));
        let mut ascend_health = HashMap::new();
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
                    let npu_id = device.npu.unwrap();
                    let npu = npu_id.to_string();
                    let chip = device.chip.map(|chip| chip.to_string());
                    let mut args = vec!["info", "-t", "proc-mem", "-i", &npu];
                    if let Some(chip) = &chip {
                        args.extend(["-c", chip]);
                    }
                    let process_idle = output(&self.ascend, &args)
                        .ok()
                        .and_then(|text| ascend_idle(&text));
                    let healthy = *ascend_health.entry(npu_id).or_insert_with(|| {
                        output(&self.ascend, &["info", "-t", "health", "-i", &npu])
                            .ok()
                            .and_then(|text| ascend_healthy(&text))
                    });
                    unhealthy = healthy == Some(false);
                    match healthy {
                        Some(false) => Some(false),
                        Some(true) => process_idle,
                        None => None,
                    }
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
                        .and_then(|text| hcu_busy_devices(text))
                        .and_then(|busy| busy.get(&device.id).copied())
                        .map(|busy| !busy);
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

fn parse_hcu(text: &str) -> Result<Vec<Device>> {
    let cards: serde_json::Value = serde_json::from_str(text).context("invalid hy-smi JSON")?;
    let cards = cards.as_object().context("invalid hy-smi inventory")?;
    let mut devices = Vec::new();
    for (card, details) in cards {
        let id = card
            .strip_prefix("card")
            .context("invalid hy-smi card key")?
            .parse::<u32>()
            .context("invalid hy-smi card index")?;
        let visible = details
            .get("Unique ID")
            .and_then(serde_json::Value::as_str)
            .context("missing HCU unique ID")?;
        ensure!(!visible.is_empty(), "empty HCU unique ID");
        devices.push(Device {
            kind: Kind::Hcu,
            id,
            name: "HCU".into(),
            visible: visible.into(),
            npu: None,
            chip: None,
        });
    }
    devices.sort_by_key(|device| device.id);
    validate(devices)
}

fn hcu_busy_devices(text: &str) -> Option<HashMap<u32, bool>> {
    let cards: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut busy = HashMap::new();
    for (card, details) in cards.as_object()? {
        let id = card.strip_prefix("card")?.parse::<u32>().ok()?;
        let details = details.as_object()?;
        busy.insert(id, !details.is_empty());
    }
    (!busy.is_empty()).then_some(busy)
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

fn ascend_idle(text: &str) -> Option<bool> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("process id:") || lower.contains("process id :") {
        Some(false)
    } else if lower.contains("no running processes") || lower.contains("no process in device") {
        Some(true)
    } else {
        None
    }
}

fn ascend_healthy(text: &str) -> Option<bool> {
    let mut found_status = false;
    for (key, value) in text.lines().filter_map(|line| line.split_once(':')) {
        let key = key.trim();
        if !key.eq_ignore_ascii_case("health") && !key.eq_ignore_ascii_case("health status") {
            continue;
        }
        found_status = true;
        if !value.trim().eq_ignore_ascii_case("ok") {
            return Some(false);
        }
    }
    found_status.then_some(true)
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

fn parse_ascend(text: &str) -> Result<Vec<Device>> {
    let mut lines = text.lines();
    let header = lines
        .find(|line| line.contains("NPU ID"))
        .context("missing Ascend mapping header")?;
    // Turn multiword column names into single tokens before indexing data rows.
    let mut header = header.to_string();
    for label in [
        "Chip Logic ID",
        "Chip Phy-ID",
        "Chip Count",
        "Chip Name",
        "Chip ID",
        "Slot ID",
        "Device ID",
        "NPU ID",
    ] {
        header = header.replace(label, &label.replace(' ', "_"));
    }
    let columns: Vec<_> = header.split_whitespace().collect();
    let index = |name: &str| columns.iter().position(|column| *column == name);
    let npu_column = index("NPU_ID").context("missing NPU ID")?;
    let logic_column = index("Chip_Logic_ID").or_else(|| index("Chip_Phy-ID"));
    let mut devices = Vec::new();
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.is_empty() || fields.iter().any(|f| f.eq_ignore_ascii_case("mcu")) {
            continue;
        }
        let get = |column: usize| {
            fields
                .get(column)
                .copied()
                .context("short Ascend mapping row")
        };
        let Ok(npu) = get(npu_column)?.parse::<u32>() else {
            continue;
        };
        let id: u32 = get(logic_column.unwrap_or(npu_column))?
            .parse()
            .context("invalid logical device ID; refusing to guess Ascend mapping")?;
        let chip = index("Chip_ID")
            .map(|column| {
                get(column)?
                    .parse::<u32>()
                    .context("invalid Ascend chip ID")
            })
            .transpose()?;
        devices.push(Device {
            kind: Kind::Ascend,
            id,
            visible: id.to_string(),
            npu: Some(npu),
            chip,
            name: index("Chip_Name")
                .map(get)
                .transpose()?
                .unwrap_or("Ascend")
                .into(),
        });
    }
    let mut counts = HashMap::new();
    for device in &devices {
        *counts.entry(device.npu).or_insert(0) += 1;
    }
    for device in &mut devices {
        if counts[&device.npu] == 1 {
            device.chip = None;
        }
    }
    devices.sort_by_key(|device| device.id);
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
    fn ascend_uses_logical_ids_and_ignores_management_chips() {
        let devices = parse_ascend("NPU ID  Chip ID  Chip Logic ID  Chip Name\n4 0 2 Ascend910B\n4 1 3 Ascend910B\n4 2 - Mcu\n").unwrap();
        assert_eq!(
            devices
                .iter()
                .map(|d| (d.id, d.npu, d.chip))
                .collect::<Vec<_>>(),
            vec![(2, Some(4), Some(0)), (3, Some(4), Some(1))]
        );
    }

    #[test]
    fn ascend_supports_phy_id_and_rejects_ambiguous_mapping() {
        let devices =
            parse_ascend("NPU ID Slot ID Chip ID Chip Phy-ID Chip Name\n7 0 0 5 Ascend950PR\n")
                .unwrap();
        assert_eq!(devices[0].visible, "5");
        assert_eq!(devices[0].chip, None);
        assert!(parse_ascend("NPU ID Chip ID Chip Name\n0 0 Ascend\n0 1 Ascend\n").is_err());
    }

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
    fn hcu_parses_unique_ids_and_fail_closed_health_and_pid_data() {
        let devices =
            parse_hcu(r#"{"card1":{"Unique ID":"hcu-1"},"card0":{"Unique ID":"hcu-0"}}"#).unwrap();
        assert_eq!(devices[0].id, 0);
        assert_eq!(devices[0].visible, "hcu-0");
        assert_eq!(
            hcu_busy_devices(r#"{"card0":{},"card1":{"pid":"1234"}}"#),
            Some(HashMap::from([(0, false), (1, true)]))
        );
        assert_eq!(hcu_busy_devices("invalid"), None);
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

    #[test]
    fn ascend_process_formats_fail_closed() {
        assert_eq!(
            ascend_idle("NPU ID : 0\nNo process in device.\n"),
            Some(true)
        );
        assert_eq!(
            ascend_idle("No running processes found in NPU 4"),
            Some(true)
        );
        assert_eq!(
            ascend_idle("Process id:45180 Process name:VLLMEngineCor"),
            Some(false)
        );
        assert_eq!(ascend_idle("Failed to query device"), None);
    }

    #[test]
    fn ascend_health_requires_every_reported_health_status_to_be_ok() {
        assert_eq!(ascend_healthy("Health : OK\nHealth : OK\n"), Some(true));
        assert_eq!(ascend_healthy("Health Status : Alarm\n"), Some(false));
        assert_eq!(ascend_healthy("Health : Critical\n"), Some(false));
        assert_eq!(ascend_healthy("Health : OK\nHealth : Alarm\n"), Some(false));
        assert_eq!(ascend_healthy("health query failed"), None);
    }
}
