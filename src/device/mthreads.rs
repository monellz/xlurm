//! Read-only MTML bindings for Moore Threads devices.
use crate::model::{Device, Kind};
use anyhow::{Context, Result, ensure};
use libc::{c_char, c_uint, c_void};
use std::ffi::{CStr, CString};

type LibraryInit = unsafe extern "C" fn(*mut *mut c_void) -> c_uint;
type LibraryShutdown = unsafe extern "C" fn(*mut c_void) -> c_uint;
type Count = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_uint;
type DeviceInit = unsafe extern "C" fn(*mut c_void, c_uint, *mut *mut c_void) -> c_uint;
type Text = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_uint;
type ProcessCount = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_uint;

struct Library(*mut c_void);
impl Library {
    fn open(path: &str) -> Result<Self> {
        let path = CString::new(path)?;
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        ensure!(
            !handle.is_null(),
            "cannot load MTML {}: {}",
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
        unsafe {
            libc::dlclose(self.0);
        }
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
fn check(code: c_uint, operation: &str) -> Result<()> {
    ensure!(code == 0, "{operation} failed with MTML error {code}");
    Ok(())
}

pub struct Mtml {
    _library: Library,
    shutdown: LibraryShutdown,
    library: *mut c_void,
    count: Count,
    device: DeviceInit,
    name: Text,
    uuid: Text,
    process_count: ProcessCount,
}
unsafe impl Send for Mtml {}
impl Mtml {
    pub fn load() -> Result<Self> {
        let library = if let Ok(path) = std::env::var("XLURM_MTML_LIBRARY") {
            Library::open(&path)?
        } else {
            Library::open("/usr/lib/libmtml.so").or_else(|_| Library::open("libmtml.so"))?
        };
        unsafe {
            let init: LibraryInit = std::mem::transmute(library.symbol(c"mtmlLibraryInit")?);
            let shutdown: LibraryShutdown =
                std::mem::transmute(library.symbol(c"mtmlLibraryShutDown")?);
            let count: Count = std::mem::transmute(library.symbol(c"mtmlLibraryCountDevice")?);
            let device: DeviceInit =
                std::mem::transmute(library.symbol(c"mtmlLibraryInitDeviceByIndex")?);
            let name: Text = std::mem::transmute(library.symbol(c"mtmlDeviceGetName")?);
            let uuid: Text = std::mem::transmute(library.symbol(c"mtmlDeviceGetUUID")?);
            let process_count: ProcessCount =
                std::mem::transmute(library.symbol(c"mtmlDeviceCountProcessMemoryUsages")?);
            let mut handle = std::ptr::null_mut();
            check(init(&mut handle), "mtmlLibraryInit")?;
            ensure!(!handle.is_null(), "null MTML library handle");
            Ok(Self {
                _library: library,
                shutdown,
                library: handle,
                count,
                device,
                name,
                uuid,
                process_count,
            })
        }
    }
    fn handle(&self, id: u32) -> Result<*mut c_void> {
        let mut handle = std::ptr::null_mut();
        check(
            unsafe { (self.device)(self.library, id, &mut handle) },
            "mtmlLibraryInitDeviceByIndex",
        )?;
        ensure!(!handle.is_null(), "null MTML device handle for index {id}");
        Ok(handle)
    }
    fn text(&self, handle: *mut c_void, function: Text, operation: &str) -> Result<String> {
        let mut bytes = [0u8; 128];
        check(
            unsafe { function(handle, bytes.as_mut_ptr().cast(), bytes.len() as c_uint) },
            operation,
        )?;
        let value = CStr::from_bytes_until_nul(&bytes)
            .with_context(|| format!("unterminated {operation} response"))?
            .to_str()
            .with_context(|| format!("invalid UTF-8 from {operation}"))?;
        ensure!(!value.is_empty(), "empty {operation} response");
        Ok(value.into())
    }
    pub fn discover(&self) -> Result<Vec<Device>> {
        let mut count = 0;
        check(
            unsafe { (self.count)(self.library, &mut count) },
            "mtmlLibraryCountDevice",
        )?;
        ensure!(
            (1..=64).contains(&count),
            "invalid MTML device count {count}"
        );
        let mut devices = Vec::new();
        for id in 0..count {
            let handle = self.handle(id)?;
            devices.push(Device {
                kind: Kind::Mthreads,
                id,
                visible: self.text(handle, self.uuid, "mtmlDeviceGetUUID")?,
                name: self.text(handle, self.name, "mtmlDeviceGetName")?,
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
        let handle = self.handle(device.id)?;
        ensure!(
            self.text(handle, self.uuid, "mtmlDeviceGetUUID")? == device.visible,
            "MTML UUID changed for index {}",
            device.id
        );
        let mut count = 0;
        check(
            unsafe { (self.process_count)(handle, &mut count) },
            "mtmlDeviceCountProcessMemoryUsages",
        )?;
        ensure!(count <= 4096, "invalid MTML process count {count}");
        Ok(if count == 0 { "idle" } else { "busy" })
    }
}
impl Drop for Mtml {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)(self.library);
        }
    }
}
