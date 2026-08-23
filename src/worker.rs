use crate::config::{self, FanConfig};
use crate::nct6798d::Nct6798d;
use crate::ring0;
use crate::sensors::SensorBridge;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One bridge sensor: label, value, unit
pub type BridgeEntry = (String, f32, String);

#[derive(Default)]
pub struct SensorData {
    pub temps: Vec<(String, f32)>,
    pub bridge: Vec<BridgeEntry>,
    pub rpm: Vec<Option<f32>>,
    pub duty: Vec<Option<f32>>,
}

pub struct Shared {
    pub data: Mutex<SensorData>,
    pub fans: Mutex<Vec<FanConfig>>,
    /// set by the UI, actioned on the worker thread (service calls block)
    pub toggle_asus: AtomicBool,
    pub asus_backup: Mutex<Vec<(String, u32)>>,
    /// last position the user dragged the window to
    pub window_pos: Mutex<Option<(i32, i32)>>,
    pub restore_mask: AtomicU64,
    pub release_all: AtomicBool,
    pub tick_ms: AtomicU32,
    pub hw_ok: AtomicBool,
    pub status: Mutex<String>,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        let mut cfg = config::load();
        // Create the config file on first run so users can find/edit it
        if !config::exists() {
            config::save(&cfg.fans, cfg.tick_ms, &cfg.asus_backup, cfg.window_pos);
        }
        let _ = &mut cfg;
        let tick = cfg.tick_ms;
        Arc::new(Shared {
            data: Mutex::new(SensorData::default()),
            fans: Mutex::new(cfg.fans),
            toggle_asus: AtomicBool::new(false),
            asus_backup: Mutex::new(cfg.asus_backup),
            window_pos: Mutex::new(cfg.window_pos),
            restore_mask: AtomicU64::new(0),
            release_all: AtomicBool::new(false),
            tick_ms: AtomicU32::new(tick),
            hw_ok: AtomicBool::new(false),
            status: Mutex::new("starting…".into()),
        })
    }

    pub fn persist(&self) {
        let fans = self.fans.lock().unwrap().clone();
        let backup = self.asus_backup.lock().unwrap().clone();
        let pos = *self.window_pos.lock().unwrap();
        config::save(&fans, self.tick_ms.load(Ordering::Relaxed), &backup, pos);
    }

    /// True when we currently hold ASUS services disabled.
    pub fn asus_disabled(&self) -> bool {
        !self.asus_backup.lock().unwrap().is_empty()
    }

    /// CPU temperature used as the default control source.
    pub fn control_temp(data: &SensorData) -> Option<f32> {
        data.temps
            .iter()
            .find(|(l, _)| l.contains("PECI") || l == &"CPU")
            .map(|(_, t)| *t)
            .or_else(|| {
                data.bridge
                    .iter()
                    .find(|(l, _, _)| l.ends_with("· CPU"))
                    .map(|(_, t, _)| *t)
            })
    }
}

pub fn spawn_worker(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("hw-worker".into())
        .spawn(move || worker_loop(shared))
        .expect("spawn worker");
}

fn worker_loop(shared: Arc<Shared>) {
    // 1. ring0 driver
    let mut status = String::new();
    if ring0::get().is_ok() {
        status.push_str("driver OK · ");
    } else {
        *shared.status.lock().unwrap() = "driver FAILED (run as admin)".into();
        return;
    }

    // 2. SuperIO
    let nct = Nct6798d::detect();
    match &nct {
        Some(n) => status.push_str(&format!("NCT6798D @ 0x{:X} · ", n.base_address())),
        None => {
            status.push_str("SuperIO NOT FOUND · ");
            shared.hw_ok.store(false, Ordering::Relaxed);
        }
    }
    shared.hw_ok.store(nct.is_some(), Ordering::Relaxed);

    // Take ownership of nct for the lifetime of the thread
    let mut nct = nct;

    // 3. Bridge
    let bridge = SensorBridge::start().ok();

    *shared.status.lock().unwrap() = status;

    loop {
        let t0 = std::time::Instant::now();

        // Handle one-shot commands
        if shared.release_all.swap(false, Ordering::Relaxed) {
            if let Some(n) = nct.as_mut() {
                for i in 0..7 {
                    n.restore_fan(i);
                }
            }
            let mut fans = shared.fans.lock().unwrap();
            for f in fans.iter_mut() {
                f.enabled = false;
            }
            shared.persist();
        }
        // Stopping services blocks for seconds per service. Doing that inline
        // froze the poll loop, so nothing refreshed the fans while the ASUS
        // stack was tearing down — run it on its own thread.
        if shared.toggle_asus.swap(false, Ordering::Relaxed) {
            let sh = shared.clone();
            std::thread::spawn(move || {
                let held = sh.asus_backup.lock().unwrap().clone();
                let msg = if held.is_empty() {
                    let (backup, msg) = crate::services::disable_all();
                    *sh.asus_backup.lock().unwrap() = backup;
                    msg
                } else {
                    let msg = crate::services::restore(&held);
                    sh.asus_backup.lock().unwrap().clear();
                    msg
                };
                *sh.status.lock().unwrap() = msg;
                sh.persist();
            });
        }

        let mask = shared.restore_mask.swap(0, Ordering::Relaxed);
        if mask != 0 {
            if let Some(n) = nct.as_mut() {
                for i in 0..7 {
                    if mask & (1 << i) != 0 {
                        n.restore_fan(i);
                    }
                }
            }
        }

        // Poll sensors
        {
            let mut data = shared.data.lock().unwrap();
            if let Some(n) = nct.as_mut() {
                data.temps = n.read_temps();
                data.rpm = n.read_fans();
                data.duty = n.read_pwm();
            } else {
                data.temps.clear();
                data.rpm.clear();
                data.duty.clear();
            }
            if let Some(b) = &bridge {
                data.bridge = extract_bridge(b);
            }
        }

        // Apply curves
        if let Some(n) = nct.as_mut() {
            let data = shared.data.lock().unwrap();
            if let Some(temp) = Shared::control_temp(&data) {
                let fans = shared.fans.lock().unwrap();
                for (i, f) in fans.iter().enumerate() {
                    if f.enabled && !f.points.is_empty() {
                        let duty = interp(&f.points, temp);
                        n.set_fan_duty(i, duty);
                    }
                }
            }
        }

        let tick = shared.tick_ms.load(Ordering::Relaxed) as u128;
        let elapsed = t0.elapsed().as_millis();
        if elapsed < tick {
            std::thread::sleep(std::time::Duration::from_millis((tick - elapsed) as u64));
        }
    }
}

fn interp(points: &[(f32, f32)], temp: f32) -> f32 {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    if pts.is_empty() {
        return 50.0;
    }
    if temp <= pts[0].0 {
        return pts[0].1;
    }
    if temp >= pts[pts.len() - 1].0 {
        return pts[pts.len() - 1].1;
    }
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if temp >= a.0 && temp <= b.0 {
            let t = (temp - a.0) / (b.0 - a.0);
            return a.1 + t * (b.1 - a.1);
        }
    }
    pts[0].1
}

fn extract_bridge(bridge: &SensorBridge) -> Vec<BridgeEntry> {
    let mut out = Vec::new();
    let Some(header) = bridge.header() else { return out };
    let values = bridge.values();

    for hw in header.hardware.iter() {
        let short = short_hw_name(&hw.name);
        for s in hw.sensors.iter() {
            let wanted = match (s.sensor_type.as_str(), s.name.as_str()) {
                ("Temperature", "Core (Tctl/Tdie)") => Some(("CPU", "°C")),
                ("Temperature", "Package") => Some(("CPU pkg", "°C")),
                ("Temperature", "GPU Core") => Some(("GPU", "°C")),
                ("Temperature", "GPU Memory Junction") => Some(("GPU mem", "°C")),
                ("Temperature", "GPU Package") => Some(("GPU pkg", "°C")),
                ("Temperature", "Composite Temperature") => Some(("NVMe", "°C")),
                ("Load", "CPU Total") => Some(("CPU load", "%")),
                ("Load", "GPU Core") => Some(("GPU load", "%")),
                ("Data", "Memory Used") => Some(("RAM used", "GB")),
                _ => None,
            };
            if let Some((label, unit)) = wanted {
                if let Some(v) = values.get(&s.id) {
                    out.push((format!("{short} · {label}"), *v, unit.to_string()));
                }
            }
        }
    }
    out
}

fn short_hw_name(full: &str) -> String {
    if full.contains("Ryzen") {
        "CPU".into()
    } else if full.contains("NVIDIA") || full.contains("AMD Radeon") {
        "GPU".into()
    } else if full.contains("CT4") || full.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        full.split_whitespace()
            .next()
            .unwrap_or(full)
            .chars()
            .take(12)
            .collect()
    } else {
        full.chars().take(20).collect()
    }
}
