//! DCMI v2 bindings to the driver's dcmi_interface_api.h. Loaded only by the daemon.
use crate::model::{Device, Kind};
use anyhow::{Context, Result, ensure};
use libc::{c_int, c_uint, c_ulong, c_void};
use std::ffi::{CStr, CString};

type Init = unsafe extern "C" fn() -> c_int;
type DeviceList = unsafe extern "C" fn(*mut c_int, *mut c_int, c_int) -> c_int;
type ChipInfoFn = unsafe extern "C" fn(c_int, *mut ChipInfo) -> c_int;
type Health = unsafe extern "C" fn(c_int, *mut c_uint) -> c_int;
type Processes = unsafe extern "C" fn(c_int, *mut ProcessInfo, *mut c_int) -> c_int;

#[repr(C)]
#[derive(Default)]
struct ChipInfo {
    chip_type: [u8; 32],
    chip_name: [u8; 32],
    chip_ver: [u8; 32],
    aicore_cnt: c_uint,
    npu_name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessInfo {
    proc_id: c_int,
    proc_mem_usage: c_ulong,
}

struct Library(*mut c_void);

impl Library {
    fn open(path: &str) -> Result<Self> {
        let path = CString::new(path)?;
        // RTLD_LOCAL keeps vendor symbols out of the scheduler's global namespace.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        ensure!(!handle.is_null(), "cannot load DCMI: {}", loader_error());
        Ok(Self(handle))
    }

    fn symbol(&self, name: &CStr) -> Result<*mut c_void> {
        let symbol = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        ensure!(
            !symbol.is_null(),
            "missing {}: {}",
            name.to_string_lossy(),
            loader_error()
        );
        Ok(symbol)
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.0) };
    }
}

fn loader_error() -> String {
    let error = unsafe { libc::dlerror() };
    if error.is_null() {
        "unknown loader error".into()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }
}

struct Api {
    // Function pointers must never outlive the library handle.
    _library: Library,
    device_list: DeviceList,
    chip_info: ChipInfoFn,
    health: Health,
    processes: Processes,
}

impl Api {
    pub fn load() -> Result<Self> {
        let library = if let Ok(path) = std::env::var("XLURM_DCMI_LIBRARY") {
            Library::open(&path)?
        } else {
            Library::open("/usr/local/dcmi/libdcmi.so")
                .or_else(|_| Library::open("/usr/local/Ascend/driver/lib64/driver/libdcmi.so"))
                .or_else(|_| Library::open("libdcmi.so"))?
        };
        // Each cast uses the exact prototype from dcmi_interface_api.h; there is
        // no generic symbol-to-function cast that could hide an ABI mismatch.
        let dcmi = unsafe {
            let init: Init = std::mem::transmute(library.symbol(c"dcmiv2_init")?);
            let dcmi = Self {
                device_list: std::mem::transmute::<*mut c_void, DeviceList>(
                    library.symbol(c"dcmiv2_get_device_list")?,
                ),
                chip_info: std::mem::transmute::<*mut c_void, ChipInfoFn>(
                    library.symbol(c"dcmiv2_get_device_chip_info")?,
                ),
                health: std::mem::transmute::<*mut c_void, Health>(
                    library.symbol(c"dcmiv2_get_device_health")?,
                ),
                processes: std::mem::transmute::<*mut c_void, Processes>(
                    library.symbol(c"dcmiv2_get_device_resource_info")?,
                ),
                _library: library,
            };
            check(init(), "dcmiv2_init")?;
            dcmi
        };
        Ok(dcmi)
    }

    pub fn discover(&self) -> Result<Vec<Device>> {
        let mut count = 0;
        let mut ids = [0; 64];
        check(
            unsafe { (self.device_list)(ids.as_mut_ptr(), &mut count, ids.len() as c_int) },
            "dcmiv2_get_device_list",
        )?;
        ensure!(
            (1..=64).contains(&count),
            "invalid DCMI device count {count}"
        );
        let mut devices = Vec::new();
        for &id in &ids[..count as usize] {
            ensure!(id >= 0, "invalid DCMI logical ID {id}");
            let mut info = ChipInfo::default();
            check(
                unsafe { (self.chip_info)(id, &mut info) },
                "dcmiv2_get_device_chip_info",
            )?;
            let name = info.chip_name.split(|&byte| byte == 0).next().unwrap();
            devices.push(Device {
                kind: Kind::Ascend,
                id: id as u32,
                visible: id.to_string(),
                name: std::str::from_utf8(name)
                    .context("invalid DCMI chip name")?
                    .into(),
                // v2 uses logical IDs directly; physical management IDs are unnecessary.
                npu: None,
                chip: None,
            });
        }
        devices.sort_by_key(|device| device.id);
        super::validate(devices)
    }

    pub fn status(&self, device: &Device) -> &'static str {
        let Ok(id) = c_int::try_from(device.id) else {
            return "unknown";
        };
        let mut health = c_uint::MAX;
        let result = unsafe { (self.health)(id, &mut health) };
        if result != 0 || health == c_uint::MAX {
            return "unknown";
        }
        if health != 0 {
            return "unhealthy";
        }
        // The documented ABI writes at most 32 entries; proc_num is output-only.
        let mut processes = [ProcessInfo::default(); 32];
        let mut count = 0;
        let result = unsafe { (self.processes)(id, processes.as_mut_ptr(), &mut count) };
        if result != 0 || !(0..=32).contains(&count) {
            "unknown"
        } else if count == 0 {
            "idle"
        } else {
            "busy"
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LegacyChipInfo {
    chip_type: [u8; 32],
    chip_name: [u8; 32],
    chip_ver: [u8; 32],
    aicore_cnt: c_uint,
    npu_name: [u8; 32],
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LegacyProcessInfo {
    proc_id: c_int,
    proc_mem_usage: c_ulong,
}
type LegacyInit = unsafe extern "C" fn() -> c_int;
type LegacyCardList = unsafe extern "C" fn(*mut c_int, *mut c_int, c_int) -> c_int;
type LegacyDeviceCount = unsafe extern "C" fn(c_int, *mut c_int) -> c_int;
type LegacyChip = unsafe extern "C" fn(c_int, c_int, *mut LegacyChipInfo) -> c_int;
type LegacyHealth = unsafe extern "C" fn(c_int, c_int, *mut c_uint) -> c_int;
type LegacyProcesses =
    unsafe extern "C" fn(c_int, c_int, *mut LegacyProcessInfo, *mut c_int) -> c_int;

struct Legacy {
    _library: Library,
    shutdown: Option<LegacyInit>,
    cards: LegacyCardList,
    device_count: LegacyDeviceCount,
    chip: LegacyChip,
    health: LegacyHealth,
    processes: LegacyProcesses,
}
impl Legacy {
    fn load() -> Result<Self> {
        let library = Library::open("/usr/local/dcmi/libdcmi.so")
            .or_else(|_| Library::open("/usr/local/Ascend/driver/lib64/driver/libdcmi.so"))
            .or_else(|_| Library::open("libdcmi.so"))?;
        unsafe {
            let init: LegacyInit = std::mem::transmute(library.symbol(c"dcmi_init")?);
            check(init(), "dcmi_init")?;
            let shutdown = library
                .symbol(c"dcmi_shutdown")
                .ok()
                .map(|p| std::mem::transmute::<*mut c_void, LegacyInit>(p));
            Ok(Self {
                cards: std::mem::transmute::<*mut c_void, LegacyCardList>(
                    library.symbol(c"dcmi_get_card_list")?,
                ),
                device_count: std::mem::transmute::<*mut c_void, LegacyDeviceCount>(
                    library.symbol(c"dcmi_get_device_num_in_card")?,
                ),
                chip: std::mem::transmute::<*mut c_void, LegacyChip>(
                    library
                        .symbol(c"dcmi_get_device_chip_info_v2")
                        .or_else(|_| library.symbol(c"dcmi_get_device_chip_info"))?,
                ),
                health: std::mem::transmute::<*mut c_void, LegacyHealth>(
                    library.symbol(c"dcmi_get_device_health")?,
                ),
                processes: std::mem::transmute::<*mut c_void, LegacyProcesses>(
                    library.symbol(c"dcmi_get_device_resource_info")?,
                ),
                shutdown,
                _library: library,
            })
        }
    }
    fn card_ids(&self) -> Result<Vec<c_int>> {
        let mut count = 0;
        let mut cards = [0; 64];
        check(
            unsafe { (self.cards)(&mut count, cards.as_mut_ptr(), cards.len() as c_int) },
            "dcmi_get_card_list",
        )?;
        ensure!((1..=64).contains(&count), "invalid DCMI card count {count}");
        Ok(cards[..count as usize].to_vec())
    }
    fn discover(&self) -> Result<Vec<Device>> {
        let mut devices = Vec::new();
        for card in self.card_ids()? {
            let mut count = 0;
            check(
                unsafe { (self.device_count)(card, &mut count) },
                "dcmi_get_device_num_in_card",
            )?;
            ensure!(
                count > 0 && count <= 64,
                "invalid DCMI device count {count}"
            );
            let device = 0;
            let mut info = LegacyChipInfo::default();
            check(
                unsafe { (self.chip)(card, device, &mut info) },
                "dcmi_get_device_chip_info",
            )?;
            let end = info
                .chip_name
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(info.chip_name.len());
            let name =
                std::str::from_utf8(&info.chip_name[..end]).context("invalid DCMI chip name")?;
            devices.push(Device {
                kind: Kind::Ascend,
                id: card as u32,
                visible: card.to_string(),
                name: name.into(),
                npu: Some(card as u32),
                chip: Some(device as u32),
            });
        }
        super::validate(devices)
    }
    fn status(&self, device: &Device) -> &'static str {
        let card = match c_int::try_from(device.npu.unwrap_or(device.id)) {
            Ok(v) => v,
            Err(_) => return "unknown",
        };
        let chip = c_int::try_from(device.chip.unwrap_or(0)).unwrap_or(0);
        let mut health = c_uint::MAX;
        if unsafe { (self.health)(card, chip, &mut health) } != 0 || health == c_uint::MAX {
            return "unknown";
        }
        if health != 0 {
            return "unhealthy";
        }
        let mut processes = [LegacyProcessInfo::default(); 64];
        let mut count = processes.len() as c_int;
        if unsafe { (self.processes)(card, chip, processes.as_mut_ptr(), &mut count) } != 0
            || !(0..=64).contains(&count)
        {
            return "unknown";
        }
        if count == 0 { "idle" } else { "busy" }
    }
}
impl Drop for Legacy {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown {
            unsafe {
                shutdown();
            }
        }
    }
}

enum Backend {
    Api(Api),
    Legacy(Legacy),
}
pub struct Dcmi {
    backend: Backend,
}
impl Dcmi {
    pub fn load() -> Result<Self> {
        if std::env::var_os("XLURM_DCMI_LIBRARY").is_some() {
            return Ok(Self {
                backend: Backend::Api(Api::load()?),
            });
        }
        match Api::load() {
            Ok(api) => Ok(Self {
                backend: Backend::Api(api),
            }),
            Err(v2_error) => match Legacy::load() {
                Ok(legacy) => {
                    eprintln!(
                        "Ascend: DCMI v2 symbols unavailable ({v2_error:#}); using dcmi_* API"
                    );
                    Ok(Self {
                        backend: Backend::Legacy(legacy),
                    })
                }
                Err(v1_error) => Err(anyhow::anyhow!(
                    "DCMI v2 and dcmi_* APIs unavailable: v2: {v2_error:#}; legacy: {v1_error:#}"
                )),
            },
        }
    }
    pub fn discover(&self) -> Result<Vec<Device>> {
        match &self.backend {
            Backend::Api(api) => api.discover(),
            Backend::Legacy(api) => api.discover(),
        }
    }
    pub fn status(&self, device: &Device) -> &'static str {
        match &self.backend {
            Backend::Api(api) => api.status(device),
            Backend::Legacy(api) => api.status(device),
        }
    }
}

fn check(code: c_int, operation: &str) -> Result<()> {
    ensure!(code == 0, "{operation} failed with DCMI error {code}");
    Ok(())
}
