use std::sync::OnceLock;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, ERROR_SERVICE_DOES_NOT_EXIST, HANDLE},
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Services::{
                CloseServiceHandle, CreateServiceW, OpenSCManagerW, OpenServiceW,
                QueryServiceStatus, SERVICE_DEMAND_START,
                SERVICE_KERNEL_DRIVER, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
                SERVICE_STATUS, StartServiceW,
                SC_MANAGER_ALL_ACCESS,
            },
        },
    },
};

const DRIVER_DEVICE: &str = "\\\\.\\WinRing0_1_2_0";
const DRIVER_SERVICE: &str = "WinRing0_1_2_0";

// LibreHardwareMonitor WinRing0 fork ioctls:
// CTL_CODE(OLS_TYPE=40000, func, METHOD_BUFFERED, access)
const fn ols_ctl(func: u32, access: u32) -> u32 {
    (40000u32 << 16) | (access << 14) | (func << 2)
}
const IOCTL_OLS_GET_DRIVER_VERSION: u32 = ols_ctl(0x800, 0); // Any
const IOCTL_OLS_READ_IO_PORT_BYTE: u32 = ols_ctl(0x833, 1); // FILE_READ_ACCESS  -> 0x9C4040CC
const IOCTL_OLS_WRITE_IO_PORT_BYTE: u32 = ols_ctl(0x836, 2); // FILE_WRITE_ACCESS -> 0x9C4080D8

#[repr(C, packed)]
struct WriteIoPortInput {
    port_number: u32,
    value: u8,
}

/// Port-I/O backend. PawnIO is preferred: modern, signed, HVCI-compatible, and its
/// modules are sandboxed to the ports they discovered. WinRing0 is the legacy
/// fallback for machines without PawnIO installed.
pub struct Ring0 {
    backend: Backend,
}

enum Backend {
    PawnIo(crate::pawnio::PawnIo),
    WinRing0(WinRing0),
}

struct WinRing0 {
    handle: SendHandle,
}

/// HANDLE wrapper that is Send+Sync (the WinRing0 handle is used from one thread at
/// a time via the global OnceLock; DeviceIoControl on it is internally synchronized
/// by the kernel).
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

static RING0: OnceLock<Result<Ring0, String>> = OnceLock::new();
/// Why PawnIO was not used, when we fell back. Reported by `--check`.
static PAWNIO_ERROR: OnceLock<String> = OnceLock::new();

pub fn get() -> Result<&'static Ring0, String> {
    let r = RING0.get_or_init(|| match crate::pawnio::PawnIo::open() {
        Ok(p) => Ok(Ring0 { backend: Backend::PawnIo(p) }),
        Err(e) => {
            let _ = PAWNIO_ERROR.set(e);
            WinRing0::new().map(|w| Ring0 { backend: Backend::WinRing0(w) })
        }
    });
    match r {
        Ok(r) => Ok(r),
        Err(e) => Err(e.clone()),
    }
}

/// Name of the active backend, for the UI status line. Call after `get()`.
pub fn backend_name() -> &'static str {
    match RING0.get() {
        Some(Ok(Ring0 { backend: Backend::PawnIo(_) })) => "PawnIO",
        Some(Ok(Ring0 { backend: Backend::WinRing0(_) })) => "WinRing0",
        _ => "none",
    }
}

/// Set when PawnIO was unavailable and we fell back to WinRing0.
pub fn pawnio_error() -> Option<&'static String> {
    PAWNIO_ERROR.get()
}

impl Ring0 {
    pub fn read_port_byte(&self, port: u16) -> u8 {
        match &self.backend {
            Backend::PawnIo(p) => p.read_port_byte(port),
            Backend::WinRing0(w) => w.read_port_byte(port),
        }
    }

    pub fn write_port_byte(&self, port: u16, value: u8) {
        match &self.backend {
            Backend::PawnIo(p) => p.write_port_byte(port, value),
            Backend::WinRing0(w) => w.write_port_byte(port, value),
        }
    }

    /// Backend version string; also a liveness check on the handle.
    pub fn version_string(&self) -> String {
        match &self.backend {
            Backend::PawnIo(p) => p.version_string(),
            Backend::WinRing0(w) => w
                .driver_version()
                .map(|v| format!("{v:#010X}"))
                .unwrap_or_else(|| "unknown".into()),
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl WinRing0 {
    fn new() -> Result<WinRing0, String> {
        unsafe {
            Self::install_driver()?;

            let name = wide(DRIVER_DEVICE);
            let handle = CreateFileW(
                PCWSTR(name.as_ptr()),
                0x8000_0000u32 | 0x4000_0000u32, // GENERIC_READ | GENERIC_WRITE
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(|e| format!("CreateFile({DRIVER_DEVICE}) failed: {e}"))?;

            Ok(WinRing0 { handle: SendHandle(handle) })
        }
    }

    fn install_driver() -> Result<(), String> {
        unsafe {
            let sys_path = find_sys_file()?;
            if !sys_path.exists() {
                return Err(format!("driver file not found at {}", sys_path.display()));
            }
            let path_wide = wide(&sys_path.to_string_lossy());

            let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
                .map_err(|e| format!("OpenSCManager failed: {e}"))?;

            const SVC_ACCESS: u32 = SERVICE_QUERY_STATUS | 0x0002 | 0x0010 | 0x0020 | 0x10000; // query|change_config|start|stop|delete

            let svc_name = wide(DRIVER_SERVICE);
            let service = match OpenServiceW(scm, PCWSTR(svc_name.as_ptr()), SVC_ACCESS) {
                Ok(s) => s,
                Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
                    let display = wide("WinRing0 Driver");
                    CreateServiceW(
                        scm,
                        PCWSTR(svc_name.as_ptr()),
                        PCWSTR(display.as_ptr()),
                        SVC_ACCESS,
                        SERVICE_KERNEL_DRIVER,
                        SERVICE_DEMAND_START,
                        windows::Win32::System::Services::SERVICE_ERROR_NORMAL,
                        PCWSTR(path_wide.as_ptr()),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .map_err(|e| format!("CreateService failed: {e}"))?
                }
                Err(e) => return Err(format!("OpenService failed: {e}")),
            };

            let mut status = SERVICE_STATUS::default();
            if QueryServiceStatus(service, &mut status).is_ok()
                && status.dwCurrentState != SERVICE_RUNNING
            {
                StartServiceW(service, None)
                    .map_err(|e| format!("StartService failed: {e}"))?;
            }

            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
            Ok(())
        }
    }

    fn read_port_byte(&self, port: u16) -> u8 {
        unsafe {
            let input = port as u32;
            let mut out: u32 = 0;
            let mut ret = 0u32;
            let ok = DeviceIoControl(
                self.handle.0,
                IOCTL_OLS_READ_IO_PORT_BYTE,
                Some(&input as *const _ as *const _),
                std::mem::size_of::<u32>() as u32,
                Some(&mut out as *mut _ as *mut _),
                std::mem::size_of::<u32>() as u32,
                Some(&mut ret),
                None,
            );
            if ok.is_ok() { (out & 0xFF) as u8 } else { 0 }
        }
    }

    fn write_port_byte(&self, port: u16, value: u8) {
        unsafe {
            let input = WriteIoPortInput {
                port_number: port as u32,
                value,
            };
            let mut ret = 0u32;
            let _ = DeviceIoControl(
                self.handle.0,
                IOCTL_OLS_WRITE_IO_PORT_BYTE,
                Some(&input as *const _ as *const _),
                std::mem::size_of::<WriteIoPortInput>() as u32,
                None,
                0,
                Some(&mut ret),
                None,
            );
        }
    }

    /// Driver version sanity check (validates the handle works).
    fn driver_version(&self) -> Option<u32> {
        unsafe {
            let mut version: u32 = 0;
            let mut ret = 0u32;
            let ok = DeviceIoControl(
                self.handle.0,
                IOCTL_OLS_GET_DRIVER_VERSION,
                None,
                0,
                Some(&mut version as *mut _ as *mut _),
                4,
                Some(&mut ret),
                None,
            );
            ok.map(|_| version).ok()
        }
    }
}

impl Drop for WinRing0 {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle.0);
        }
    }
}

fn find_sys_file() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().unwrap();
    let candidates = [
        dir.join("WinRing0x64.sys"),
        dir.join("assets").join("WinRing0x64.sys"),
        dir.parent()
            .map(|d| d.join("assets").join("WinRing0x64.sys"))
            .unwrap_or_default(),
        std::path::PathBuf::from("assets\\WinRing0x64.sys"),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "WinRing0x64.sys not found; looked in {:?}",
        candidates
    ))
}
