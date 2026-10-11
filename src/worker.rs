use crate::actions::{Gate, RealActions};
use crate::audio::{Audio, Detector, Tone};
use crate::protection::Protection;
use crate::{model::State, monitor::AstralPowerMonitor};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
pub type Shared = Arc<Mutex<State>>;
pub struct Worker {
    pub shared: Shared,
    pub controls: Controls,
    _stop: mpsc::Sender<()>,
}
#[derive(Clone)]
pub struct Controls {
    pub gate: Arc<Gate>,
    pub audio: Audio,
    sender: mpsc::Sender<Control>,
}
enum Control {
    Arm(u64),
    Disarm,
    Restore,
    CancelFailed(String),
}
impl Controls {
    pub fn arm(&self) {
        let _ = self
            .sender
            .send(Control::Arm(self.gate.epoch.load(Ordering::SeqCst)));
    }
    pub fn disarm(&self) {
        self.gate.prevent_actions();
        self.audio.cancel_countdown();
        let _ = self.sender.send(Control::Disarm);
    }
    pub fn restore(&self) {
        self.disarm();
        let gate = self.gate.clone();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let command = match gate.disarm_and_cancel() {
                Ok(()) => Control::Restore,
                Err(e) => Control::CancelFailed(e),
            };
            let _ = sender.send(command);
        });
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.controls.gate.prevent_actions();
        // Cancellation must finish before the process exits, not on a detached task.
        let _ = self.controls.gate.disarm_and_cancel();
        self.controls.audio.quit();
    }
}
pub fn log_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("AstralPowerMonitor")
        .join("readings.jsonl")
}
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
struct JsonlLog {
    file: Option<File>,
    path: PathBuf,
    limit: u64,
}
impl JsonlLog {
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        repair_tail(&mut file)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file: Some(file),
            path: path.into(),
            limit: MAX_LOG_BYTES,
        })
    }
    fn write_record(&mut self, mut record: serde_json::Value) -> io::Result<()> {
        let fields = record
            .as_object_mut()
            .ok_or_else(|| io::Error::other("Log record must be an object"))?;
        fields.insert("schema_version".into(), 1.into());
        fields.insert(
            "unix_time_ms".into(),
            (SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64)
                .into(),
        );
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');
        if self
            .file
            .as_ref()
            .is_some_and(|f| f.metadata().map(|m| m.len() >= self.limit).unwrap_or(false))
        {
            // Close the Windows handle before rotating; retain three old JSONL files.
            self.file.take();
            for number in (1..=3).rev() {
                let target = self.path.with_extension(format!("{number}.jsonl"));
                if target.exists() {
                    std::fs::remove_file(&target)?;
                }
                let source = if number == 1 {
                    self.path.clone()
                } else {
                    self.path.with_extension(format!("{}.jsonl", number - 1))
                };
                if source.exists() {
                    std::fs::rename(source, target)?;
                }
            }
            self.reopen()?;
        }
        // A failed rotation may leave no handle: try reopening on the next sample.
        if self.file.is_none() {
            self.reopen()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("JSONL unavailable"))?;
        let result = file.write_all(&line).and_then(|()| file.flush());
        if result.is_err() {
            // A failed write may have left half a record. Reopen and repair before
            // the next attempt instead of appending onto that partial object.
            self.file.take();
        }
        result
    }
    fn reopen(&mut self) -> io::Result<()> {
        let limit = self.limit;
        *self = Self::open(&self.path)?;
        self.limit = limit;
        Ok(())
    }
    fn sample(&mut self, gpu: &str, reading: crate::model::Reading) -> io::Result<()> {
        let rounded = |value: f32| ((value as f64) * 1000.0).round() / 1000.0;
        let pins: Vec<_> = reading
            .pins
            .iter()
            .enumerate()
            .map(|(index, pin)| {
                serde_json::json!({
                    "pin": index + 1, "volts": rounded(pin.volts), "amps": rounded(pin.amps),
                    "watts": rounded(pin.watts())
                })
            })
            .collect();
        self.write_record(
            serde_json::json!({"type": "sample", "gpu": gpu, "pins": pins,
            "connector_watts": rounded(reading.watts())}),
        )
    }
}

// Preserve complete records. Finish a valid final object missing its newline,
// or discard only the incomplete last record. Scan in bounded chunks.
fn repair_tail(file: &mut File) -> io::Result<()> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    let mut end = length;
    let mut start = 0;
    while end > 0 {
        let begin = end.saturating_sub(4096);
        file.seek(SeekFrom::Start(begin))?;
        let mut chunk = vec![0; (end - begin) as usize];
        file.read_exact(&mut chunk)?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            start = begin + index as u64 + 1;
            break;
        }
        end = begin;
    }
    if length - start <= 64 * 1024 {
        file.seek(SeekFrom::Start(start))?;
        let mut tail = vec![0; (length - start) as usize];
        file.read_exact(&mut tail)?;
        if serde_json::from_slice::<serde_json::Value>(&tail).is_ok_and(|record| record.is_object())
        {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            return Ok(());
        }
    }
    file.set_len(start)
}

fn log_record(log: &mut Option<JsonlLog>, output: &Shared, record: serde_json::Value) {
    if let Some(log) = log {
        let error = log
            .write_record(record)
            .err()
            .map(|error| error.to_string());
        output.lock().unwrap_or_else(|p| p.into_inner()).log_error = error;
    }
}
impl Worker {
    pub fn start(monitor_only: bool) -> Self {
        let shared = Arc::new(Mutex::new(State::new(log_path())));
        shared.lock().unwrap().monitor_only = monitor_only;
        let output = shared.clone();
        let (stop, stopped) = mpsc::channel::<()>();
        let gate = Arc::new(Gate::default());
        let (sender, commands) = mpsc::channel::<Control>();
        let controls = Controls {
            gate: gate.clone(),
            audio: Audio::start(gate.armed.clone()),
            sender,
        };
        let audio = controls.audio.clone();
        std::thread::spawn(move || {
            let mut log = match JsonlLog::open(&log_path()) {
                Ok(log) => Some(log),
                Err(e) => {
                    output.lock().unwrap_or_else(|p| p.into_inner()).log_error =
                        Some(e.to_string());
                    None
                }
            };
            log_record(
                &mut log,
                &output,
                serde_json::json!({"type": "startup", "app_version": env!("CARGO_PKG_VERSION"),"protection_armed":false,"monitor_only":monitor_only}),
            );
            let mut protection = Protection::default();
            let mut detector = Detector::default();
            let folder = log_path().parent().unwrap().to_path_buf();
            let marker = folder.join("mitigation.json");
            let mut actions: Option<RealActions> = None;
            if marker.exists() {
                output
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .control_detail =
                    "Previous emergency GPU limits may remain · use Restore saved GPU settings"
                        .into();
            }
            let mut reader: Option<AstralPowerMonitor> = None;
            let mut logged = Instant::now() - Duration::from_secs(2);
            let mut connected = false;
            let mut last_sensor_error: Option<String> = None;
            loop {
                while let Ok(command) = commands.try_recv() {
                    let event = match command {
                        Control::Arm(epoch) => {
                            let mut state = output.lock().unwrap_or_else(|p| p.into_inner());
                            if epoch != gate.epoch.load(Ordering::SeqCst) {
                                continue;
                            }
                            if state.monitor_only
                                || state.live_at(Instant::now()).is_none()
                                || gate.pending()
                                || marker.exists()
                            {
                                state.control_detail="Cannot arm: monitor-only, stale sensor, pending shutdown, or recovery needed".into();
                                serde_json::json!({"type":"protection","action":"arm_rejected","detail":state.control_detail})
                            } else {
                                if !gate.try_arm(epoch) {
                                    continue;
                                }
                                protection.disarm();
                                state.protection_status =
                                    "Protection ARMED · GPU limiting + power-off enabled".into();
                                state.control_detail = state.control_readiness.clone();
                                serde_json::json!({"type":"protection","action":"armed","detail":state.control_detail})
                            }
                        }
                        Control::Disarm => {
                            protection.disarm();
                            let mut state = output.lock().unwrap_or_else(|p| p.into_inner());
                            state.protection_status =
                                "Protection OFF · emergency GPU limits kept until Restore".into();
                            serde_json::json!({"type":"protection","action":"disarmed"})
                        }
                        Control::Restore => {
                            let result = if gate.pending() {
                                Err("Shutdown cancellation must succeed before restoration".into())
                            } else {
                                actions
                                    .as_mut()
                                    .ok_or_else(|| "GPU controls not connected".into())
                                    .and_then(|a| protection.manual_restore(a))
                            };
                            let mut state = output.lock().unwrap_or_else(|p| p.into_inner());
                            state.control_detail = match &result {
                                Ok(()) => {
                                    "Saved power limit restored; app clock locks removed".into()
                                }
                                Err(e) => format!("Restore failed: {e}"),
                            };
                            serde_json::json!({"type":"protection","action":if result.is_ok(){"manual_restore"}else{"restore_failed"},"detail":state.control_detail})
                        }
                        Control::CancelFailed(error) => {
                            output
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .control_detail =
                                format!("Shutdown cancellation failed; settings kept: {error}");
                            serde_json::json!({"type":"protection","action":"cancel_failed","detail":error})
                        }
                    };
                    log_record(&mut log, &output, event);
                }
                let result = (|| {
                    if reader.is_none() {
                        reader = Some(AstralPowerMonitor::new()?);
                        let monitor = reader.as_ref().unwrap();
                        // Read-only initialization: connect to precisely the sensor's
                        // subsystem; ambiguous multiple-card matches are rejected.
                        actions = Some(RealActions::new(
                            monitor.subsystem,
                            folder.clone(),
                            gate.clone(),
                        ));
                        let readiness = actions.as_ref().unwrap().readiness();
                        let mut state = output.lock().unwrap_or_else(|p| p.into_inner());
                        state.control_readiness = readiness.clone();
                        if !marker.exists() {
                            state.control_detail = readiness;
                        }
                    }
                    let monitor = reader.as_ref().ok_or("Sensor disconnected")?;
                    monitor.read().map(|reading| (monitor.name, reading))
                })();
                let wait = match result {
                    Ok((name, reading)) => {
                        let now = Instant::now();
                        if !connected {
                            log_record(
                                &mut log,
                                &output,
                                serde_json::json!({"type": "sensor_connected", "gpu": name}),
                            );
                            connected = true;
                            last_sensor_error = None;
                        }
                        {
                            let mut state = output.lock().unwrap_or_else(|p| p.into_inner());
                            state.gpu_name = name.into();
                            state.record(reading, now);
                        }
                        // Advisory warnings remain active without arming protection.
                        if let Some(tone) = detector.tick(now, reading.max_amps()) {
                            audio.cue(tone);
                            log_record(
                                &mut log,
                                &output,
                                serde_json::json!({"type":"alert","tone":tone.name(),"max_pin_amps":reading.max_amps(),"muted":tone==Tone::Warning && audio.warning_muted.load(Ordering::SeqCst)}),
                            );
                        }
                        if gate.armed.load(Ordering::SeqCst)
                            && let Some(actions) = actions.as_mut()
                        {
                            for event in protection.tick(now, reading.max_amps(), actions) {
                                match event.action {
                                    "gpu_limited" => audio.cue(Tone::Limited),
                                    "shutdown_requested"
                                        if gate.pending() && gate.armed.load(Ordering::SeqCst) =>
                                    {
                                        audio.cue(Tone::Shutdown);
                                    }
                                    _ => {}
                                }
                                {
                                    let mut state =
                                        output.lock().unwrap_or_else(|p| p.into_inner());
                                    state.control_detail = event.detail.clone();
                                    state.protection_status = if protection.shutdown_pending {
                                        "POWER-OFF requested · 5 s countdown · Disarm/Cancel to abort".into()
                                    } else if protection.limited() {
                                        "Protection ARMED · emergency GPU limits active".into()
                                    } else {
                                        "Protection ARMED · GPU limiting + power-off enabled".into()
                                    };
                                }
                                log_record(
                                    &mut log,
                                    &output,
                                    serde_json::json!({"type":"protection","action":event.action,"detail":event.detail,"max_pin_amps":reading.max_amps()}),
                                );
                            }
                        }
                        if now.duration_since(logged) >= Duration::from_secs(1) {
                            logged = now;
                            if let Some(log) = &mut log {
                                let error = log.sample(name, reading).err().map(|e| e.to_string());
                                output.lock().unwrap_or_else(|p| p.into_inner()).log_error = error;
                            }
                        }
                        Duration::from_millis(500)
                    }
                    Err(error) => {
                        protection.reset_observations();
                        detector.unavailable();
                        if last_sensor_error.as_deref() != Some(&error) {
                            log_record(
                                &mut log,
                                &output,
                                serde_json::json!({"type": "sensor_unavailable", "error": error}),
                            );
                            last_sensor_error = Some(error.clone());
                        }
                        connected = false;
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
            controls,
            _stop: stop,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonl_contains_six_pins_and_complete_typed_records() {
        let folder = std::env::temp_dir().join(format!("astral-jsonl-test-{}", std::process::id()));
        let path = folder.join("readings.jsonl");
        let mut log = JsonlLog::open(&path).unwrap();
        log.write_record(serde_json::json!({"type": "startup"}))
            .unwrap();
        log.sample(
            "Test GPU",
            crate::model::Reading {
                pins: [crate::model::Pin {
                    volts: 12.0,
                    amps: 1.0,
                }; 6],
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[0]["type"], "startup");
        let sample = records.last().unwrap();
        assert_eq!(sample["type"], "sample");
        assert_eq!(sample["schema_version"], 1);
        assert!(sample["unix_time_ms"].as_u64().is_some());
        assert_eq!(sample["gpu"], "Test GPU");
        assert_eq!(sample["connector_watts"], 72.0);
        assert_eq!(sample["pins"].as_array().unwrap().len(), 6);
        assert_eq!(sample["pins"][5]["pin"], 6);
        assert_eq!(sample["pins"][0]["amps"], 1.0);
        assert!(text.ends_with('\n'));
        drop(log);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn rotates_logs_and_keeps_only_three_archives() {
        let folder =
            std::env::temp_dir().join(format!("astral-jsonl-rotation-{}", std::process::id()));
        let path = folder.join("readings.jsonl");
        let mut log = JsonlLog::open(&path).unwrap();
        log.limit = 1; // Force rotations with real, complete JSON records.
        for amps in 1..=5 {
            log.sample(
                "Test GPU",
                crate::model::Reading {
                    pins: [crate::model::Pin {
                        volts: 12.0,
                        amps: amps as f32,
                    }; 6],
                },
            )
            .unwrap();
        }
        let current: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(current["connector_watts"], 360.0);
        assert!(!path.with_extension("4.jsonl").exists());
        for number in 1..=3 {
            let archived: serde_json::Value = serde_json::from_str(
                std::fs::read_to_string(path.with_extension(format!("{number}.jsonl")))
                    .unwrap()
                    .trim(),
            )
            .unwrap();
            assert_eq!(archived["pins"][0]["amps"], (5 - number) as f64);
        }
        drop(log);
        for file in [
            path.clone(),
            path.with_extension("1.jsonl"),
            path.with_extension("2.jsonl"),
            path.with_extension("3.jsonl"),
        ] {
            std::fs::remove_file(file).unwrap();
        }
        std::fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn discards_only_an_incomplete_tail_and_keeps_existing_csv() {
        let folder = std::env::temp_dir().join(format!("astral-jsonl-tail-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("readings.jsonl");
        let csv = folder.join("readings.csv");
        std::fs::write(&csv, "old,csv\n1,2\n").unwrap();
        std::fs::write(&path, "{\"type\":\"previous\"}\n{\"type\":\"broken").unwrap();
        let mut log = JsonlLog::open(&path).unwrap();
        log.write_record(serde_json::json!({"type": "new"}))
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["type"], "previous");
        assert_eq!(records[1]["type"], "new");
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), "old,csv\n1,2\n");
        drop(log);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(csv).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn completes_a_valid_record_that_lacks_a_newline() {
        let folder =
            std::env::temp_dir().join(format!("astral-jsonl-newline-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("readings.jsonl");
        std::fs::write(&path, "{\"type\":\"previous\"}").unwrap();
        let mut log = JsonlLog::open(&path).unwrap();
        log.write_record(serde_json::json!({"type": "new"}))
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.starts_with("{\"type\":\"previous\"}\n"));
        drop(log);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn repairs_a_large_tail_in_bounded_chunks() {
        let folder =
            std::env::temp_dir().join(format!("astral-jsonl-large-tail-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("readings.jsonl");
        let mut content = b"{\"type\":\"previous\"}\n".to_vec();
        content.extend(vec![b'x'; 100_000]);
        std::fs::write(&path, content).unwrap();
        let log = JsonlLog::open(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"type\":\"previous\"}\n"
        );
        drop(log);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn failed_write_reopens_and_repairs_before_retrying() {
        let folder =
            std::env::temp_dir().join(format!("astral-jsonl-retry-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("readings.jsonl");
        std::fs::write(&path, "{\"type\":\"previous\"}\n{\"partial\":").unwrap();
        let mut log = JsonlLog {
            file: Some(File::open(&path).unwrap()),
            path: path.clone(),
            limit: MAX_LOG_BYTES,
        };
        assert!(
            log.write_record(serde_json::json!({"type": "new"}))
                .is_err()
        );
        assert!(log.file.is_none());
        log.write_record(serde_json::json!({"type": "new"}))
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let records: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["type"], "new");
        drop(log);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(folder).unwrap();
    }
}
