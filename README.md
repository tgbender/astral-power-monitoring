# Astral Power Monitor for Windows

A small native Rust tray application for ASUS Astral per-pin power monitoring,
extended from [Timic3/astral-power-monitoring](https://github.com/Timic3/astral-power-monitoring).

## What it does

- Displays current, voltage, calculated power, and session peak current for all six power pins.
- Shows total connector power and the highest pin current.
- Samples twice per second on a worker thread, independently of the window.
- Marks readings unavailable after a read failure or three seconds without a fresh sample.
- Reconnects every five seconds after sensor/driver failures.
- Displays a warning at 9.2 A; this is a display warning, not an automatic intervention.
- Saves a CSV sample about once per second to `%LOCALAPPDATA%\AstralPowerMonitor\readings.csv`.
- Rotates the CSV at 5 MiB and retains three older files.
- Allows only one GUI instance per Windows session.

**This version is monitoring only. It does not change clocks, voltage, power limits,
or fan speeds, and does not shut Windows down.** Current readings are not connector
temperature measurements or a guarantee against connector damage.

## Using the app

Launch `AstralPowerMonitor.exe` (the release build is named `astral_power_monitoring.exe`).
Closing the window hides it while monitoring continues. Click the tray icon to show it.
Right-click the tray icon or the readings window for Show readings, Open CSV history,
Reset session peaks, and Quit. Launching it again restores the existing window.

Use `--tray` to start with the window hidden. No autostart task is installed automatically.

For a bounded console snapshot in PowerShell:

```powershell
& .\target\release\astral_power_monitoring.exe --once | Out-String
& .\target\release\astral_power_monitoring.exe --samples 3 | Out-String
```

## Build and validate

Requires x64 Windows 10/11, Rust 1.88 or later, the MSVC toolchain/Windows SDK,
and an installed NVIDIA driver. Python, HWiNFO, and administrator access are not required.

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

The native interface uses Windows controls and GDI. The compiled executable is roughly
300 KiB. Sensor access uses NVIDIA's installed NVAPI driver library.

The tests do not access GPU hardware. They cover pin ordering and units, malformed packets,
high-current readings, stale/disconnected state, card selection, the native request layout,
and CSV writing/rotation. Live hardware validation is a separate `--once` check.

## Sensor protocol and supported cards

The IT8915FN sensor is read through `NvAPI_I2CReadEx`, address `0x2B`, register `0x80`,
port 1. The 24-byte response contains six reversed blocks of big-endian millivolts
and milliamps. Pin 1 is at bytes 20–23 and Pin 6 at bytes 0–3.

The reader selects the first supported ASUS subsystem ID rather than assuming GPU 0.
Supported IDs include the Astral 5090 OC, OC White, LC, 5090D OC, Matrix 5090,
and the listed Astral 5080 variants. The OC White model was checked on real hardware.
BTF models are excluded because slot power can bypass their cable sensors.
No I2C write function is bound.
