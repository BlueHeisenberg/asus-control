use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const FAN_NAMES: [&str; 7] = ["CPU_FAN", "FAN2", "FAN3", "FAN4", "FAN5", "FAN6", "FAN7"];

#[derive(Serialize, Deserialize, Clone)]
pub struct FanConfig {
    pub enabled: bool,
    pub points: Vec<(f32, f32)>, // (temp °C, duty %)
}

/// Global show/hide hotkey. `mods` is a MOD_* bitmask (ALT 1, CTRL 2,
/// SHIFT 4, WIN 8), `vk` a virtual-key code. Default Ctrl+Shift+F12:
/// plain Shift+F12 is already claimed on a lot of machines, and a hotkey
/// that silently fails to register is worse than one extra modifier.
#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Default for Hotkey {
    fn default() -> Self {
        Hotkey { mods: 2 | 4, vk: 0x7B } // MOD_CONTROL | MOD_SHIFT + VK_F12
    }
}

impl Hotkey {
    /// Human-readable form for the status line, e.g. "Shift+F12".
    pub fn label(&self) -> String {
        let mut s = String::new();
        for (bit, name) in [(1, "Alt"), (2, "Ctrl"), (4, "Shift"), (8, "Win")] {
            if self.mods & bit != 0 {
                s.push_str(name);
                s.push('+');
            }
        }
        match self.vk {
            0x70..=0x87 => s.push_str(&format!("F{}", self.vk - 0x6F)),
            v => s.push_str(&format!("VK 0x{v:02X}")),
        }
        s
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub fans: Vec<FanConfig>,
    pub tick_ms: u32,
    /// (service name, previous start type) for ASUS services we disabled.
    /// Empty means we have not disabled anything. This is the only way back,
    /// so it lives in the config file rather than in memory.
    #[serde(default)]
    pub asus_backup: Vec<(String, u32)>,
    #[serde(default)]
    pub hotkey: Hotkey,
    /// Where the user dragged the window to, in physical screen pixels.
    /// None = fall back to docking bottom-right of the cursor's monitor.
    #[serde(default)]
    pub window_pos: Option<(i32, i32)>,
    /// keep the window above every other window. On by default; a user who
    /// turns it off has that written to their own config and kept.
    #[serde(default = "yes")]
    pub always_on_top: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            fans: default_fans(),
            tick_ms: 500,
            asus_backup: Vec::new(),
            hotkey: Hotkey::default(),
            window_pos: None,
            always_on_top: true,
        }
    }
}

fn yes() -> bool {
    true
}

pub fn default_fans() -> Vec<FanConfig> {
    let curve = vec![
        (30.0f32, 20.0f32),
        (50.0, 35.0),
        (65.0, 55.0),
        (80.0, 80.0),
        (95.0, 100.0),
    ];
    FAN_NAMES
        .iter()
        .enumerate()
        .map(|(i, _)| FanConfig {
            enabled: i == 0,
            points: curve.clone(),
        })
        .collect()
}

fn config_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(base).join("asus-control");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.json")
}

pub fn load() -> Config {
    let path = config_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .map(|mut c: Config| {
            // Pad/trim to exactly FAN_NAMES.len()
            c.fans.resize_with(default_fans().len(), || FanConfig {
                enabled: false,
                points: default_fans()[6].points.clone(),
            });
            c.fans.truncate(FAN_NAMES.len());
            if c.tick_ms < 100 {
                c.tick_ms = 100;
            }
            c
        })
        .unwrap_or_default()
}

/// Whether a config file exists on disk yet.
pub fn exists() -> bool {
    config_path().exists()
}

pub fn save(
    fans: &[FanConfig],
    tick_ms: u32,
    asus_backup: &[(String, u32)],
    window_pos: Option<(i32, i32)>,
    always_on_top: bool,
) {
    let cfg = Config {
        fans: fans.to_vec(),
        tick_ms,
        asus_backup: asus_backup.to_vec(),
        window_pos,
        always_on_top,
        // nothing in the running app edits the hotkey, so keep whatever the
        // user hand-edited into the file instead of stamping the default back
        hotkey: load().hotkey,
    };
    if let Ok(json) = serde_json::to_string_pretty(&cfg) {
        let _ = std::fs::write(config_path(), json);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_default_is_shift_f12() {
        assert_eq!(Hotkey::default().label(), "Shift+F12");
        assert_eq!(Hotkey { mods: 2 | 4, vk: 0x70 }.label(), "Ctrl+Shift+F1");
        // existing configs without the field must still load
        let c: Config = serde_json::from_str(r#"{"fans":[],"tick_ms":500}"#).unwrap();
        assert_eq!(c.hotkey.vk, 0x7B);
        assert_eq!(c.hotkey.mods, 6);
    }
}
