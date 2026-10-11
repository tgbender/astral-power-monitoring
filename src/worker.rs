use crate::actions::{Gate, RealActions};
use crate::audio::{Audio, Detector, Tone};
use crate::history::History;
use crate::protection::Protection;
use crate::watchdog::{Heartbeat, Watchdog};
use crate::{model::State, monitor::AstralPowerMonitor};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
pub type Shared = Arc<Mutex<State>>;
pub struct Worker {
    pub shared: Shared,
    pub controls: Controls,
    stop: mpsc::Sender<()>,
    finished: mpsc::Receiver<()>,
    watchdog: Watchdog,
}
#[derive(Clone)]
pub struct Controls {
    pub gate: Arc<Gate>,
    pub audio: Audio,
    pub history: History,
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
        self.watchdog.stop();
        let _ = self.controls.gate.disarm_and_cancel();
        self.controls.audio.quit();
        let _ = self.stop.send(());
        // Never join a potentially stalled GPU driver thread indefinitely.
        let _ = self.finished.recv_timeout(Duration::from_millis(750));
        let _ = self.controls.history.finish(Duration::from_secs(1));
    }
}
pub fn log_path() -> PathBuf {
    crate::history::path()
}
fn log_record(log: &History, _output: &Shared, record: serde_json::Value) {
    log.event(record);
}
impl Worker {
    pub fn start(monitor_only: bool) -> Self {
        let shared = Arc::new(Mutex::new(State::new(log_path())));
        shared.lock().unwrap().monitor_only = monitor_only;
        let output = shared.clone();
        let (stop, stopped) = mpsc::channel::<()>();
        let gate = Arc::new(Gate::default());
        let (sender, commands) = mpsc::channel::<Control>();
        let (finished_tx, finished) = mpsc::channel();
        let controls = Controls {
            gate: gate.clone(),
            audio: Audio::start(gate.armed.clone()),
            history: History::start(log_path()),
            sender,
        };
        let audio = controls.audio.clone();
        let log = controls.history.clone();
        let heartbeat = Arc::new(Heartbeat::default());
        let watchdog = Watchdog::start(heartbeat.clone(), audio.clone(), log.clone());
        std::thread::spawn(move || {
            log_record(
                &log,
                &output,
                serde_json::json!({"type": "startup", "app_version": env!("CARGO_PKG_VERSION"),"protection_armed":false,"monitor_only":monitor_only,"history":"sqlite","sample_interval_ms":500,"batch_seconds":60,"retention_hours":24}),
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
                    log_record(&log, &output, event);
                }
                let result = (|| {
                    if reader.is_none() {
                        reader = Some(AstralPowerMonitor::new()?);
                        let monitor = reader.as_ref().unwrap();
                        // Read-only initialization: connect to precisely the sensor's
                        // subsystem; ambiguous multiple-card matches are rejected.
                        actions = Some(
                            RealActions::new(monitor.subsystem, folder.clone(), gate.clone())
                                .with_history(log.clone()),
                        );
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
                        heartbeat.record(now);
                        if !connected {
                            log_record(
                                &log,
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
                        log.sample(name, reading);
                        // Advisory warnings remain active without arming protection.
                        if let Some(tone) = detector.tick(now, reading.max_amps()) {
                            audio.cue(tone);
                            log_record(
                                &log,
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
                                    &log,
                                    &output,
                                    serde_json::json!({"type":"protection","action":event.action,"detail":event.detail,"max_pin_amps":reading.max_amps()}),
                                );
                            }
                        }
                        Duration::from_millis(500)
                    }
                    Err(error) => {
                        heartbeat.failed();
                        protection.reset_observations();
                        detector.unavailable();
                        if last_sensor_error.as_deref() != Some(&error) {
                            log_record(
                                &log,
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
            log.event(serde_json::json!({"type":"session_end"}));
            let _ = finished_tx.send(());
        });
        Self {
            shared,
            controls,
            stop,
            finished,
            watchdog,
        }
    }
}
