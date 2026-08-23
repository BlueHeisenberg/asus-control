use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const FAN_NAMES: [&str; 7] = ["CPU_FAN", "FAN2", "FAN3", "FAN4", "FAN5", "FAN6", "FAN7"];

#[derive(Serialize, Deserialize, Clone)]
pub struct FanConfig {
    pub enabled: bool,
    pub points: Vec<(f32, f32)>, // (temp °C, duty %)
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub fans: Vec<FanConfig>,
    pub tick_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            fans: default_fans(),
            tick_ms: 500,
        }
    }
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

pub fn save(fans: &[FanConfig], tick_ms: u32) {
    let cfg = Config {
        fans: fans.to_vec(),
        tick_ms,
    };
    if let Ok(json) = serde_json::to_string_pretty(&cfg) {
        let _ = std::fs::write(config_path(), json);
    }
}

