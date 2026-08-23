# asus-control

Lightweight fan control + hardware monitor for ASUS ROG CROSSHAIR X870E EXTREME (works on other NCT6798D-based boards too). Replaces Armoury Crate's Fan Xpert.

Pure Rust on raw Win32 + GDI/GDI+ — no UI framework, no runtime. 440 KB binary,
per-monitor-v2 DPI aware, dark title bar.

## Features

- Live temperatures from the motherboard SuperIO (NCT6798D @ 0x290): PECI/CPU, SYS, AUX0-4, SMBUS, TSENSOR
- Live fan RPMs for all 7 headers + current PWM duty
- Software fan curves: drag points in the editor, double-click adds, per-fan enable
- Presets: Default / Quiet / Full Speed
- Extra sensors via LibreHardwareMonitor bridge: GPU temp/load, NVMe temps, RAM, CPU load
- "Release all fans" restores firmware/BIOS control instantly

## Run

Requires administrator (driver load). Launch `deploy/asus-control.exe` and accept UAC.

Files needed next to the exe:
- `WinRing0x64.sys` — signed port-I/O driver (LibreHardwareMonitor's WinRing0 fork).
  **Not shipped in this repo** — copy it out of any [LibreHardwareMonitor]
  (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases)
  release. Expected SHA-256 of the build this was developed against:
  `11bd2c9f9e2397c9a16e0990e4ed2cf0679498fe0fd418a3dfdac60b5c160ee5`.
  Be aware of what it is: a driver that grants ring-0 port I/O and MSR access
  to user mode. That is the whole point of it here, and it is also why WinRing0
  shows up in bring-your-own-vulnerable-driver attacks. Install it knowingly.
- `bridge/publish/bridge.exe` (+ DLLs) — .NET sensor bridge (optional but
  recommended). Build it yourself with the `dotnet publish` line below.

## Elevated shell (`tools/admin.ps1`)

One UAC prompt, then any number of admin commands — handy for rebuild/redeploy
loops where every launch would otherwise re-prompt.

```
.	oolsdmin.ps1 -Start        # UAC prompt -> elevated listener window
.	oolsdmin.ps1 <command>     # run <command> in it, output comes back here
.	oolsdmin.ps1 -Status
.	oolsdmin.ps1 -Stop         # or just close the listener window
```

The listener is a live PowerShell session, so `cd`/`$env:`/variables persist
between commands. Its named pipe is ACL'd to the launching user's SID only.

## Self tests

```
asus-control.exe --check     # driver + SuperIO detection + sensor dump + bridge
asus-control.exe --testfan   # ramps CPU_FAN 30/60/90% and prints RPM (proof of control)
```

## How it works

1. `ring0.rs` installs & loads WinRing0_1_2_0 service, then does port I/O via
   DeviceIoControl (LHM-fork ioctls: READ_PORT_BYTE = CTL_CODE(40000,0x833,RD)).
2. `nct6798d.rs` enters SuperIO config mode on 0x2E/0x4E, selects LDN 0x0B,
   clears the HM IO-space lock (cfg 0x28 bit 4), reads the HWM base (0x290),
   then banked-register access for temps (0x073+), 13-bit fan tachs (0x4B0+),
   PWM command/mode regs (0x109/0x102 pattern).
3. `worker.rs` polls at a configurable tick (default 500 ms), interpolates each
   enabled fan's curve at the control temperature, writes duty.
   `ui.rs` computes every panel rect and child-control position in one
   `relayout()`, so painted panels and controls cannot drift into each other.
4. `sensors.rs` spawns the C# LibreHardwareMonitor bridge (JSON lines over stdout).

## Build

```
cargo build --release
# output: target/release/asus-control.exe  -> copy into deploy/
dotnet publish bridge -c Release -r win-x64 --self-contained false -o bridge/publish
```

## License

MIT — see [LICENSE](LICENSE). Third-party components are listed there too.
