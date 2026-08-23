use crate::ring0;

/// HWM base I/O address (from SuperIO LDN 0x0B config regs 0x60/0x61)
pub struct Nct6798d {
    base: u16,
    current_bank: u8,
    /// saved control-mode register values for restore
    saved_mode: [u8; 7],
    saved_pwm: [u8; 7],
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

impl Nct6798d {
    /// Detect the chip. Returns None if no NCT6798D/6799D found.
    pub fn detect() -> Option<Nct6798d> {
        let r0 = ring0::get().ok()?;
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

            return Some(Nct6798d {
                base,
                current_bank: 0xFF,
                saved_mode: [0; 7],
                saved_pwm: [0; 7],
            });
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
        let duty = ((percent.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u8;
        // Save defaults on first touch
        if self.saved_mode[index] == 0 {
            self.saved_mode[index] = self.read_byte(FAN_CONTROL_MODE_REG[index]);
            self.saved_pwm[index] = self.read_byte(FAN_PWM_COMMAND_REG[index]);
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
        if self.saved_mode[index] != 0 {
            self.write_byte(FAN_CONTROL_MODE_REG[index], self.saved_mode[index]);
            self.write_byte(FAN_PWM_COMMAND_REG[index], self.saved_pwm[index]);
            self.saved_mode[index] = 0;
            self.saved_pwm[index] = 0;
        }
    }

    pub fn base_address(&self) -> u16 {
        self.base
    }
}
