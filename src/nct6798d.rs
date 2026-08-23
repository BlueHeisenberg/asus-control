use crate::ring0;
use std::sync::atomic::{AtomicU16, Ordering};

/// HWM base I/O address (from SuperIO LDN 0x0B config regs 0x60/0x61)
pub struct Nct6798d {
    base: u16,
    /// last bank we selected; invalidated on every lock acquire because
    /// another process may have changed it while we were not holding the mutex
    current_bank: u8,
}

// ---- Register map (from LibreHardwareMonitor Nct677X.cs, NCT6798D/NCT6799D) ----

const BANK_SELECT_REGISTER: u16 = 0x4E;
const ADDR_REG_OFFSET: u16 = 0x05;
const DATA_REG_OFFSET: u16 = 0x06;

const VENDOR_ID_HIGH: u16 = 0x804F;
const VENDOR_ID_LOW: u16 = 0x004F;
const NUVOTON_VENDOR_ID: u16 = 0x5CA3;

/// PWM duty output registers (read current duty)
const FAN_PWM_OUT_REG: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09];
/// PWM command registers (write duty)
const FAN_PWM_COMMAND_REG: [u16; 7] = [0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09];
/// Control-mode registers (write 0 => manual/software mode)
const FAN_CONTROL_MODE_REG: [u16; 7] = [0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02];

/// Fan tachometer count registers (13-bit counter)
const FAN_COUNT_REG: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];

/// Voltage registers
const VOLTAGE_REG: [u16; 16] = [
    0x480, 0x481, 0x482, 0x483, 0x484, 0x485, 0x486, 0x487,
    0x488, 0x489, 0x48A, 0x48B, 0x48C, 0x48D, 0x48E, 0x48F,
];
const VBAT_MONITOR_CTRL_REG: u16 = 0x005D;

/// Temperature source descriptors:
/// (register, half_register Option<u16>, half_bit, source_register Option<u16>, name)
#[derive(Clone, Copy)]
struct TempSource {
    reg: u16,
    half_reg: u16, // 0 = none
    half_bit: u8,
    src_reg: u16, // 0 = none (use fixed source id below)
    src_id: u8,
    label: &'static str,
}

const TEMPS: [TempSource; 10] = [
    TempSource { reg: 0x073, half_reg: 0x074, half_bit: 7, src_reg: 0x100, src_id: 1,  label: "PECI/CPU" },
    TempSource { reg: 0x075, half_reg: 0x076, half_bit: 7, src_reg: 0x200, src_id: 2,  label: "CPU" },
    TempSource { reg: 0x077, half_reg: 0x078, half_bit: 7, src_reg: 0x300, src_id: 3,  label: "SYS" },
    TempSource { reg: 0x079, half_reg: 0x07A, half_bit: 7, src_reg: 0x800, src_id: 8,  label: "AUX0" },
    TempSource { reg: 0x07B, half_reg: 0x07C, half_bit: 7, src_reg: 0x900, src_id: 9,  label: "AUX1" },
    TempSource { reg: 0x07D, half_reg: 0x07E, half_bit: 7, src_reg: 0xA00, src_id: 10, label: "AUX2" },
    TempSource { reg: 0x4A0, half_reg: 0x49E, half_bit: 6, src_reg: 0xB00, src_id: 11, label: "AUX3" },
    TempSource { reg: 0x027, half_reg: 0,     half_bit: 0, src_reg: 0x621, src_id: 26, label: "AUX4" },
    TempSource { reg: 0x150, half_reg: 0x151, half_bit: 7, src_reg: 0x622, src_id: 27, label: "SMBUS0" },
    TempSource { reg: 0x4A2, half_reg: 0x4A1, half_bit: 7, src_reg: 0xC00, src_id: 12, label: "TSENSOR" },
];

/// The board's SuperIO is a shared index/data port pair: you write the register
/// to ADDR then read/write DATA. That sequence is not atomic, and Armoury Crate,
/// HWiNFO and LHM all drive the same chip. Every vendor tool arbitrates on this
/// mutex; if we don't, another process lands its index between our two writes
/// and we read — or worse, WRITE — a completely different register.
const ISA_MUTEX_NAME: &str = r"Global\Access_ISABUS.HTP.Method";

fn isa_mutex() -> windows::Win32::Foundation::HANDLE {
    use std::sync::OnceLock;
    use windows::Win32::System::Threading::CreateMutexW;
    static H: OnceLock<usize> = OnceLock::new();
    let raw = *H.get_or_init(|| unsafe {
        let name: Vec<u16> = ISA_MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr()))
            .map(|h| h.0 as usize)
            .unwrap_or(0)
    });
    windows::Win32::Foundation::HANDLE(raw as *mut _)
}

/// Held for the duration of one logical SuperIO transaction.
struct IsaGuard(Option<windows::Win32::Foundation::HANDLE>);

impl IsaGuard {
    fn new() -> IsaGuard {
        use windows::Win32::System::Threading::WaitForSingleObject;
        let h = isa_mutex();
        if h.0 as usize == 0 {
            return IsaGuard(None);
        }
        let r = unsafe { WaitForSingleObject(h, 200) };
        // WAIT_OBJECT_0 = 0, WAIT_ABANDONED = 0x80 (previous owner died holding it)
        if r.0 == 0 || r.0 == 0x80 {
            IsaGuard(Some(h))
        } else {
            // Timed out. Proceed unlocked rather than stall fan control, but we
            // do NOT own the mutex so we must not release it.
            IsaGuard(None)
        }
    }
}

impl Drop for IsaGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            unsafe {
                let _ = windows::Win32::System::Threading::ReleaseMutex(h);
            }
        }
    }
}

/// Mirror of each fan's pre-takeover BIOS state, kept in statics rather than in
/// Nct6798d so ANY thread — including a panic hook, after the worker is gone —
/// can hand the fans back to firmware. 0xFFFF means "we never touched this fan".
const UNSAVED: u16 = 0xFFFF;
static SAVED_MODE: [AtomicU16; 7] = [const { AtomicU16::new(UNSAVED) }; 7];
static SAVED_PWM: [AtomicU16; 7] = [const { AtomicU16::new(UNSAVED) }; 7];

/// Give every fan we took over back to the firmware. Safe to call from a panic
/// hook or at exit: it re-detects the chip instead of needing the worker's copy.
pub fn emergency_restore() {
    let Some(mut n) = Nct6798d::detect() else { return };
    for i in 0..7 {
        n.restore_fan(i);
    }
}

impl Nct6798d {
    /// Detect the chip. Returns None if no NCT6798D/6799D found.
    pub fn detect() -> Option<Nct6798d> {
        let r0 = ring0::get().ok()?;
        let _isa = IsaGuard::new();
        // Try standard SuperIO ports
        for sio_port in [0x2Eu16, 0x4Eu16] {
            // Enter config mode
            r0.write_port_byte(sio_port, 0x87);
            r0.write_port_byte(sio_port, 0x87);

            // Read device ID (config regs 0x20 / 0x21)
            r0.write_port_byte(sio_port, 0x20);
            let hi = r0.read_port_byte(sio_port + 1);
            r0.write_port_byte(sio_port, 0x21);
            let lo = r0.read_port_byte(sio_port + 1);
            let devid = ((hi as u16) << 8) | lo as u16;

            // NCT6798D = 0xD428, NCT6799D = 0xD800
            let known = matches!(devid & 0xFFF8, 0xD420 | 0xD800);

            if !known {
                Self::exit_config(r0, sio_port);
                continue;
            }

            // Select logical device 0x0B (Hardware Monitor)
            r0.write_port_byte(sio_port, 0x07);
            r0.write_port_byte(sio_port + 1, 0x0B);

            // Enable the logical device (reg 0x30 bit 0)
            r0.write_port_byte(sio_port, 0x30);
            let en = r0.read_port_byte(sio_port + 1);
            if en & 0x01 == 0 {
                r0.write_port_byte(sio_port + 1, en | 0x01);
            }

            // Clear HM IO space lock (config reg 0x28 bit 4)
            r0.write_port_byte(sio_port, 0x28);
            let v = r0.read_port_byte(sio_port + 1);
            if v & 0x10 != 0 {
                r0.write_port_byte(sio_port + 1, v & !0x10);
            }

            // Read the HWM I/O base address (regs 0x60/0x61)
            r0.write_port_byte(sio_port, 0x60);
            let bhi = r0.read_port_byte(sio_port + 1);
            r0.write_port_byte(sio_port, 0x61);
            let blo = r0.read_port_byte(sio_port + 1);
            let base = (((bhi as u16) << 8) | blo as u16) & !0x7u16;

            Self::exit_config(r0, sio_port);

            if base == 0 || base >= 0xFF00 {
                continue;
            }

            return Some(Nct6798d { base, current_bank: 0xFF });
        }
        None
    }

    fn exit_config(r: &ring0::Ring0, sio_port: u16) {
        r.write_port_byte(sio_port, 0xAA);
    }

    #[inline]
    fn set_bank(&mut self, bank: u8) {
        if self.current_bank != bank {
            let r = ring0::get().unwrap();
            r.write_port_byte(self.base + ADDR_REG_OFFSET, BANK_SELECT_REGISTER as u8);
            r.write_port_byte(self.base + DATA_REG_OFFSET, bank);
            self.current_bank = bank;
        }
    }

    pub fn read_byte(&mut self, reg: u16) -> u8 {
        let r = ring0::get().unwrap();
        let bank = (reg >> 8) as u8;
        self.set_bank(bank);
        r.write_port_byte(self.base + ADDR_REG_OFFSET, (reg & 0xFF) as u8);
        r.read_port_byte(self.base + DATA_REG_OFFSET)
    }

    pub fn write_byte(&mut self, reg: u16, value: u8) {
        let r = ring0::get().unwrap();
        let bank = (reg >> 8) as u8;
        self.set_bank(bank);
        r.write_port_byte(self.base + ADDR_REG_OFFSET, (reg & 0xFF) as u8);
        r.write_port_byte(self.base + DATA_REG_OFFSET, value);
    }

    /// Vendor ID sanity check
    pub fn is_nuvoton(&mut self) -> bool {
        let hi = self.read_byte(VENDOR_ID_HIGH) as u16;
        let lo = self.read_byte(VENDOR_ID_LOW) as u16;
        ((hi << 8) | lo) == NUVOTON_VENDOR_ID
    }

    /// Read temperatures in degrees Celsius.
    pub fn read_temps(&mut self) -> Vec<(String, f32)> {
        let _isa = IsaGuard::new();
        self.current_bank = 0xFF;
        let mut out = Vec::new();
        for ts in TEMPS.iter() {
            let raw = self.read_byte(ts.reg) as i8;
            let mut value = (raw as i32) << 1;
            if ts.half_reg != 0 && ts.half_bit > 0 {
                let hb = self.read_byte(ts.half_reg);
                value |= ((hb >> ts.half_bit) & 1) as i32;
            }
            let temp = value as f32 * 0.5;
            if (-55.0..=125.0).contains(&temp) {
                out.push((ts.label.to_string(), temp));
            }
        }
        out
    }

    /// Read fan RPMs.
    pub fn read_fans(&mut self) -> Vec<Option<f32>> {
        let _isa = IsaGuard::new();
        self.current_bank = 0xFF;
        let mut out = Vec::with_capacity(7);
        for reg in FAN_COUNT_REG.iter() {
            let high = self.read_byte(*reg) as u16;
            let low = self.read_byte(*reg + 1) as u16;
            // NCT6687-style sentinel check not needed here
            let count = (high << 5) | (low & 0x1F);
            const MAX_FAN_COUNT: u16 = 0x1FFF;
            const MIN_FAN_COUNT: u16 = 0x15;
            if count < MAX_FAN_COUNT {
                if count >= MIN_FAN_COUNT {
                    out.push(Some(1_350_000f32 / count as f32));
                } else {
                    out.push(None);
                }
            } else {
                out.push(Some(0.0));
            }
        }
        out
    }

    /// Read voltages.
    pub fn read_voltages(&mut self) -> Vec<Option<f32>> {
        let mut out = Vec::with_capacity(16);
        let vbat_enabled = (self.read_byte(VBAT_MONITOR_CTRL_REG) & 0x01) > 0;
        for (i, reg) in VOLTAGE_REG.iter().enumerate() {
            let v = 0.008f32 * self.read_byte(*reg) as f32;
            let valid = if *reg == VOLTAGE_REG[8] { v > 0.0 && vbat_enabled } else { v > 0.0 };
            out.push(if valid { Some(v) } else { None });
            let _ = i;
        }
        out
    }

    /// Read current PWM duty (0-100%) per fan.
    pub fn read_pwm(&mut self) -> Vec<Option<f32>> {
        let _isa = IsaGuard::new();
        self.current_bank = 0xFF;
        let mut out = Vec::with_capacity(7);
        for reg in FAN_PWM_OUT_REG.iter() {
            let v = self.read_byte(*reg);
            out.push(if v > 0 { Some(v as f32 * 100.0 / 255.0) } else { Some(0.0) });
        }
        out
    }

    /// Take manual (software) control of a fan and write a duty percentage.
    pub fn set_fan_duty(&mut self, index: usize, percent: f32) {
        if index >= 7 {
            return;
        }
        let _isa = IsaGuard::new();
        self.current_bank = 0xFF;
        let duty = ((percent.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u8;
        // Save the firmware's settings the first time we touch this fan.
        // NB: mode 0 is a LEGAL value, so "is it saved" needs its own sentinel —
        // using 0 meant restore silently did nothing and the saved PWM got
        // overwritten with our own duty on every tick.
        if SAVED_MODE[index].load(Ordering::Relaxed) == UNSAVED {
            let m = self.read_byte(FAN_CONTROL_MODE_REG[index]) as u16;
            let p = self.read_byte(FAN_PWM_COMMAND_REG[index]) as u16;
            SAVED_MODE[index].store(m, Ordering::Relaxed);
            SAVED_PWM[index].store(p, Ordering::Relaxed);
        }
        // Manual mode
        self.write_byte(FAN_CONTROL_MODE_REG[index], 0);
        // Write duty
        self.write_byte(FAN_PWM_COMMAND_REG[index], duty);
    }

    /// Restore automatic (firmware/BIOS) control of a fan.
    pub fn restore_fan(&mut self, index: usize) {
        if index >= 7 {
            return;
        }
        let mode = SAVED_MODE[index].load(Ordering::Relaxed);
        if mode != UNSAVED {
            let _isa = IsaGuard::new();
            self.current_bank = 0xFF;
            self.write_byte(FAN_CONTROL_MODE_REG[index], mode as u8);
            self.write_byte(FAN_PWM_COMMAND_REG[index], SAVED_PWM[index].load(Ordering::Relaxed) as u8);
            SAVED_MODE[index].store(UNSAVED, Ordering::Relaxed);
            SAVED_PWM[index].store(UNSAVED, Ordering::Relaxed);
        }
    }

    pub fn base_address(&self) -> u16 {
        self.base
    }
}
