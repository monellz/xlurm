//! Read-only MXSML bindings, matching the driver's MxSmlExtension.h and MxSml.h.
use crate::model::{Device, Kind};
use anyhow::{Context, Result, ensure};
use libc::{c_char, c_int, c_uint, c_void};
use std::ffi::{CStr, CString};

type Init = unsafe extern "C" fn() -> c_uint;
type Count = unsafe extern "C" fn(*mut c_uint) -> c_uint;
type Handle = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_uint;
type Text = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_uint;
type Processes = unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut ProcessInfo) -> c_uint;
type Availability = unsafe extern "C" fn(c_uint, *mut UnavailableReason) -> c_uint;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessInfo {
    compute_instance_id: c_uint,
    gpu_instance_id: c_uint,
    pid: c_uint,
    used_gpu_memory: u64,
}

#[repr(C)]
struct UnavailableReason {
    code: c_int,
    reason: [c_char; 64],
}

struct Library(*mut c_void);
impl Library {
    fn open(path: &str) -> Result<Self> {
        let path = CString::new(path)?;
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        ensure!(
            !handle.is_null(),
            "cannot load MetaX MXSML {}: {}",
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

pub struct Mxsml {
    // All function pointers remain valid through shutdown.
    _library: Library,
    shutdown: Init,
    count: Count,
    handle: Handle,
    uuid: Text,
    name: Text,
    processes: Processes,
    availability: Availability,
}
impl Mxsml {
    pub fn load() -> Result<Self> {
        let library = if let Ok(path) = std::env::var("XLURM_MXSML_LIBRARY") {
            Library::open(&path)?
        } else {
            Library::open("/opt/mxdriver/lib/libmxsml.so")
                .or_else(|_| Library::open("/opt/maca/lib/libmxsml.so"))
                .or_else(|_| Library::open("/opt/mxn100/lib/libmxsml.so"))
                .or_else(|_| Library::open("libmxsml.so"))?
        };
        // Each function uses its declared C prototype; the extension process
        // layout differs from both NVML and the original MXSML process layout.
        unsafe {
            let init: Init = std::mem::transmute(library.symbol(c"mxSmlExInit")?);
            let shutdown =
                std::mem::transmute::<*mut c_void, Init>(library.symbol(c"mxSmlExShutdown")?);
            let count = std::mem::transmute::<*mut c_void, Count>(
                library.symbol(c"mxSmlExDeviceGetCount")?,
            );
            let handle = std::mem::transmute::<*mut c_void, Handle>(
                library.symbol(c"mxSmlExGetDeviceHandleByIndex")?,
            );
            let uuid =
                std::mem::transmute::<*mut c_void, Text>(library.symbol(c"mxSmlExDeviceGetUUID")?);
            let name =
                std::mem::transmute::<*mut c_void, Text>(library.symbol(c"mxSmlExDeviceGetName")?);
            let processes = std::mem::transmute::<*mut c_void, Processes>(
                library.symbol(c"mxSmlExDeviceGetComputeRunningProcesses")?,
            );
            let availability = std::mem::transmute::<*mut c_void, Availability>(
                library.symbol(c"mxSmlGetDeviceUnavailableReason")?,
            );
            check(init(), "mxSmlExInit")?;
            Ok(Self {
                _library: library,
                shutdown,
                count,
                handle,
                uuid,
                name,
                processes,
                availability,
            })
        }
    }

    fn device_handle(&self, id: u32) -> Result<*mut c_void> {
        let mut handle = std::ptr::null_mut();
        check(
            unsafe { (self.handle)(id, &mut handle) },
            "mxSmlExGetDeviceHandleByIndex",
        )?;
        ensure!(!handle.is_null(), "null MetaX device handle for index {id}");
        Ok(handle)
    }

    fn text(&self, handle: *mut c_void, function: Text, operation: &str) -> Result<String> {
        // The UUID ABI requires 80 bytes; the name ABI accepts an explicit size.
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
        let uuid = self.text(handle, self.uuid, "mxSmlExDeviceGetUUID")?;
        ensure!(
            uuid.starts_with("GPU-") && !uuid.contains(char::is_whitespace),
            "invalid MetaX UUID {uuid}"
        );
        Ok(uuid)
    }

    pub fn discover(&self) -> Result<Vec<Device>> {
        let mut count = 0;
        check(unsafe { (self.count)(&mut count) }, "mxSmlExDeviceGetCount")?;
        ensure!(
            (1..=64).contains(&count),
            "invalid MetaX device count {count}"
        );
        let mut devices = Vec::new();
        for id in 0..count {
            let handle = self.device_handle(id)?;
            devices.push(Device {
                kind: Kind::Metax,
                id,
                // Preserve mx-smi UUIDs for existing state and worker adoption.
                visible: self.uuid(handle)?,
                name: self.text(handle, self.name, "mxSmlExDeviceGetName")?,
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
            "MetaX UUID changed for index {}",
            device.id
        );
        let mut reason = UnavailableReason {
            code: -1,
            reason: [0; 64],
        };
        check(
            unsafe { (self.availability)(device.id, &mut reason) },
            "mxSmlGetDeviceUnavailableReason",
        )?;
        ensure!(reason.code >= 0, "missing MetaX availability status");
        if reason.code != 0 {
            return Ok("unhealthy");
        }
        // A single bounded call avoids a count/fill race. Insufficient capacity,
        // invalid counts, or malformed entries are unknown, never idle.
        let mut processes = vec![ProcessInfo::default(); 4096];
        let mut count = processes.len() as c_uint;
        check(
            unsafe { (self.processes)(handle, &mut count, processes.as_mut_ptr()) },
            "mxSmlExDeviceGetComputeRunningProcesses",
        )?;
        ensure!(
            count as usize <= processes.len(),
            "invalid MetaX process count {count}"
        );
        ensure!(
            processes[..count as usize].iter().all(|p| p.pid > 0),
            "invalid MetaX process ID"
        );
        Ok(if count == 0 { "idle" } else { "busy" })
    }
}
impl Drop for Mxsml {
    fn drop(&mut self) {
        unsafe { (self.shutdown)() };
    }
}
fn check(code: c_uint, operation: &str) -> Result<()> {
    ensure!(code == 0, "{operation} failed with MXSML error {code}");
    Ok(())
}
