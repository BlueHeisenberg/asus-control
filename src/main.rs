#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod nct6798d;
mod ring0;
mod sensors;
mod ui;
mod worker;

fn main() {
    if std::env::args().any(|a| a == "--check") {
        check();
        return;
    }
    if std::env::args().any(|a| a == "--testfan") {
        test_fan();
        return;
    }

    let shared = worker::Shared::new();
    worker::spawn_worker(shared.clone());
    ui::run(shared);
}

fn check() {
    println!("== asus-control self test ==");
    let r0 = match ring0::get() {
        Ok(r) => {
            let v = r.driver_version().map(|v| v.to_string()).unwrap_or_default();
            println!("[OK] WinRing0 driver loaded (version {v})");
            r
        }
        Err(e) => {
            println!("[FAIL] driver: {e}");
            return;
        }
    };

    // RTC validation
    r0.write_port_byte(0x70, 0x08);
    let m = r0.read_port_byte(0x71);
    r0.write_port_byte(0x70, 0x09);
    let y = r0.read_port_byte(0x71);
    println!("[IO] RTC month/year = {m:#04X}/{y:#04X} (expect 0x08/0x26)");

    let Some(mut nct) = nct6798d::Nct6798d::detect() else {
        println!("[FAIL] NCT6798D not found");
        return;
    };
    println!("[OK] SuperIO at base 0x{:X}", nct.base_address());

    for (l, t) in nct.read_temps() {
        println!("  temp {l}: {t:.1} C");
    }
    for (i, f) in nct.read_fans().iter().enumerate() {
        println!("  fan{}: {}", i + 1, f.map(|v| format!("{v:.0} rpm")).unwrap_or("--".into()));
    }
    println!("  pwm duty: {:?}", nct.read_pwm());
}

/// Ramps CPU_FAN through several duties and prints RPM at each step.
fn test_fan() {
    let Some(mut nct) = nct6798d::Nct6798d::detect() else {
        println!("[FAIL] no SuperIO");
        return;
    };
    println!("fan1 baseline: {:?}", nct.read_fans()[0]);
    for duty in [30u32, 60, 90] {
        nct.set_fan_duty(0, duty as f32);
        println!("set fan1 duty {duty}% ...");
        std::thread::sleep(std::time::Duration::from_millis(2500));
        println!("   rpm now: {:?}", nct.read_fans()[0]);
    }
    nct.restore_fan(0);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    println!("restored; rpm: {:?}", nct.read_fans()[0]);
}
