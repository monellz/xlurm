//! Minimal bindings to the installed HCU rocm_smi_v2.h. Daemon-only, read-only.
use crate::model::{Device, Kind};
use anyhow::{Result, ensure};
use libc::{c_int, c_void};
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::path::PathBuf;

type Init = unsafe extern "C" fn(u64) -> c_int;
type Shutdown = unsafe extern "C" fn() -> c_int;
type Count = unsafe extern "C" fn(*mut u32) -> c_int;
type UniqueId = unsafe extern "C" fn(u32, *mut u64) -> c_int;
type Processes = unsafe extern "C" fn(*mut ProcessInfo, *mut u32) -> c_int;
type ProcessGpus = unsafe extern "C" fn(u32, *mut u32, *mut u32) -> c_int;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessInfo {
    process_id: u32,
    pasid: u32,
    vram_usage: u64,
    sdma_usage: u64,
    cu_occupancy: u32,
}

struct Library(*mut c_void);
impl Library {
    fn open(path: &str) -> Result<Self> {
        let path = CString::new(path)?;
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        ensure!(
            !handle.is_null(),
            "cannot load HCU SMI {}: {}",
            path.to_string_lossy(),
            loader_error()
        );
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

pub struct Smi {
    // Keep the library alive until after shutdown and the last function call.
    _library: Library,
    shutdown: Shutdown,
    count: Count,
    unique_id: UniqueId,
    processes: Processes,
    process_gpus: ProcessGpus,
}
impl Smi {
    pub fn load() -> Result<Self> {
        let library = if let Ok(path) = std::env::var("XLURM_HCU_SMI_LIBRARY") {
            Library::open(&path)?
        } else {
            Library::open("/opt/hyhal/lib/librocm_smi64.so")
                .or_else(|_| Library::open("/opt/dtk/.hyhal/rocm_smi/lib/librocm_smi64.so"))
                .or_else(|_| Library::open("librocm_smi64.so"))?
        };
        // Exact C prototypes from the vendor header, including process struct layout.
        unsafe {
            let init: Init = std::mem::transmute(library.symbol(c"rsmi_init")?);
            let shutdown =
                std::mem::transmute::<*mut c_void, Shutdown>(library.symbol(c"rsmi_shut_down")?);
            let count = std::mem::transmute::<*mut c_void, Count>(
                library.symbol(c"rsmi_num_monitor_devices")?,
            );
            let unique_id = std::mem::transmute::<*mut c_void, UniqueId>(
                library.symbol(c"rsmi_dev_unique_id_get")?,
            );
            let processes = std::mem::transmute::<*mut c_void, Processes>(
                library.symbol(c"rsmi_compute_process_info_get")?,
            );
            let process_gpus = std::mem::transmute::<*mut c_void, ProcessGpus>(
                library.symbol(c"rsmi_compute_process_gpus_get")?,
            );
            check(init(0), "rsmi_init")?;
            Ok(Self {
                _library: library,
                shutdown,
                count,
                unique_id,
                processes,
                process_gpus,
            })
        }
    }

    fn device_count(&self) -> Result<u32> {
        let mut count = 0;
        check(
            unsafe { (self.count)(&mut count) },
            "rsmi_num_monitor_devices",
        )?;
        ensure!(
            (1..=64).contains(&count),
            "invalid HCU device count {count}"
        );
        Ok(count)
    }

    pub fn discover(&self) -> Result<Vec<Device>> {
        let mut devices = Vec::new();
        for id in 0..self.device_count()? {
            let mut uid = 0;
            check(
                unsafe { (self.unique_id)(id, &mut uid) },
                "rsmi_dev_unique_id_get",
            )?;
            ensure!(
                uid != 0 && uid != u64::MAX,
                "invalid HCU unique ID for device {id}"
            );
            // Match hy-smi exactly so persisted allocation keys survive upgrades.
            devices.push(Device {
                kind: Kind::Hcu,
                id,
                name: "HCU".into(),
                visible: format!("{uid:016x}"),
                npu: None,
                chip: None,
            });
        }
        super::validate(devices)
    }

    pub fn busy_devices(&self) -> Result<HashSet<u32>> {
        let device_count = self.device_count()?;
        // Fixed bounded buffers avoid count/fill races and unbounded driver allocations.
        // Insufficient space or a process disappearing during lookup fails closed.
        let mut processes = vec![ProcessInfo::default(); 4096];
        let mut count = processes.len() as u32;
        check(
            unsafe { (self.processes)(processes.as_mut_ptr(), &mut count) },
            "rsmi_compute_process_info_get",
        )?;
        ensure!(
            count as usize <= processes.len(),
            "invalid HCU process count {count}"
        );
        let mut busy = HashSet::new();
        for process in &processes[..count as usize] {
            ensure!(process.process_id > 0, "invalid HCU process ID");
            let mut ids = [u32::MAX; 64];
            let mut count = ids.len() as u32;
            check(
                unsafe { (self.process_gpus)(process.process_id, ids.as_mut_ptr(), &mut count) },
                "rsmi_compute_process_gpus_get",
            )?;
            ensure!(
                (1..=ids.len() as u32).contains(&count),
                "invalid HCU process device count {count}"
            );
            for &id in &ids[..count as usize] {
                ensure!(id < device_count, "invalid HCU process device index {id}");
                busy.insert(id);
            }
        }
        Ok(busy)
    }
}
impl Drop for Smi {
    fn drop(&mut self) {
        unsafe { (self.shutdown)() };
    }
}
fn check(code: c_int, operation: &str) -> Result<()> {
    ensure!(code == 0, "{operation} failed with HCU SMI error {code}");
    Ok(())
}

pub fn health_tool() -> PathBuf {
    if let Some(path) = std::env::var_os("XLURM_HCU_HEALTH_TOOL") {
        return path.into();
    }
    let dtk = PathBuf::from("/opt/dtk/.hyhal/bin/hy-smi");
    if dtk.is_file() {
        dtk
    } else {
        super::locate("hy-smi", &["/opt/hyhal/bin/hy-smi"])
    }
}
