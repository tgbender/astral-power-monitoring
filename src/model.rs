use std::path::PathBuf;
use std::time::{Duration, Instant};
pub const WARNING_AMPS: f32 = 9.2;
pub const STALE_AFTER: Duration = Duration::from_secs(3);
#[derive(Clone, Copy, Debug)]
pub struct Pin {
    pub volts: f32,
    pub amps: f32,
}
impl Pin {
    pub fn watts(self) -> f32 {
        self.volts * self.amps
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    pub pins: [Pin; 6],
}
impl Reading {
    pub fn decode(raw: &[u8]) -> Result<Self, String> {
        if raw.len() != 24 {
            return Err("Incomplete sensor packet".into());
        }
        let mut pins = [Pin {
            volts: 0.0,
            amps: 0.0,
        }; 6];
        for (index, pin) in pins.iter_mut().enumerate() {
            let offset = (5 - index) * 4;
            pin.volts = u16::from_be_bytes([raw[offset], raw[offset + 1]]) as f32 / 1000.0;
            pin.amps = u16::from_be_bytes([raw[offset + 2], raw[offset + 3]]) as f32 / 1000.0;
            if !(1.0..=16.0).contains(&pin.volts) || !(0.0..=30.0).contains(&pin.amps) {
                return Err(format!("Invalid reading on pin {}", index + 1));
            }
        }
        Ok(Self { pins })
    }
    pub fn watts(self) -> f32 {
        self.pins.iter().map(|p| p.watts()).sum()
    }
    pub fn max_amps(self) -> f32 {
        self.pins.iter().map(|p| p.amps).fold(0.0, f32::max)
    }
}
pub struct State {
    pub reading: Option<Reading>,
    pub updated: Option<Instant>,
    pub error: Option<String>,
    pub gpu_name: String,
    pub peaks: [f32; 6],
    pub samples: u64,
    pub log_path: PathBuf,
    pub protection_status: String,
    pub control_detail: String,
    pub control_readiness: String,
    pub monitor_only: bool,
}
impl State {
    pub fn new(log_path: PathBuf) -> Self {
        Self {
            reading: None,
            updated: None,
            error: None,
            gpu_name: "Connecting to GPU…".into(),
            peaks: [0.0; 6],
            samples: 0,
            log_path,
            protection_status: "Protection OFF · arm manually from the tray menu".into(),
            control_detail: "Checking GPU control availability…".into(),
            control_readiness: "Checking GPU control availability…".into(),
            monitor_only: false,
        }
    }
    pub fn record(&mut self, reading: Reading, now: Instant) {
        for (peak, pin) in self.peaks.iter_mut().zip(reading.pins) {
            *peak = peak.max(pin.amps);
        }
        self.reading = Some(reading);
        self.updated = Some(now);
        self.error = None;
        self.samples += 1;
    }
    pub fn live_at(&self, now: Instant) -> Option<Reading> {
        if self.error.is_some() {
            return None;
        }
        self.updated
            .filter(|updated| now.saturating_duration_since(*updated) <= STALE_AFTER)?;
        self.reading
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn packet() -> [u8; 24] {
        let mut raw = [0; 24];
        for pin in 0..6 {
            let offset = (5 - pin) * 4;
            raw[offset..offset + 2].copy_from_slice(&12000u16.to_be_bytes());
            raw[offset + 2..offset + 4].copy_from_slice(&((pin as u16 + 1) * 1000).to_be_bytes());
        }
        raw
    }
    #[test]
    fn decodes_pin_order_and_units() {
        let r = Reading::decode(&packet()).unwrap();
        assert_eq!(r.pins[0].amps, 1.0);
        assert_eq!(r.pins[5].amps, 6.0);
        assert_eq!(r.watts(), 252.0);
    }
    #[test]
    fn rejects_truncated_zero_and_garbage_packets() {
        assert!(Reading::decode(&[0; 23]).is_err());
        assert!(Reading::decode(&[0; 24]).is_err());
        assert!(Reading::decode(&[255; 24]).is_err());
    }
    #[test]
    fn preserves_real_high_currents() {
        let mut raw = packet();
        raw[22..24].copy_from_slice(&22000u16.to_be_bytes());
        assert_eq!(Reading::decode(&raw).unwrap().max_amps(), 22.0);
    }
    #[test]
    fn stale_or_failed_samples_are_never_live() {
        let mut state = State::new(PathBuf::new());
        let now = Instant::now();
        state.record(Reading::decode(&packet()).unwrap(), now);
        assert!(state.live_at(now).is_some());
        assert!(state.live_at(now + Duration::from_secs(4)).is_none());
        state.error = Some("disconnected".into());
        assert!(state.live_at(now).is_none());
        state.record(
            Reading::decode(&packet()).unwrap(),
            now + Duration::from_secs(5),
        );
        assert!(state.live_at(now + Duration::from_secs(5)).is_some());
        assert_eq!(state.peaks[5], 6.0);
    }
}
