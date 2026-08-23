use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Deserialize, Debug, Clone)]
pub struct BridgeHeader {
    #[serde(default)]
    pub hardware: Vec<BridgeHardware>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BridgeHardware {
    pub id: String,
    pub name: String,
    #[serde(rename = "hardwareType")]
    pub hardware_type: String,
    #[serde(default)]
    pub sensors: Vec<BridgeSensor>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BridgeSensor {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub sensor_type: String,
    pub unit: String,
}

#[derive(Deserialize)]
struct ValuesMsg {
    data: HashMap<String, f32>,
}

/// Manages the LibreHardwareMonitor bridge subprocess.
pub struct SensorBridge {
    child: Mutex<Option<Child>>,
    header: Arc<Mutex<Option<BridgeHeader>>>,
    values: Arc<Mutex<HashMap<String, f32>>>,
}

impl SensorBridge {
    /// Spawn the bridge exe (must be next to our exe or in ./bridge/publish).
    pub fn start() -> Result<SensorBridge, String> {
        let exe = find_bridge_exe()?;

        // CREATE_NO_WINDOW = 0x08000000 — hide the child's console
        let mut child = Command::new(&exe)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|e| format!("failed to spawn {exe:?}: {e}"))?;

        let stdout = child.stdout.take().ok_or("no stdout")?;

        let header = Arc::new(Mutex::new(None));
        let values = Arc::new(Mutex::new(HashMap::new()));

        {
            let header = header.clone();
            let values = values.clone();
            std::thread::Builder::new()
                .name("sensor-bridge".into())
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        let Ok(line) = line else { break };
                        // Cheap dispatch on prefix before full parse
                        if line.starts_with(r#"{"type":"header""#) {
                            if let Ok(h) = serde_json::from_str::<BridgeHeader>(&line) {
                                *header.lock().unwrap() = Some(h);
                            }
                        } else if line.starts_with(r#"{"type":"values""#) {
                            if let Ok(v) = serde_json::from_str::<ValuesMsg>(&line) {
                                let mut g = values.lock().unwrap();
                                // Replace wholesale; bridge sends a complete snapshot
                                *g = v.data;
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }

        Ok(SensorBridge {
            child: Mutex::new(Some(child)),
            header,
            values,
        })
    }

    pub fn header(&self) -> Option<BridgeHeader> {
        self.header.lock().unwrap().clone()
    }

    pub fn wait_header(&self, timeout_ms: u64) -> Option<BridgeHeader> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while std::time::Instant::now() < deadline {
            if let Some(h) = self.header.lock().unwrap().as_ref() {
                return Some(h.clone());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    pub fn values(&self) -> HashMap<String, f32> {
        self.values.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<f32> {
        self.values.lock().unwrap().get(id).copied()
    }
}

impl Drop for SensorBridge {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn find_bridge_exe() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().unwrap();
    let candidates = [
        dir.join("sensor-bridge.exe"),
        dir.join("bridge").join("publish").join("bridge.exe"),
        dir.parent()
            .map(|d| d.join("bridge").join("publish").join("bridge.exe"))
            .unwrap_or_default(),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "sensor-bridge.exe not found; looked in {:?}",
        candidates
    ))
}
