//! Offline, dependency-free generator. Not part of the monitoring executable.
use std::f64::consts::PI;
use std::fs;
use std::path::Path;

const RATE: u32 = 48_000;
const GAIN: f64 = 0.36;

fn silence(samples: &mut Vec<i16>, seconds: f64) {
    samples.resize(samples.len() + (seconds * RATE as f64).round() as usize, 0);
}

fn tone(samples: &mut Vec<i16>, hz: f64, seconds: f64) {
    let count = (seconds * RATE as f64).round() as usize;
    for index in 0..count {
        let time = index as f64 / RATE as f64;
        let remaining = (count - 1 - index) as f64 / RATE as f64;
        // Smooth attack/release avoids clicks. Slight harmonics make tones easier
        // to distinguish from pure sine waves without introducing harsh edges.
        let attack = (time / 0.008).min(1.0);
        let release = (remaining / 0.060).min(1.0);
        let envelope = (0.5 - 0.5 * (PI * attack).cos()) * (0.5 - 0.5 * (PI * release).cos());
        let phase = 2.0 * PI * hz * time;
        let wave = 0.84 * phase.sin() + 0.12 * (2.0 * phase).sin() + 0.04 * (3.0 * phase).sin();
        let value = GAIN * envelope * wave;
        assert!(value.abs() <= 1.0);
        samples.push((value * i16::MAX as f64).round() as i16);
    }
}

fn save(folder: &Path, name: &str, samples: &[i16]) {
    let bytes = u32::try_from(samples.len() * 2).unwrap();
    let mut wav = Vec::with_capacity(bytes as usize + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(bytes + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // Mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // Bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&bytes.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(folder.join(name), wav).unwrap();
    println!("{name}: {:.2} s", samples.len() as f64 / RATE as f64);
}

fn main() {
    let folder = Path::new("assets/sounds");
    fs::create_dir_all(folder).unwrap();

    // Gentle ascending pair: an early warning, not yet an intervention.
    let mut warning = Vec::new();
    tone(&mut warning, 660.0, 0.12);
    silence(&mut warning, 0.08);
    tone(&mut warning, 880.0, 0.16);
    silence(&mut warning, 0.08);
    save(folder, "warning.wav", &warning);

    // Three faster high pulses: escalation above the critical tier.
    let mut critical = Vec::new();
    for _ in 0..3 {
        tone(&mut critical, 1100.0, 0.12);
        silence(&mut critical, 0.10);
    }
    save(folder, "critical.wav", &critical);

    // Descending pair: the app has applied emergency GPU limits.
    let mut limited = Vec::new();
    tone(&mut limited, 880.0, 0.16);
    silence(&mut limited, 0.06);
    tone(&mut limited, 440.0, 0.24);
    silence(&mut limited, 0.08);
    save(folder, "limited.wav", &limited);

    // Monitoring is blind: a slower, lower descending triple, distinct from limiting.
    let mut lost = Vec::new();
    tone(&mut lost, 660.0, 0.18);
    silence(&mut lost, 0.08);
    tone(&mut lost, 440.0, 0.18);
    silence(&mut lost, 0.08);
    tone(&mut lost, 330.0, 0.26);
    silence(&mut lost, 0.12);
    save(folder, "monitoring-lost.wav", &lost);

    // Short confirmation only after the watchdog verifies stable fresh readings.
    let mut recovered = Vec::new();
    tone(&mut recovered, 440.0, 0.09);
    silence(&mut recovered, 0.07);
    tone(&mut recovered, 660.0, 0.12);
    silence(&mut recovered, 0.08);
    save(folder, "monitoring-recovered.wav", &recovered);

    // One second of alternating tones, suitable for a countdown loop.
    let mut shutdown = Vec::new();
    for _ in 0..2 {
        tone(&mut shutdown, 880.0, 0.17);
        silence(&mut shutdown, 0.04);
        tone(&mut shutdown, 1320.0, 0.17);
        silence(&mut shutdown, 0.04);
    }
    silence(&mut shutdown, 0.16);
    save(folder, "shutdown.wav", &shutdown);
}
