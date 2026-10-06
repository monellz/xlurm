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
    None,
}

pub struct Inventory {
    pub devices: Vec<Device>,
    pub status: HashMap<String, String>,
    nvidia: PathBuf,
    ascend: PathBuf,
    mthreads: PathBuf,
    metax: PathBuf,
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
        };
        for kind in Kind::PRIORITY {
            if backend == Backend::None
                || (backend == Backend::Nvidia && kind != Kind::Nvidia)
                || (backend == Backend::Ascend && kind != Kind::Ascend)
                || (backend == Backend::Mthreads && kind != Kind::Mthreads)
                || (backend == Backend::Metax && kind != Kind::Metax)
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
        for device in &self.devices {
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
                    let npu = device.npu.unwrap().to_string();
                    let chip = device.chip.map(|chip| chip.to_string());
                    let mut args = vec!["info", "-t", "proc-mem", "-i", &npu];
                    if let Some(chip) = &chip {
                        args.extend(["-c", chip]);
                    }
                    output(&self.ascend, &args)
                        .ok()
                        .and_then(|text| ascend_idle(&text))
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
            };
            self.status.insert(
                device.key(),
                match idle {
                    Some(true) => "idle",
                    Some(false) => "busy",
                    None => "unknown",
                }
                .into(),
            );
        }
    }
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
}
