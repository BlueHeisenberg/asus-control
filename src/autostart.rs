//! Machine-wide "start with Windows", via a scheduled task.
//!
//! Why a task and not HKLM\...\Run: this app requires administrator (it loads a
//! port-I/O driver). A Run entry for an elevated exe produces a UAC prompt at
//! every logon, which nobody accepts forever. A scheduled task registered with
//! RunLevel=HighestAvailable starts elevated without prompting.
//!
//! Why it is machine-wide: the task is registered once, for the whole box, with
//! the BUILTIN\Users group as its principal and a logon trigger that carries no
//! UserId — so it fires in the interactive session of whoever logs on. The
//! toggle therefore reflects real machine state (does the task exist) rather
//! than anything stored per user.
//!
//! Per-user settings are unaffected: fan curves and everything else live in
//! %APPDATA%\asus-control\config.json, which is already per-profile.

use std::os::windows::process::CommandExt;
use std::process::Command;

const TASK_NAME: &str = "asus-control";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn schtasks(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("schtasks: {e}"))
}

/// Is the machine-wide autostart task registered?
pub fn is_enabled() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn task_xml(exe: &str) -> String {
    // BUILTIN\Users = S-1-5-32-545. A LogonTrigger with no UserId means "any
    // user". HighestAvailable gives admins a silent elevated start; a standard
    // user still gets a launch, just unelevated (where the driver won't load).
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>asus-control fan control (starts for any user who logs on)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <GroupId>S-1-5-32-545</GroupId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// Where we remember that somebody deliberately turned autostart OFF.
///
/// This has to live machine-wide, not in the per-user config: the task itself
/// is machine-wide, so "I don't want this starting on boot" is a decision about
/// the PC. ProgramData is the natural place and we are already elevated.
fn optout_marker() -> std::path::PathBuf {
    let base = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    std::path::PathBuf::from(base).join("asus-control").join("autostart-optout")
}

/// On by default: register the task the first time we ever run, unless someone
/// has explicitly opted out. Without the marker we would recreate the task on
/// every launch and the off switch would not stick.
pub fn ensure_default() {
    if is_enabled() || optout_marker().exists() {
        return;
    }
    let _ = set(true);
}

/// Register or remove the task. Requires administrator, which we already are.
pub fn set(on: bool) -> Result<(), String> {
    let marker = optout_marker();
    if on {
        let _ = std::fs::remove_file(&marker);
    } else if let Some(dir) = marker.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&marker, b"autostart disabled by a user of this PC\r\n");
    }

    if !on {
        let o = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
        return if o.status.success() || !is_enabled() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
        };
    }

    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned();

    // schtasks wants the definition as a file; it reads UTF-16 with a BOM.
    let dir = std::env::temp_dir();
    let path = dir.join("asus-control-task.xml");
    let xml = task_xml(&exe);
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, &bytes).map_err(|e| format!("write task xml: {e}"))?;

    let o = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &path.to_string_lossy(), "/F"])?;
    let _ = std::fs::remove_file(&path);

    if o.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&o.stderr);
        let out = String::from_utf8_lossy(&o.stdout);
        Err(format!("{} {}", err.trim(), out.trim()).trim().to_string())
    }
}
