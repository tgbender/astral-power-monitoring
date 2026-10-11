# Alert sound previews

These original sounds are synthesized from mathematical oscillators and contain
no third-party recordings, melodies, or samples. They are supplied under the
repository's [Unlicense](../../LICENSE), with no attribution requirement.

| File | Pattern | Intended meaning |
| --- | --- | --- |
| warning.wav | Short ascending 660 / 880 Hz pair | Sustained elevated current |
| critical.wav | Three 1100 Hz pulses | Critical current |
| limited.wav | Descending 880 / 440 Hz pair | Emergency GPU limits applied |
| shutdown.wav | Alternating 880 / 1320 Hz, one-second cycle | Power-off countdown |

All files are mono, 48 kHz, 16-bit PCM WAV. Smooth attack and release envelopes
avoid clicks; the waveform has ample headroom. Playback loudness still depends
on the user's device and volume settings.

These are previews only: they are not yet connected to app events. When integrated,
warning and intervention sounds should play once per event; the shutdown cycle
can repeat during the existing five-second countdown. Audio must not block the
sensor worker or delay protection. A higher-priority alert should replace lower
priority audio instead of overlapping it.

Reproduce from the repository root, using the standalone Rust generator:

```powershell
rustc --edition 2024 tools/generate_alerts.rs -o dist/generate-alerts.exe
& .\dist\generate-alerts.exe
```

The generator is an offline development tool, not a runtime dependency of the app.
