//! Read-only HGML v2 bindings, matching the PPU SDK's hgml.h.
use crate::model::{Device, Kind};
use anyhow::{Context, Result, ensure};
use libc::{c_char, c_uint, c_void};
use std::ffi::{CStr, CString};

type Init = unsafe extern "C" fn() -> c_uint;
type Count = unsafe extern "C" fn(*mut c_uint) -> c_uint;
type Handle = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_uint;
type Text = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_uint;
type Processes = unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut ProcessInfo) -> c_uint;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessInfo {
    pid: c_uint,
    used_gpu_memory: u64,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

struct Library(*mut c_void);
impl Library {
    fn open(path: &str) -> Result<Self> {
        let path = CString::new(path)?;
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        ensure!(
            !handle.is_null(),
            "cannot load PPU HGML {}: {}",
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

pub struct Hgml {
    // All function pointers remain valid through shutdown.
    _library: Library,
    shutdown: Init,
    count: Count,
    handle: Handle,
    uuid: Text,
    name: Text,
    processes: Processes,
}
impl Hgml {
    pub fn load() -> Result<Self> {
        let library = if let Ok(path) = std::env::var("XLURM_HGML_LIBRARY") {
            Library::open(&path)?
        } else {
            Library::open("/usr/local/PPU_SDK/lib/libhgml.so")
                .or_else(|_| {
                    Library::open("/usr/local/PPU_SDK/targets/x86_64-linux/lib/libhgml.so")
                })
                .or_else(|_| Library::open("libhgml.so"))?
        };
        // Use versioned v2 entry points and their exact hgml.h process layout.
        unsafe {
            let init: Init = std::mem::transmute(library.symbol(c"hgmlInit_v2")?);
            let shutdown =
                std::mem::transmute::<*mut c_void, Init>(library.symbol(c"hgmlShutdown")?);
            let count = std::mem::transmute::<*mut c_void, Count>(
                library.symbol(c"hgmlDeviceGetCount_v2")?,
            );
            let handle = std::mem::transmute::<*mut c_void, Handle>(
                library.symbol(c"hgmlDeviceGetHandleByIndex_v2")?,
            );
            let uuid =
                std::mem::transmute::<*mut c_void, Text>(library.symbol(c"hgmlDeviceGetUUID")?);
            let name =
                std::mem::transmute::<*mut c_void, Text>(library.symbol(c"hgmlDeviceGetName")?);
            let processes = std::mem::transmute::<*mut c_void, Processes>(
                library.symbol(c"hgmlDeviceGetComputeRunningProcesses_v2")?,
            );
            check(init(), "hgmlInit_v2")?;
            Ok(Self {
                _library: library,
                shutdown,
                count,
                handle,
                uuid,
                name,
                processes,
            })
        }
    }

    fn device_handle(&self, id: u32) -> Result<*mut c_void> {
        let mut handle = std::ptr::null_mut();
        check(
            unsafe { (self.handle)(id, &mut handle) },
            "hgmlDeviceGetHandleByIndex_v2",
        )?;
        ensure!(!handle.is_null(), "null PPU device handle for index {id}");
        Ok(handle)
    }

    fn text(&self, handle: *mut c_void, function: Text, operation: &str) -> Result<String> {
        // The SDK guarantees 96 bytes for UUIDs and 96 bytes for device names.
        let mut bytes = [0u8; 128];
        check(
            unsafe { function(handle, bytes.as_mut_ptr().cast(), bytes.len() as c_uint) },
            operation,
        )?;
        let text = CStr::from_bytes_until_nul(&bytes)
            .with_context(|| format!("unterminated {operation} response"))?
            .to_str()
            .with_context(|| format!("invalid UTF-8 from {operation}"))?;
        ensure!(!text.is_empty(), "empty {operation} response");
        Ok(text.into())
    }

    fn uuid(&self, handle: *mut c_void) -> Result<String> {
        let uuid = self.text(handle, self.uuid, "hgmlDeviceGetUUID")?;
        ensure!(
            (uuid.starts_with("GPU-") || uuid.starts_with("PPU-"))
                && !uuid.contains(char::is_whitespace),
            "invalid PPU UUID {uuid}"
        );
        Ok(uuid)
    }

    pub fn discover(&self) -> Result<Vec<Device>> {
        let mut count = 0;
        check(unsafe { (self.count)(&mut count) }, "hgmlDeviceGetCount_v2")?;
        ensure!(
            (1..=64).contains(&count),
            "invalid PPU device count {count}"
        );
        let mut devices = Vec::new();
        for id in 0..count {
            let handle = self.device_handle(id)?;
            devices.push(Device {
                kind: Kind::Ppu,
                id,
                // Preserve ppu-smi UUIDs for existing state and worker adoption.
                visible: self.uuid(handle)?,
                name: self.text(handle, self.name, "hgmlDeviceGetName")?,
                npu: None,
                chip: None,
            });
        }
        super::validate(devices)
    }

    pub fn status(&self, device: &Device) -> &'static str {
        self.query_status(device).unwrap_or("unknown")
    }

    fn query_status(&self, device: &Device) -> Result<&'static str> {
        let handle = self.device_handle(device.id)?;
        // Fail closed if an index now names another device after a driver reset.
        ensure!(
            self.uuid(handle)? == device.visible,
            "PPU UUID changed for index {}",
            device.id
        );
        // A single bounded call avoids a count/fill race. Insufficient capacity,
        // invalid counts, or malformed entries are unknown, never idle.
        let mut processes = vec![ProcessInfo::default(); 4096];
        let mut count = processes.len() as c_uint;
        check(
            unsafe { (self.processes)(handle, &mut count, processes.as_mut_ptr()) },
            "hgmlDeviceGetComputeRunningProcesses_v2",
        )?;
        ensure!(
            count as usize <= processes.len(),
            "invalid PPU process count {count}"
        );
        ensure!(
            processes[..count as usize].iter().all(|p| p.pid > 0),
            "invalid PPU process ID"
        );
        Ok(if count == 0 { "idle" } else { "busy" })
    }
}
impl Drop for Hgml {
    fn drop(&mut self) {
        unsafe { (self.shutdown)() };
    }
}
fn check(code: c_uint, operation: &str) -> Result<()> {
    ensure!(code == 0, "{operation} failed with HGML error {code}");
    Ok(())
}
