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
- `bridge/publish/bridge.exe` (+ DLLs) — .NET sensor bridge (optional but
  recommended). Build it yourself with the `dotnet publish` line below.

Plus a port-I/O driver — see below. PawnIO is preferred; WinRing0 is used
automatically if PawnIO is not there.

## Port I/O driver

Reading the SuperIO means talking to ISA I/O ports, which needs a kernel driver.
Two backends, picked at startup in this order:

### 1. PawnIO (preferred)

[PawnIO](https://pawnio.eu) is a modern, signed, HVCI-compatible driver that runs
small verified bytecode *modules* in the kernel instead of handing user mode a raw
"write any port" ioctl. It replaces WinRing0 because WinRing0 is exactly that raw
primitive: a 2007-signed driver on Microsoft's vulnerable-driver blocklist, blocked
outright on machines with HVCI / memory integrity on. PawnIO's `LpcIO` module also
confines us to the I/O BARs it discovered on the SuperIO chip, so a bug here cannot
scribble over an unrelated port.

Two pieces:

- **PawnIO itself** — install from [pawnio.eu](https://pawnio.eu). We load
  `PawnIOLib.dll` dynamically (`LoadLibrary`), never statically: the library is
  LGPL-2.1 and this app is MIT. Looked up on `PATH` first, then in
  `%ProgramFiles%\PawnIO\`.
- **`LpcIO.bin`** — the signed SuperIO module. **Not shipped in this repo** (LGPL,
  and it should come from upstream unmodified). Grab `LpcIO.bin` out of
  `release_0_2_10.zip` at
  [PawnIO.Modules releases](https://github.com/namazso/PawnIO.Modules/releases) and
  drop it next to `asus-control.exe`. Also picked up from `%ProgramFiles%\PawnIO\`
  or `%ProgramFiles%\PawnIO\modules\` if the installer put it there.

Missing either piece is not an error — it just falls back.

### 2. WinRing0 (legacy fallback)

- `WinRing0x64.sys` — signed port-I/O driver (LibreHardwareMonitor's WinRing0 fork).
  **Not shipped in this repo** — copy it out of any [LibreHardwareMonitor]
  (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases)
  release. Expected SHA-256 of the build this was developed against:
  `11bd2c9f9e2397c9a16e0990e4ed2cf0679498fe0fd418a3dfdac60b5c160ee5`.
  Be aware of what it is: a driver that grants ring-0 port I/O and MSR access
  to user mode. That is the whole point of it here, and it is also why WinRing0
  shows up in bring-your-own-vulnerable-driver attacks, why it is on the blocklist,
  and why it will not load under HVCI. Install it knowingly.

`asus-control.exe --check` prints which backend actually loaded, its version, and —
when it fell back — why PawnIO was skipped.

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

1. `ring0.rs` picks a backend and exposes one `read_port_byte`/`write_port_byte`
   pair to the rest of the app. `pawnio.rs` loads `PawnIOLib.dll` + the `LpcIO`
   module, selects the 0x2E or 0x4E slot, runs `ioctl_find_bars` once (under the ISA
   mutex, since it walks all 255 logical devices) so the HWM window at 0x290 becomes
   accessible, then maps SuperIO index/data accesses onto `ioctl_superio_inb/outb`
   and everything else onto `ioctl_pio_inb/outb`. If that fails, `ring0.rs` installs
   & loads the WinRing0_1_2_0 service and does port I/O via DeviceIoControl
   (LHM-fork ioctls: READ_PORT_BYTE = CTL_CODE(40000,0x833,RD)).
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
