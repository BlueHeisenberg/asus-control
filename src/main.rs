#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod nct6798d;
mod pawnio;
mod ring0;
mod sensors;
mod services;
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

    // Dying while we hold the fans leaves them frozen at the last duty we wrote,
    // with no curve to raise them as the CPU heats. `panic = "abort"` means no
    // unwinding and no Drop, so this hook is the only chance to hand them back.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        nct6798d::emergency_restore();
        prev(info);
    }));

    // autostart is on unless somebody turned it off on this machine
    autostart::ensure_default();

    let shared = worker::Shared::new();
    worker::spawn_worker(shared.clone());
    ui::run(shared);

    // normal exit: never leave the board under stale software control
    nct6798d::emergency_restore();
}

fn check() {
    println!("== asus-control self test ==");
    let r0 = match ring0::get() {
        Ok(r) => {
            println!(
                "[OK] {} backend loaded (version {})",
                ring0::backend_name(),
                r.version_string()
            );
            r
        }
        Err(e) => {
            println!("[FAIL] driver: {e}");
            if let Some(p) = ring0::pawnio_error() {
                println!("       PawnIO: {p}");
            }
            return;
        }
    };
    if let Some(p) = ring0::pawnio_error() {
        println!("[..] PawnIO unavailable, fell back to WinRing0: {p}");
    }

    // RTC validation. PawnIO confines us to the LpcIO module's discovered BARs, and
    // the RTC pair is not one of them, so this probe only means anything on WinRing0.
    if ring0::backend_name() == "WinRing0" {
        r0.write_port_byte(0x70, 0x08);
        let m = r0.read_port_byte(0x71);
        r0.write_port_byte(0x70, 0x09);
        let y = r0.read_port_byte(0x71);
        println!("[IO] RTC month/year = {m:#04X}/{y:#04X} (expect 0x08/0x26)");
    }

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
