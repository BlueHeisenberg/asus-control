//! Stop/disable the ASUS background stack (Armoury Crate & friends).
//!
//! Why this exists: `AsusFanControlService` drives the same NCT6798D PWM
//! registers we do, so with it running the two of us fight over every fan.
//!
//! Only `SERVICE_WIN32` services are ever touched. That filter is the safety
//! property — it excludes kernel drivers, so `Asusgio3` (AsIO3.sys) and any
//! other boot/system-start driver is out of reach of this code by construction.
//! Every change records the previous start type so it can be put back exactly.

use std::ffi::c_void;
use windows::{
    core::PCWSTR,
    Win32::System::Services::*,
};

/// A service is ours to manage if its name starts with one of these.
const PREFIXES: [&str; 3] = ["asus", "armoury", "rog"];

#[derive(Clone)]
pub struct Svc {
    pub name: String,
    pub display: String,
    pub running: bool,
    pub start_type: u32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn is_asus(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    PREFIXES.iter().any(|p| n.starts_with(p))
}

struct Scm(SC_HANDLE);
impl Drop for Scm {
    fn drop(&mut self) {
        unsafe { let _ = CloseServiceHandle(self.0); }
    }
}

fn open_scm(access: u32) -> Result<Scm, String> {
    unsafe {
        OpenSCManagerW(None, None, access)
            .map(Scm)
            .map_err(|e| format!("OpenSCManager: {e}"))
    }
}

/// Every ASUS user-mode service currently registered.
pub fn list() -> Vec<Svc> {
    let Ok(scm) = open_scm(SC_MANAGER_ENUMERATE_SERVICE | SC_MANAGER_CONNECT) else {
        return Vec::new();
    };
    unsafe {
        let mut needed = 0u32;
        let mut count = 0u32;
        // first call sizes the buffer
        let _ = EnumServicesStatusExW(
            scm.0, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL,
            None, &mut needed, &mut count, None, PCWSTR::null(),
        );
        if needed == 0 {
            return Vec::new();
        }
        // back the byte buffer with u64s: the API writes an array of structs
        // containing pointers, and casting a 1-aligned Vec<u8> to that is UB
        let mut backing = vec![0u64; (needed as usize + 7) / 8];
        let buf = std::slice::from_raw_parts_mut(backing.as_mut_ptr() as *mut u8, backing.len() * 8);
        if EnumServicesStatusExW(
            scm.0, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL,
            Some(buf), &mut needed, &mut count, None, PCWSTR::null(),
        )
        .is_err()
        {
            return Vec::new();
        }

        let items = std::slice::from_raw_parts(
            backing.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW,
            count as usize,
        );
        items
            .iter()
            .filter_map(|it| {
                let name = it.lpServiceName.to_string().ok()?;
                if !is_asus(&name) {
                    return None;
                }
                Some(Svc {
                    display: it.lpDisplayName.to_string().unwrap_or_else(|_| name.clone()),
                    running: it.ServiceStatusProcess.dwCurrentState != SERVICE_STOPPED,
                    start_type: query_start_type(&name).unwrap_or(SERVICE_AUTO_START.0),
                    name,
                })
            })
            .collect()
    }
}

fn query_start_type(name: &str) -> Option<u32> {
    let scm = open_scm(SC_MANAGER_CONNECT).ok()?;
    unsafe {
        let svc = OpenServiceW(scm.0, PCWSTR(wide(name).as_ptr()), SERVICE_QUERY_CONFIG).ok()?;
        let mut needed = 0u32;
        let _ = QueryServiceConfigW(svc, None, 0, &mut needed);
        let mut backing = vec![0u64; (needed as usize + 7) / 8 + 1];
        let cfg = backing.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        let ok = QueryServiceConfigW(svc, Some(cfg), needed, &mut needed).is_ok();
        let st = ok.then(|| (*cfg).dwStartType.0);
        let _ = CloseServiceHandle(svc);
        st
    }
}

fn set_start_type(name: &str, start: u32) -> Result<(), String> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    unsafe {
        let svc = OpenServiceW(scm.0, PCWSTR(wide(name).as_ptr()), SERVICE_CHANGE_CONFIG)
            .map_err(|e| format!("{name}: open: {e}"))?;
        let r = ChangeServiceConfigW(
            svc,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            SERVICE_START_TYPE(start),
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
        .map_err(|e| format!("{name}: config: {e}"));
        let _ = CloseServiceHandle(svc);
        r
    }
}

fn stop(name: &str) -> Result<(), String> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    unsafe {
        let svc = OpenServiceW(
            scm.0,
            PCWSTR(wide(name).as_ptr()),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        )
        .map_err(|e| format!("{name}: open: {e}"))?;
        let mut st = SERVICE_STATUS::default();
        let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut st);
        // give it a moment to actually leave RUNNING
        for _ in 0..20 {
            if QueryServiceStatus(svc, &mut st).is_err() || st.dwCurrentState == SERVICE_STOPPED {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = CloseServiceHandle(svc);
        Ok(())
    }
}

fn start(name: &str) -> Result<(), String> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    unsafe {
        let svc = OpenServiceW(scm.0, PCWSTR(wide(name).as_ptr()), SERVICE_START)
            .map_err(|e| format!("{name}: open: {e}"))?;
        let r = StartServiceW(svc, None).map_err(|e| format!("{name}: start: {e}"));
        let _ = CloseServiceHandle(svc);
        r
    }
}

/// Stop every ASUS service and mark it disabled.
/// Returns `(name, previous_start_type)` for each one changed — persist this,
/// it is the only way back.
pub fn disable_all() -> (Vec<(String, u32)>, String) {
    let mut backup = Vec::new();
    let mut failed = 0;
    let all = list();
    for s in &all {
        if s.running {
            let _ = stop(&s.name);
        }
        match set_start_type(&s.name, SERVICE_DISABLED.0) {
            Ok(()) => backup.push((s.name.clone(), s.start_type)),
            Err(_) => failed += 1,
        }
    }
    let msg = if failed == 0 {
        format!("ASUS services disabled ({})", backup.len())
    } else {
        format!("ASUS services disabled ({}/{}, {failed} failed)", backup.len(), all.len())
    };
    (backup, msg)
}

/// Put every service back to the start type it had, and restart the ones
/// that were set to start automatically.
pub fn restore(backup: &[(String, u32)]) -> String {
    let mut failed = 0;
    for (name, start_type) in backup {
        if set_start_type(name, *start_type).is_err() {
            failed += 1;
            continue;
        }
        if *start_type == SERVICE_AUTO_START.0 {
            let _ = start(name);
        }
    }
    if failed == 0 {
        format!("ASUS services restored ({})", backup.len())
    } else {
        format!("ASUS services restored ({}/{}, {failed} failed)", backup.len() - failed, backup.len())
    }
}

// silence the unused-import lint for c_void on some windows-rs versions
const _: Option<*const c_void> = None;
