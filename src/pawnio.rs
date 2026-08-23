//! PawnIO backend: port I/O through the signed, HVCI-compatible PawnIO driver
//! using the official `LpcIO` module.
//!
//! PawnIOLib.dll is LGPL-2.1 and this app is MIT, so the library is resolved with
//! LoadLibrary/GetProcAddress at runtime and never linked. That also lets us fail
//! softly: if the DLL or the module blob is missing, `ring0` falls back to WinRing0.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use windows::{
    core::{s, w, PCSTR, PCWSTR},
    Win32::{
        Foundation::HANDLE,
        System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    },
};

type FnVersion = unsafe extern "system" fn(*mut u32) -> i32;
type FnOpen = unsafe extern "system" fn(*mut HANDLE) -> i32;
type FnLoad = unsafe extern "system" fn(HANDLE, *const u8, usize) -> i32;
type FnExecute =
    unsafe extern "system" fn(HANDLE, PCSTR, *const u64, usize, *mut u64, usize, *mut usize) -> i32;
type FnClose = unsafe extern "system" fn(HANDLE) -> i32;

pub struct PawnIo {
    handle: SendHandle,
    execute: FnExecute,
    close: FnClose,
    /// (major << 16) | (minor << 8) | patch
    version: u32,
    /// SuperIO index port of the slot we selected (0x2E or 0x4E); data port is +1.
    index_port: u16,
    /// Shadow of the last byte written to the index port. The LpcIO module's
    /// superio_* ioctls take a register number, but our caller drives the raw
    /// index/data pair, so we remember the index to pair it with the data access.
    last_index: AtomicU8,
}

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

impl PawnIo {
    pub fn open() -> Result<PawnIo, String> {
        unsafe {
            let lib = LoadLibraryW(w!("PawnIOLib.dll"))
                .or_else(|_| {
                    let p = install_dir()
                        .map(|d| wide(&d.join("PawnIOLib.dll").to_string_lossy()))
                        .unwrap_or_default();
                    LoadLibraryW(PCWSTR(p.as_ptr()))
                })
                .map_err(|_| "PawnIOLib.dll not found (PawnIO not installed?)".to_string())?;

            let get = |name: PCSTR, label: &str| -> Result<*const (), String> {
                GetProcAddress(lib, name)
                    .map(|p| p as *const ())
                    .ok_or_else(|| format!("PawnIOLib.dll has no export {label}"))
            };
            let version: FnVersion =
                std::mem::transmute(get(s!("pawnio_version"), "pawnio_version")?);
            let open: FnOpen = std::mem::transmute(get(s!("pawnio_open"), "pawnio_open")?);
            let load: FnLoad = std::mem::transmute(get(s!("pawnio_load"), "pawnio_load")?);
            let execute: FnExecute =
                std::mem::transmute(get(s!("pawnio_execute"), "pawnio_execute")?);
            let close: FnClose = std::mem::transmute(get(s!("pawnio_close"), "pawnio_close")?);

            let blob_path = find_module()?;
            let blob = std::fs::read(&blob_path)
                .map_err(|e| format!("cannot read {}: {e}", blob_path.display()))?;

            let mut ver = 0u32;
            if version(&mut ver) < 0 {
                return Err("pawnio_version failed".into());
            }

            let mut handle = HANDLE::default();
            let hr = open(&mut handle);
            if hr < 0 {
                return Err(format!(
                    "pawnio_open failed (0x{hr:08X}) - driver not running?"
                ));
            }
            let io = PawnIo {
                handle: SendHandle(handle),
                execute,
                close,
                version: ver,
                index_port: 0x2E,
                last_index: AtomicU8::new(0),
            };
            let hr = load(handle, blob.as_ptr(), blob.len());
            if hr < 0 {
                return Err(format!(
                    "pawnio_load({}) failed (0x{hr:08X})",
                    blob_path.display()
                ));
            }
            io.probe_slots()
        }
    }

    /// Select the slot the SuperIO actually answers on and let the module discover
    /// its I/O BARs - until `ioctl_find_bars` has run the module denies every port
    /// outside the index/data pair, including the HWM window at 0x290.
    ///
    /// find_bars walks all 255 logical devices, writing the device-select register
    /// each time, so it needs the same arbitration nct6798d uses per transaction.
    /// This is the one-time init path, not a duplicate of that per-call lock.
    fn probe_slots(mut self) -> Result<PawnIo, String> {
        let _isa = IsaLock::acquire();
        for (slot, port) in [(0u64, 0x2Eu16), (1, 0x4E)] {
            if !self.exec(s!("ioctl_select_slot"), &[slot], &mut []) {
                continue;
            }
            self.index_port = port;
            // enter config mode (0x87 twice), find BARs, then leave it as we found it
            self.write_port_byte(port, 0x87);
            self.write_port_byte(port, 0x87);
            let found = self.exec(s!("ioctl_find_bars"), &[], &mut []);
            self.write_port_byte(port, 0xAA);
            if found {
                return Ok(self);
            }
        }
        Err("LpcIO module found no SuperIO chip on 0x2E or 0x4E".into())
    }

    fn exec(&self, name: PCSTR, input: &[u64], out: &mut [u64]) -> bool {
        let mut returned = 0usize;
        unsafe {
            (self.execute)(
                self.handle.0,
                name,
                input.as_ptr(),
                input.len(),
                out.as_mut_ptr(),
                out.len(),
                &mut returned,
            ) >= 0
        }
    }

    pub fn read_port_byte(&self, port: u16) -> u8 {
        let mut out = [0u64; 1];
        let ok = if port == self.index_port + 1 {
            self.exec(
                s!("ioctl_superio_inb"),
                &[self.last_index.load(Ordering::Relaxed) as u64],
                &mut out,
            )
        } else {
            self.exec(s!("ioctl_pio_inb"), &[port as u64], &mut out)
        };
        if ok {
            out[0] as u8
        } else {
            0
        }
    }

    pub fn write_port_byte(&self, port: u16, value: u8) {
        if port == self.index_port {
            // An index write is also how the 0x87/0x87 unlock and 0xAA lock
            // sequences are sent, so it has to hit the port, not just the shadow.
            self.last_index.store(value, Ordering::Relaxed);
            self.exec(s!("ioctl_pio_outb"), &[port as u64, value as u64], &mut []);
        } else if port == self.index_port + 1 {
            self.exec(
                s!("ioctl_superio_outb"),
                &[self.last_index.load(Ordering::Relaxed) as u64, value as u64],
                &mut [],
            );
        } else {
            self.exec(s!("ioctl_pio_outb"), &[port as u64, value as u64], &mut []);
        }
    }

    pub fn version_string(&self) -> String {
        format!(
            "{}.{}.{}",
            self.version >> 16,
            (self.version >> 8) & 0xFF,
            self.version & 0xFF
        )
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe {
            (self.close)(self.handle.0);
        }
    }
}

/// The same mutex nct6798d guards its transactions with; see probe_slots().
struct IsaLock(Option<HANDLE>);

impl IsaLock {
    fn acquire() -> IsaLock {
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        unsafe {
            let Ok(h) = CreateMutexW(None, false, w!("Global\\Access_ISABUS.HTP.Method")) else {
                return IsaLock(None);
            };
            let r = WaitForSingleObject(h, 200);
            // WAIT_OBJECT_0, or WAIT_ABANDONED (previous owner died holding it)
            if r.0 == 0 || r.0 == 0x80 {
                IsaLock(Some(h))
            } else {
                IsaLock(None)
            }
        }
    }
}

impl Drop for IsaLock {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            unsafe {
                let _ = windows::Win32::System::Threading::ReleaseMutex(h);
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn install_dir() -> Option<PathBuf> {
    std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join("PawnIO"))
}

/// LpcIO.bin is LGPL too and is not committed here - see the README.
fn find_module() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("LpcIO.bin"));
        }
    }
    if let Some(dir) = install_dir() {
        candidates.push(dir.join("LpcIO.bin"));
        candidates.push(dir.join("modules").join("LpcIO.bin"));
    }
    candidates
        .iter()
        .find(|c| c.exists())
        .cloned()
        .ok_or_else(|| format!("LpcIO.bin not found; looked in {candidates:?}"))
}
