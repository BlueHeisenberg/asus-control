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
                QueryServiceStatus, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
                SERVICE_KERNEL_DRIVER, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STOPPED,
                SERVICE_STATUS, ControlService, DeleteService, StartServiceW,
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

pub struct Ring0 {
    handle: SendHandle,
}

/// HANDLE wrapper that is Send+Sync (WinRing0 handle is used from one thread at a
/// time via the global OnceLock; DeviceIoControl on it is internally synchronized
/// by the kernel).
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

static RING0: OnceLock<Result<Ring0, String>> = OnceLock::new();

pub fn get() -> Result<&'static Ring0, String> {
    let r = RING0.get_or_init(|| Ring0::new());
    match r {
        Ok(r) => Ok(r),
        Err(e) => Err(e.clone()),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Ring0 {
    fn new() -> Result<Ring0, String> {
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

            Ok(Ring0 { handle: SendHandle(handle) })
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

    pub fn uninstall_driver() {
        unsafe {
            let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS) else { return };
            let svc_name = wide(DRIVER_SERVICE);
            let Ok(service) =
                OpenServiceW(scm, PCWSTR(svc_name.as_ptr()), SERVICE_QUERY_STATUS | 0x0020 | 0x10000)
            else {
                let _ = CloseServiceHandle(scm);
                return;
            };

            let mut status = SERVICE_STATUS::default();
            if QueryServiceStatus(service, &mut status).is_ok()
                && status.dwCurrentState != SERVICE_STOPPED
            {
                let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut status);
            }
            let _ = DeleteService(service);
            let _ = CloseServiceHandle(service);
            let _ = CloseServiceHandle(scm);
        }
    }

    pub fn read_port_byte(&self, port: u16) -> u8 {
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

    /// Debug variant returning raw Result info
    pub fn read_port_byte_dbg(&self, port: u16) -> (Option<u32>, String) {
        unsafe {
            let input = port as u32;
            let mut out: u32 = 0;
            let mut ret = 0u32;
            match DeviceIoControl(
                self.handle.0,
                IOCTL_OLS_READ_IO_PORT_BYTE,
                Some(&input as *const _ as *const _),
                std::mem::size_of::<u32>() as u32,
                Some(&mut out as *mut _ as *mut _),
                std::mem::size_of::<u32>() as u32,
                Some(&mut ret),
                None,
            ) {
                Ok(_) => {
                    if ret != 4 {
                        (Some(out), format!("ok but bytesReturned={ret}"))
                    } else {
                        (Some(out), "ok".into())
                    }
                }
                Err(e) => (None, format!("ERR {e}")),
            }
        }
    }

    pub fn write_port_byte(&self, port: u16, value: u8) {
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
    pub fn driver_version(&self) -> Option<u32> {
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

impl Drop for Ring0 {
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


