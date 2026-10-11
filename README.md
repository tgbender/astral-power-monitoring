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
- Saves a JSONL sample about once per second to `%LOCALAPPDATA%\AstralPowerMonitor\readings.jsonl`.
- Records startup and sensor connection/unavailability events alongside samples.
- Rotates JSONL at 5 MiB and retains three older files (`readings.1.jsonl` through `.3.jsonl`).
- Repairs an incomplete final record on startup. Existing CSV history is preserved.
- Allows only one GUI instance per Windows session.
- Provides manually armed GPU limiting and Windows power-off protection using the Python guard's default ladder.
- Plays embedded audio warnings even while protection is disarmed. Audio playback runs separately from sensor reads and protective actions.

**Every launch starts disarmed.** Until you explicitly arm protection, the app reads
sensors without changing the GPU or shutting Windows down. Current readings are not connector
temperature measurements or a guarantee against connector damage.

## Using the app

Launch `AstralPowerMonitor.exe` (the release build is named `astral_power_monitoring.exe`).
Closing the window hides it while monitoring continues. Click the tray icon to show it.
Right-click the tray icon or the readings window for Show readings, Open JSONL history,
Reset session peaks, Arm protection, Disarm / Cancel, Restore saved GPU settings,
Test all four sounds, Mute warning chime, and Quit.
Launching it again restores the existing window.

Use `--tray` to start with the window hidden. Launching the executable alone does not
install startup entries. The optional installation below creates a shortcut for this user.
Use `--monitor-only` to prohibit arming for the entire session; launching with this flag
also disarms an existing instance when Windows permits messaging that instance.

## Sound alerts

The original WAVs are embedded in the executable; no external sound files or Python
runtime are needed. Playback uses asynchronous Windows audio on a dedicated thread.
Audio errors appear in the window and never stop sampling or delay protective actions.

- At least 9.5 A continuously for two seconds: ascending warning chime, once per episode.
- At least 13 A: critical triple tone immediately, once per episode. This early sound
  does not change the original three-second protection timer.
- GPU limiting successfully activates: descending intervention chime.
- Windows accepts a shutdown request: alternating countdown alarm for up to five seconds.
  It stops when disarmed/cancelled, when the app quits, or when its timer expires.

Warning episodes re-arm after fresh current below 9.3 A for one second. Sensor errors
and gaps cannot count toward the sustained warning or clear an existing warning episode.
Higher-priority alerts interrupt lower-priority sounds; intervention waits for a playing
critical tone, while a shutdown alarm supersedes both. Nothing beeps every sensor poll.

**Mute warning chime** only mutes the early 9.5 A chime. Critical, intervention, and
shutdown tones remain audible. Mute is not saved across app launches. **Test all four
sounds** plays one preview of each and never arms protection, changes GPU settings,
or schedules a shutdown. Test audio cannot replace an active real alert. Windows volume
and the selected output device still determine whether any sound can be heard.

## Start at Windows login

After copying the release build to `dist\AstralPowerMonitor.exe`, quit the current
monitor and run `scripts\Install-Startup.ps1`. This copies the executable to
`%LOCALAPPDATA%\AstralPowerMonitor\AstralPowerMonitor.exe` and creates
`Astral Power Monitor.lnk` in this user's Windows Startup folder, targeting `--tray`.
It does not install a service, scheduled task, restart policy, or automatic arming.
The installation is specific to the current user and uses their ordinary login privileges.
Every login starts with protection disarmed and sound warnings enabled.

To enable GPU limiting, quit that ordinary instance and run the installed executable
as administrator before manually arming. To disable startup, run
`scripts\Remove-Startup.ps1` or remove the named startup shortcut. The removal script
verifies its target and preserves the executable, history, and recovery files.

## Protection (manual arming)

Run as administrator to permit NVIDIA clock/power changes, then choose **Arm protection
for this session** and confirm. Arming is never saved, enabled by a command-line flag,
or enabled automatically after a restart. Ordinary monitoring needs no administrator access.

The highest current across all six valid pins drives the original Python defaults:

| Trigger | First response | Power-off escalation |
| --- | --- | --- |
| At least 9.5 A for 15 s | Lock GPU clocks low and set the driver-reported minimum power limit | Current remains at least 9.5 A after 10 s |
| At least 13 A for 3 s | Same GPU limiting | Current remains at least 9.5 A after 5 s |
| At least 16 A on two consecutive readings | Attempt GPU limiting | Request power-off immediately |

The grace period belongs to the first mitigation; later crossings do not restart it,
matching Python. A successful clock lock or power-cap change counts as mitigation,
even if the other change fails. If both fail or NVML cannot control the matching card,
the triggering tier requests power-off directly, also matching Python. Driver write
permissions cannot be proved with the read-only readiness check: a non-elevated session
may fall back to power-off at the trigger instead of limiting first.

On this 5090, the driver reports a 400 W minimum. Clocks use the lowest enumerated
graphics frequency, falling back to 500, 700, then 1000 MHz if necessary. The previous
power cap is saved; restoration does not assume 600 W. The app resets its GPU clock
lock on restoration rather than preserving a pre-existing manual clock lock. Avoid
having another utility change these settings while protection is engaged.

Limits remain for at least 120 s and are automatically restored only after 60 s of
readings strictly below 9.3 A and a cleared grace period. A tier that re-triggers within
600 s of that automatic release attempts limiting and requests power-off directly.
Tier 1 uses the Python 0.2 A hysteresis and one-second clear window. Read failures
or gaps longer than five seconds reset observation timers without releasing an
existing GPU limit; missing readings never satisfy the clean-release window.

Power-off invokes System32 `shutdown.exe /s /t 5 /f`, **not restart**. It forces apps
closed after five seconds, which can discard unsaved work. A rejected request is
logged while sampling continues; the next qualifying sample retries. An accepted
request is latched to avoid repeated requests during its countdown.

**Escape hatches:**

- **Disarm / Cancel pending power-off** disables actions immediately and attempts to
  abort this app's pending countdown independently of the sensor thread. Cancellation
  can only work before the five-second countdown expires. Emergency GPU limits stay
  in place until you choose **Restore saved GPU settings (disarms)**.
- **Quit** disarms and attempts to cancel a pending app shutdown. Closing the window
  only hides it. Quitting deliberately retains emergency GPU limits for later restoration.
- `AstralPowerMonitor.exe --cancel-shutdown` directly aborts a pending Windows shutdown
  even if the GUI is hung. This aborts any Windows shutdown countdown, not just this app's.
  Windows may require an elevated terminal to abort an elevated request.
- `--monitor-only` disables arming; when targeting an already elevated instance, run
  it with matching privileges or use that instance's tray menu. If Windows blocks the
  message, the launch reports failure instead of claiming the existing app is disarmed.

Recovery intent and the original cap are flushed to
`%LOCALAPPDATA%\AstralPowerMonitor\mitigation.json` before driver writes. After a crash,
the app remains disarmed and requires manual restoration before arming again. Failed
restoration retains the marker for retry; successful individual changes are tracked so
retries only restore remaining changes. GPU UUID and power bounds are checked before
restoring. `last_shutdown.json` retains the last protective shutdown request and reason.
Neither recovery file causes automatic arming or automatic GPU writes on startup.

For a bounded console snapshot in PowerShell:

```powershell
& .\target\release\astral_power_monitoring.exe --once | Out-String
& .\target\release\astral_power_monitoring.exe --samples 3 | Out-String
```

## Build and validate

Requires x64 Windows 10/11, Rust 1.88 or later, the MSVC toolchain/Windows SDK,
and an installed NVIDIA driver. Python and HWiNFO are not required. Administrator access
is needed for GPU limiting, not read-only monitoring.

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

The native interface uses Windows controls and GDI. The compiled executable, including
the embedded sounds, is roughly 870 KiB. Sensor access uses NVIDIA's installed NVAPI driver library; GPU limiting uses
NVML loaded exclusively from System32. Control selection requires one unambiguous match
to the monitored card's subsystem ID, with UUID checks for saved-state restoration.

The tests do not access GPU hardware. They cover pin ordering and units, malformed packets,
high-current readings, stale/disconnected state, card selection, the native request layout,
JSONL writing/rotation/tail recovery, every protection tier, grace periods, release/re-trigger,
failed actions, recovery markers, disarm/cancel races, audio thresholds, priority,
muting, countdown cancellation, and playback failures. Protection/audio tests use fake actions
and never shut down Windows or change the real GPU. Live hardware validation is a separate
`--once` check; `--check-control` queries NVML without changing settings. Actual driver
writes and Windows shutdown have not been exercised on the live machine during development.

Each JSONL line is a JSON object with `schema_version`, `unix_time_ms`, and `type`.
Sample records also contain `gpu`, a six-element `pins` array (`pin`, `volts`, `amps`,
`watts`), and `connector_watts`. Event types are `startup`, `sensor_connected`,
`sensor_unavailable`, `alert`, and `protection`. Advisory alert records include `tone`,
`max_pin_amps`, and `muted`. Protection events include an `action` and, where
applicable, `detail` and `max_pin_amps`. Repeated identical sensor errors are not logged
on every retry.

## Sensor protocol and supported cards

The IT8915FN sensor is read through `NvAPI_I2CReadEx`, address `0x2B`, register `0x80`,
port 1. The 24-byte response contains six reversed blocks of big-endian millivolts
and milliamps. Pin 1 is at bytes 20–23 and Pin 6 at bytes 0–3.

The reader selects the first supported ASUS subsystem ID rather than assuming GPU 0.
Supported IDs include the Astral 5090 OC, OC White, LC, 5090D OC, Matrix 5090,
and the listed Astral 5080 variants. The OC White model was checked on real hardware.
BTF models are excluded because slot power can bypass their cable sensors.
No I2C write function is bound.
