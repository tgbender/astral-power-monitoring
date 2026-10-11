# Embedded alert sounds

These original sounds are synthesized from mathematical oscillators and contain
no third-party recordings, melodies, or samples. They are supplied under the
repository's [Unlicense](../../LICENSE), with no attribution requirement.

| File | Pattern | Intended meaning |
| --- | --- | --- |
| warning.wav | Short ascending 660 / 880 Hz pair | Sustained elevated current |
| critical.wav | Three 1100 Hz pulses | Critical current |
| limited.wav | Descending 880 / 440 Hz pair | Emergency GPU limits applied |
| shutdown.wav | Alternating 880 / 1320 Hz, one-second cycle | Power-off countdown |
| monitoring-lost.wav | Slower descending 660 / 440 / 330 Hz triple | Sensor monitoring unavailable |
| monitoring-recovered.wav | Short ascending 440 / 660 Hz pair | Stable fresh readings restored |

All files are mono, 48 kHz, 16-bit PCM WAV. Smooth attack and release envelopes
avoid clicks; the waveform has ample headroom. Playback loudness still depends
on the user's device and volume settings.

The app embeds these sounds and plays them on a dedicated audio thread. Warning
and intervention sounds play once per event; the shutdown cycle repeats for up to
five seconds during an accepted shutdown countdown and stops when disarmed.
Higher-priority audio interrupts lower-priority audio instead of overlapping it.
The tray's Test all sounds command plays one cycle of each without invoking
GPU controls or Windows shutdown. See the main README for thresholds and muting.

Reproduce from the repository root, using the standalone Rust generator:

```powershell
rustc --edition 2024 tools/generate_alerts.rs -o dist/generate-alerts.exe
& .\dist\generate-alerts.exe
```

The generator is an offline development tool, not a runtime dependency of the app.
