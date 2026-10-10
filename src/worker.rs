use crate::{model::State, monitor::AstralPowerMonitor};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
pub type Shared = Arc<Mutex<State>>;
pub struct Worker {
    pub shared: Shared,
    _stop: mpsc::Sender<()>,
}
pub fn log_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("AstralPowerMonitor")
        .join("readings.csv")
}
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
struct CsvLog {
    file: Option<File>,
    path: PathBuf,
}
impl CsvLog {
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if file.metadata()?.len() == 0 {
            writeln!(
                file,
                "unix_time,pin1_v,pin1_a,pin2_v,pin2_a,pin3_v,pin3_a,pin4_v,pin4_a,pin5_v,pin5_a,pin6_v,pin6_a,connector_w"
            )?;
        }
        Ok(Self {
            file: Some(file),
            path: path.into(),
        })
    }
    fn write(&mut self, reading: crate::model::Reading) -> io::Result<()> {
        if self.file.as_ref().is_some_and(|f| {
            f.metadata()
                .map(|m| m.len() >= MAX_LOG_BYTES)
                .unwrap_or(false)
        }) {
            // Close the Windows handle before rotating; retain three old CSVs.
            self.file.take();
            for number in (1..=3).rev() {
                let target = self.path.with_extension(format!("{number}.csv"));
                if target.exists() {
                    std::fs::remove_file(&target)?;
                }
                let source = if number == 1 {
                    self.path.clone()
                } else {
                    self.path.with_extension(format!("{}.csv", number - 1))
                };
                if source.exists() {
                    std::fs::rename(source, target)?;
                }
            }
            *self = Self::open(&self.path)?;
        }
        // A failed rotation may leave no handle: try reopening on the next sample.
        if self.file.is_none() {
            *self = Self::open(&self.path)?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("CSV unavailable"))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        write!(file, "{stamp:.3}")?;
        for pin in reading.pins {
            write!(file, ",{:.3},{:.3}", pin.volts, pin.amps)?;
        }
        writeln!(file, ",{:.3}", reading.watts())?;
        file.flush()
    }
}
impl Worker {
    pub fn start() -> Self {
        let shared = Arc::new(Mutex::new(State::new(log_path())));
        let output = shared.clone();
        let (stop, stopped) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let mut log = match CsvLog::open(&log_path()) {
                Ok(log) => Some(log),
                Err(e) => {
                    output.lock().unwrap_or_else(|p| p.into_inner()).log_error =
                        Some(e.to_string());
                    None
                }
            };
            let mut reader: Option<AstralPowerMonitor> = None;
            let mut logged = Instant::now() - Duration::from_secs(2);
            loop {
                let result = (|| {
                    if reader.is_none() {
                        reader = Some(AstralPowerMonitor::new()?);
                    }
                    let monitor = reader.as_ref().ok_or("Sensor disconnected")?;
                    monitor.read().map(|reading| (monitor.name, reading))
                })();
                let wait = match result {
                    Ok((name, reading)) => {
                        let now = Instant::now();
                        {
                            let mut state = output.lock().unwrap_or_else(|p| p.into_inner());
                            state.gpu_name = name.into();
                            state.record(reading, now);
                        }
                        if now.duration_since(logged) >= Duration::from_secs(1) {
                            logged = now;
                            if let Some(log) = &mut log {
                                let error = log.write(reading).err().map(|e| e.to_string());
                                output.lock().unwrap_or_else(|p| p.into_inner()).log_error = error;
                            }
                        }
                        Duration::from_millis(500)
                    }
                    Err(error) => {
                        output.lock().unwrap_or_else(|p| p.into_inner()).error = Some(error);
                        reader = None;
                        Duration::from_secs(5)
                    }
                };
                match stopped.recv_timeout(wait) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        Self {
            shared,
            _stop: stop,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_contains_six_pins_and_a_flushed_complete_row() {
        let folder = std::env::temp_dir().join(format!("astral-csv-test-{}", std::process::id()));
        let path = folder.join("readings.csv");
        let mut log = CsvLog::open(&path).unwrap();
        log.write(crate::model::Reading {
            pins: [crate::model::Pin {
                volts: 12.0,
                amps: 1.0,
            }; 6],
        })
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.lines().last().unwrap().ends_with(",72.000"));
        assert_eq!(text.lines().last().unwrap().split(',').count(), 14);
        drop(log);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn rotates_logs_and_keeps_only_three_archives() {
        let folder =
            std::env::temp_dir().join(format!("astral-csv-rotation-{}", std::process::id()));
        let path = folder.join("readings.csv");
        let mut log = CsvLog::open(&path).unwrap();
        for amps in 1..=5 {
            if amps > 1 {
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(MAX_LOG_BYTES)
                    .unwrap();
            }
            log.write(crate::model::Reading {
                pins: [crate::model::Pin {
                    volts: 12.0,
                    amps: amps as f32,
                }; 6],
            })
            .unwrap();
        }
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with(",360.000\n")
        );
        assert!(!path.with_extension("4.csv").exists());
        drop(log);
        for file in [
            path.clone(),
            path.with_extension("1.csv"),
            path.with_extension("2.csv"),
            path.with_extension("3.csv"),
        ] {
            std::fs::remove_file(file).unwrap();
        }
        std::fs::remove_dir(folder).unwrap();
    }
}
