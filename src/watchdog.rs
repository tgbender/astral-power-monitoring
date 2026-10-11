//! Monitors freshness independently of the sensor thread. No GPU/Windows actions.
use crate::audio::{Audio, Tone};
use crate::history::History;
use crate::model::STALE_AFTER;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const STARTUP_GRACE: Duration = Duration::from_secs(10);
const RECOVERY_STABLE: Duration = Duration::from_secs(1);
const CHECK_INTERVAL: Duration = Duration::from_millis(500);
#[derive(Clone, Copy, Default)]
struct Snapshot {
    last_success: Option<Instant>,
    failed: bool,
    samples: u64,
}
#[derive(Default)]
pub struct Heartbeat {
    state: Mutex<Snapshot>,
}
impl Heartbeat {
    pub fn record(&self, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.last_success = Some(now);
        state.failed = false;
        state.samples = state.samples.saturating_add(1);
    }
    pub fn failed(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).failed = true;
    }
    fn snapshot(&self) -> Option<Snapshot> {
        match self.state.try_lock() {
            Ok(state) => Some(*state),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(*p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Event {
    Lost,
    Recovered,
}
struct Detector {
    started: Instant,
    lost: bool,
    recovering: Option<(Instant, u64)>,
}
impl Detector {
    fn new(started: Instant) -> Self {
        Self {
            started,
            lost: false,
            recovering: None,
        }
    }
    fn tick(&mut self, now: Instant, snapshot: Snapshot) -> Option<Event> {
        let stale = snapshot
            .last_success
            .is_none_or(|last| now.saturating_duration_since(last) > STALE_AFTER);
        if !self.lost {
            let grace = snapshot.last_success.is_none()
                && now.saturating_duration_since(self.started) < STARTUP_GRACE;
            if stale && !grace {
                self.lost = true;
                self.recovering = None;
                return Some(Event::Lost);
            }
        } else if !stale && !snapshot.failed {
            let (start, samples) = *self.recovering.get_or_insert((now, snapshot.samples));
            if now.saturating_duration_since(start) >= RECOVERY_STABLE && snapshot.samples > samples
            {
                self.lost = false;
                self.recovering = None;
                return Some(Event::Recovered);
            }
        } else {
            self.recovering = None;
        }
        None
    }
}
fn run(
    heartbeat: Arc<Heartbeat>,
    stopped: mpsc::Receiver<()>,
    mut emit: impl FnMut(Event, Snapshot),
) {
    let mut detector = Detector::new(Instant::now());
    let mut snapshot = Snapshot::default();
    loop {
        // Check stop first so normal app exit cannot produce a false outage alert.
        match stopped.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(current) = heartbeat.snapshot() {
            snapshot = current;
        }
        if let Some(event) = detector.tick(Instant::now(), snapshot) {
            emit(event, snapshot);
        }
        match stopped.recv_timeout(CHECK_INTERVAL) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
}
pub struct Watchdog {
    stop: mpsc::Sender<()>,
    finished: mpsc::Receiver<()>,
}
impl Watchdog {
    pub fn start(heartbeat: Arc<Heartbeat>, audio: Audio, history: History) -> Self {
        let (stop, stopped) = mpsc::channel();
        let (finished_tx, finished) = mpsc::channel();
        std::thread::spawn(move || {
            run(heartbeat, stopped, |event, snapshot| {
                let (action, tone) = match event {
                    Event::Lost => ("lost", Tone::MonitoringLost),
                    Event::Recovered => ("recovered", Tone::Recovered),
                };
                audio.cue(tone);
                history.event(serde_json::json!({"type":"monitoring","action":action,"tone":tone.name(),"samples":snapshot.samples,
                    "last_sample_age_ms":snapshot.last_success.map(|last|last.elapsed().as_millis() as u64),
                    "detail":if event==Event::Lost{"No fresh valid pin readings; current cannot be checked"}else{"Fresh readings restored and stable"}}));
            });
            let _ = finished_tx.send(());
        });
        Self { stop, finished }
    }
    pub fn stop(&self) {
        let _ = self.stop.send(());
        let _ = self.finished.recv_timeout(Duration::from_millis(750));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_grace_and_once_per_outage() {
        let t = Instant::now();
        let mut d = Detector::new(t);
        let s = Snapshot::default();
        assert_eq!(d.tick(t + Duration::from_secs(9), s), None);
        assert_eq!(d.tick(t + Duration::from_secs(10), s), Some(Event::Lost));
        for sec in 11..30 {
            assert_eq!(d.tick(t + Duration::from_secs(sec), s), None);
        }
    }
    #[test]
    fn stalled_sensor_thread_trips_at_freshness_limit_without_error_callback() {
        let t = Instant::now();
        let mut d = Detector::new(t);
        let s = Snapshot {
            last_success: Some(t),
            samples: 1,
            failed: false,
        };
        assert_eq!(d.tick(t + STALE_AFTER, s), None);
        assert_eq!(
            d.tick(t + STALE_AFTER + Duration::from_millis(1), s),
            Some(Event::Lost)
        );
        assert_eq!(d.tick(t + Duration::from_secs(20), s), None);
    }
    #[test]
    fn isolated_good_sample_and_repeated_failures_do_not_announce_recovery() {
        let t = Instant::now();
        let mut d = Detector::new(t);
        assert_eq!(
            d.tick(t + Duration::from_secs(10), Snapshot::default()),
            Some(Event::Lost)
        );
        let s = Snapshot {
            last_success: Some(t + Duration::from_secs(11)),
            samples: 1,
            failed: false,
        };
        assert_eq!(d.tick(t + Duration::from_secs(11), s), None);
        assert_eq!(d.tick(t + Duration::from_secs(12), s), None);
        assert_eq!(
            d.tick(t + Duration::from_secs(13), Snapshot { failed: true, ..s }),
            None
        );
        assert_eq!(
            d.tick(
                t + Duration::from_secs(14),
                Snapshot {
                    last_success: Some(t + Duration::from_secs(14)),
                    samples: 2,
                    failed: false
                }
            ),
            None
        );
        assert_eq!(
            d.tick(
                t + Duration::from_millis(14500),
                Snapshot {
                    last_success: Some(t + Duration::from_millis(14500)),
                    samples: 3,
                    failed: false
                }
            ),
            None
        );
        assert_eq!(
            d.tick(
                t + Duration::from_secs(15),
                Snapshot {
                    last_success: Some(t + Duration::from_secs(15)),
                    samples: 4,
                    failed: false
                }
            ),
            Some(Event::Recovered)
        );
        assert_eq!(
            d.tick(
                t + Duration::from_secs(19),
                Snapshot {
                    last_success: Some(t + Duration::from_secs(15)),
                    samples: 4,
                    failed: false
                }
            ),
            Some(Event::Lost)
        );
    }
    #[test]
    fn independent_thread_alerts_when_sampler_makes_no_progress() {
        let heartbeat = Arc::new(Heartbeat::default());
        heartbeat.record(Instant::now() - Duration::from_secs(4));
        let (stop, stopped) = mpsc::channel();
        let (events, received) = mpsc::channel();
        let task = std::thread::spawn(move || {
            run(heartbeat, stopped, |event, _| {
                events.send(event).unwrap();
            })
        });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Lost
        );
        stop.send(()).unwrap();
        task.join().unwrap();
        assert!(received.try_recv().is_err());
    }
    #[test]
    fn normal_exit_does_not_emit_an_outage() {
        let heartbeat = Arc::new(Heartbeat::default());
        heartbeat.record(Instant::now() - Duration::from_secs(10));
        let (stop, stopped) = mpsc::channel();
        stop.send(()).unwrap();
        run(heartbeat, stopped, |_, _| {
            panic!("normal exit must remain quiet")
        });
    }
    #[test]
    fn continuously_fresh_readings_remain_quiet_after_startup_grace() {
        let t = Instant::now();
        let mut detector = Detector::new(t);
        for sample in 1..=240 {
            let now = t + Duration::from_millis(sample * 500);
            assert_eq!(
                detector.tick(
                    now,
                    Snapshot {
                        last_success: Some(now),
                        samples: sample,
                        failed: false
                    }
                ),
                None
            );
        }
    }
}
