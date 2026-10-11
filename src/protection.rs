//! Python's default tier ladder. Time and hardware actions are injectable in tests.
use std::time::{Duration, Instant};

pub trait Actions {
    fn limit(&mut self) -> Result<String, String>;
    fn restore(&mut self) -> Result<(), String>;
    fn restore_manual(&mut self) -> Result<(), String> {
        self.restore()
    }
    fn shutdown(&mut self, reason: &str) -> Result<(), String>;
}

#[derive(Debug)]
pub struct Event {
    pub action: &'static str,
    pub detail: String,
}
#[derive(Default)]
pub struct Protection {
    over_since: Option<Instant>,
    clear_start: Option<Instant>,
    critical_since: Option<Instant>,
    catastrophic_samples: u32,
    last_sample: Option<Instant>,
    engaged: Option<(Instant, u8)>,
    grace_deadline: Option<Instant>,
    clean_since: Option<Instant>,
    released_at: Option<Instant>,
    pub shutdown_pending: bool,
}
impl Protection {
    pub fn limited(&self) -> bool {
        self.engaged.is_some()
    }
    pub fn reset_observations(&mut self) {
        self.over_since = None;
        self.clear_start = None;
        self.critical_since = None;
        self.catastrophic_samples = 0;
        self.clean_since = None;
        self.last_sample = None;
        self.released_at = None;
        // Keep an existing limit and its grace deadline across missing samples.
    }
    pub fn disarm(&mut self) {
        self.reset_observations();
        self.shutdown_pending = false;
    }
    pub fn manual_restore(&mut self, actions: &mut impl Actions) -> Result<(), String> {
        actions.restore_manual()?;
        *self = Self::default();
        Ok(())
    }
    fn request_shutdown(
        &mut self,
        actions: &mut impl Actions,
        reason: String,
        events: &mut Vec<Event>,
    ) {
        match actions.shutdown(&reason) {
            Ok(()) => {
                self.shutdown_pending = true;
                events.push(Event {
                    action: "shutdown_requested",
                    detail: reason,
                });
            }
            Err(error) => events.push(Event {
                action: "shutdown_failed",
                detail: format!("{reason}; {error}"),
            }),
        }
    }
    fn trigger(
        &mut self,
        now: Instant,
        tier: u8,
        amps: f32,
        actions: &mut impl Actions,
        events: &mut Vec<Event>,
    ) {
        let recurrent = self
            .released_at
            .is_some_and(|t| now.duration_since(t) <= Duration::from_secs(600));
        let reason = format!("Tier {tier}: highest pin {amps:.3} A");
        if self.engaged.is_none() {
            match actions.limit() {
                Ok(detail) => {
                    self.engaged = Some((now, tier));
                    self.grace_deadline = match tier {
                        1 => Some(now + Duration::from_secs(10)),
                        2 => Some(now + Duration::from_secs(5)),
                        _ => None,
                    };
                    self.clean_since = None;
                    events.push(Event {
                        action: "gpu_limited",
                        detail: format!("{reason}; {detail}"),
                    });
                }
                Err(error) => {
                    events.push(Event {
                        action: "limit_failed",
                        detail: error,
                    });
                    self.request_shutdown(
                        actions,
                        format!("{reason}; GPU limiting unavailable/failed"),
                        events,
                    );
                    return;
                }
            }
        }
        // Like Python, engaging again never restarts the original grace deadline.
        if tier == 3 || recurrent {
            self.request_shutdown(
                actions,
                format!(
                    "{reason}; {}",
                    if recurrent {
                        "fault recurred within 600 s of release"
                    } else {
                        "16 A on two consecutive samples"
                    }
                ),
                events,
            );
        }
    }
    pub fn tick(&mut self, now: Instant, amps: f32, actions: &mut impl Actions) -> Vec<Event> {
        let mut events = Vec::new();
        if self.shutdown_pending {
            return events;
        }
        if !amps.is_finite() || !(0.0..=30.0).contains(&amps) {
            self.reset_observations();
            return events;
        }
        if self
            .last_sample
            .is_some_and(|t| now.duration_since(t) > Duration::from_secs(5))
        {
            self.reset_observations();
        }
        self.last_sample = Some(now);
        let tier1 = if amps >= 9.5 {
            self.clear_start = None;
            now.duration_since(*self.over_since.get_or_insert(now)) >= Duration::from_secs(15)
        } else {
            if amps < 9.3 {
                if now.duration_since(*self.clear_start.get_or_insert(now))
                    >= Duration::from_secs(1)
                {
                    self.over_since = None;
                }
            } else {
                self.clear_start = None;
            }
            false
        };
        self.catastrophic_samples = if amps >= 16.0 {
            self.catastrophic_samples.saturating_add(1)
        } else {
            0
        };
        if self.catastrophic_samples >= 2 {
            self.trigger(now, 3, amps, actions, &mut events);
            return events;
        }
        let tier2 = if amps >= 13.0 {
            now.duration_since(*self.critical_since.get_or_insert(now)) >= Duration::from_secs(3)
        } else {
            self.critical_since = None;
            false
        };
        if tier2 {
            self.trigger(now, 2, amps, actions, &mut events);
        }
        if self.shutdown_pending {
            return events;
        }
        if tier1 {
            self.trigger(now, 1, amps, actions, &mut events);
        }
        if self.shutdown_pending {
            return events;
        }
        if let Some((engaged_at, tier)) = self.engaged {
            if self.grace_deadline.is_some_and(|deadline| now >= deadline) {
                if amps >= 9.5 {
                    self.request_shutdown(
                        actions,
                        format!(
                            "Tier {tier}: highest pin {amps:.3} A remains >= 9.5 A after limiting"
                        ),
                        &mut events,
                    );
                    return events;
                }
                self.grace_deadline = None;
                events.push(Event {
                    action: "grace_cleared",
                    detail: "Current fell below 9.5 A; emergency limits held".into(),
                });
            }
            if amps < 9.3 {
                self.clean_since.get_or_insert(now);
            } else {
                self.clean_since = None;
            }
            if self.grace_deadline.is_none()
                && now.duration_since(engaged_at) >= Duration::from_secs(120)
                && self
                    .clean_since
                    .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(60))
            {
                match actions.restore() {
                    Ok(()) => {
                        *self = Self {
                            released_at: Some(now),
                            ..Default::default()
                        };
                        events.push(Event {
                            action: "gpu_restored",
                            detail: "Minimum 120 s hold and 60 s below 9.3 A completed".into(),
                        });
                    }
                    Err(error) => events.push(Event {
                        action: "restore_failed",
                        detail: error,
                    }),
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        limits: u32,
        restores: u32,
        stops: u32,
        fail_limit: bool,
        fail_stop: bool,
        fail_restore: bool,
    }
    impl Actions for Fake {
        fn limit(&mut self) -> Result<String, String> {
            self.limits += 1;
            if self.fail_limit {
                Err("refused".into())
            } else {
                Ok("simulated".into())
            }
        }
        fn restore(&mut self) -> Result<(), String> {
            self.restores += 1;
            if self.fail_restore {
                Err("refused".into())
            } else {
                Ok(())
            }
        }
        fn shutdown(&mut self, _: &str) -> Result<(), String> {
            self.stops += 1;
            if self.fail_stop {
                Err("refused".into())
            } else {
                Ok(())
            }
        }
    }
    fn ticks(p: &mut Protection, a: &mut Fake, start: Instant, from: u64, through: u64, amps: f32) {
        for half in from..=through {
            p.tick(start + Duration::from_millis(half * 500), amps, a);
        }
    }
    #[test]
    fn tier1_15_seconds_and_ten_second_grace() {
        let (mut p, mut a, t) = (Protection::default(), Fake::default(), Instant::now());
        ticks(&mut p, &mut a, t, 0, 29, 9.5);
        assert_eq!(a.limits, 0);
        ticks(&mut p, &mut a, t, 30, 49, 9.5);
        assert_eq!((a.limits, a.stops), (1, 0));
        ticks(&mut p, &mut a, t, 50, 60, 9.5);
        assert_eq!((a.limits, a.stops), (1, 1));
    }
    #[test]
    fn tier2_three_seconds_and_five_second_grace() {
        let (mut p, mut a, t) = (Protection::default(), Fake::default(), Instant::now());
        ticks(&mut p, &mut a, t, 0, 5, 13.0);
        assert_eq!(a.limits, 0);
        ticks(&mut p, &mut a, t, 6, 15, 13.0);
        assert_eq!((a.limits, a.stops), (1, 0));
        p.tick(t + Duration::from_secs(8), 13.0, &mut a);
        assert_eq!(a.stops, 1);
    }
    #[test]
    fn tier3_requires_two_consecutive_valid_samples() {
        let (mut p, mut a, t) = (Protection::default(), Fake::default(), Instant::now());
        p.tick(t, 16.0, &mut a);
        assert_eq!(a.stops, 0);
        p.tick(t + Duration::from_millis(500), 15.9, &mut a);
        p.tick(t + Duration::from_secs(1), 16.0, &mut a);
        assert_eq!(a.stops, 0);
        p.tick(t + Duration::from_millis(1500), 16.0, &mut a);
        assert_eq!((a.limits, a.stops), (1, 1));
    }
    #[test]
    fn failed_shutdown_keeps_sampling_and_retries() {
        let (mut p, mut a, t) = (
            Protection::default(),
            Fake {
                fail_limit: true,
                fail_stop: true,
                ..Default::default()
            },
            Instant::now(),
        );
        ticks(&mut p, &mut a, t, 0, 7, 13.0);
        assert_eq!(a.stops, 2);
        assert!(!p.shutdown_pending);
        a.fail_stop = false;
        p.tick(t + Duration::from_secs(4), 13.0, &mut a);
        assert!(p.shutdown_pending);
        ticks(&mut p, &mut a, t, 9, 12, 13.0);
        assert_eq!(a.stops, 3);
    }
    #[test]
    fn disconnect_and_long_gap_do_not_release_limits_or_count_as_clean() {
        for disconnect in [true, false] {
            let (mut p, mut a, t) = (Protection::default(), Fake::default(), Instant::now());
            ticks(&mut p, &mut a, t, 0, 6, 13.0);
            ticks(&mut p, &mut a, t, 7, 130, 1.0);
            if disconnect {
                p.reset_observations();
            }
            p.tick(t + Duration::from_secs(200), 1.0, &mut a);
            assert_eq!(a.restores, 0);
            assert!(p.limited());
            ticks(&mut p, &mut a, t, 401, 519, 1.0);
            assert_eq!(a.restores, 0);
            p.tick(t + Duration::from_secs(260), 1.0, &mut a);
            assert_eq!(a.restores, 1);
        }
    }
    #[test]
    fn release_hold_hysteresis_and_retrigger_match_python() {
        let (mut p, mut a, t) = (Protection::default(), Fake::default(), Instant::now());
        ticks(&mut p, &mut a, t, 0, 30, 9.5);
        ticks(&mut p, &mut a, t, 31, 268, 9.29);
        assert_eq!(a.restores, 0);
        p.tick(t + Duration::from_secs(135), 9.29, &mut a);
        assert_eq!(a.restores, 1);
        ticks(&mut p, &mut a, t, 271, 277, 13.0);
        assert_eq!((a.limits, a.stops), (2, 1));
    }
    #[test]
    fn failed_restore_keeps_limited_state_for_retry() {
        let (mut p, mut a, t) = (
            Protection::default(),
            Fake {
                fail_restore: true,
                ..Default::default()
            },
            Instant::now(),
        );
        ticks(&mut p, &mut a, t, 0, 6, 13.0);
        ticks(&mut p, &mut a, t, 7, 246, 1.0);
        assert!(p.limited());
        assert_eq!(a.restores, 1);
        a.fail_restore = false;
        p.tick(t + Duration::from_millis(123500), 1.0, &mut a);
        assert!(!p.limited());
    }
    #[test]
    fn invalid_samples_and_disarm_reset_catastrophic_confirmation() {
        let (mut p, mut a, t) = (Protection::default(), Fake::default(), Instant::now());
        p.tick(t, 16.0, &mut a);
        p.tick(t + Duration::from_millis(500), f32::NAN, &mut a);
        p.tick(t + Duration::from_secs(1), 16.0, &mut a);
        assert_eq!(a.stops, 0);
        p.disarm();
        p.tick(t + Duration::from_millis(1500), 16.0, &mut a);
        assert_eq!(a.stops, 0);
    }
    #[test]
    fn tier1_hysteresis_and_one_second_clear() {
        let (mut p, mut a, t) = (Protection::default(), Fake::default(), Instant::now());
        ticks(&mut p, &mut a, t, 0, 26, 9.5);
        p.tick(t + Duration::from_secs(14), 9.4, &mut a);
        p.tick(t + Duration::from_secs(15), 9.5, &mut a);
        assert_eq!(a.limits, 1);
        let (mut p, mut a) = (Protection::default(), Fake::default());
        ticks(&mut p, &mut a, t, 0, 25, 9.5);
        ticks(&mut p, &mut a, t, 26, 28, 9.29);
        ticks(&mut p, &mut a, t, 29, 30, 9.5);
        assert_eq!(a.limits, 0);
    }
}
