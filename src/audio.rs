//! Audio is advisory: this module cannot control the GPU or shut Windows down.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use windows_sys::Win32::Media::Audio::{
    PlaySoundW, SND_ASYNC, SND_LOOP, SND_MEMORY, SND_NODEFAULT,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tone {
    Warning,
    Limited,
    Critical,
    Shutdown,
}
impl Tone {
    fn duration(self) -> Duration {
        Duration::from_millis(match self {
            Self::Warning => 440,
            Self::Limited => 540,
            Self::Critical => 660,
            Self::Shutdown => 1000,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Limited => "gpu_limited",
            Self::Critical => "critical",
            Self::Shutdown => "shutdown_countdown",
        }
    }
}

#[derive(Default)]
pub struct Detector {
    over_since: Option<Instant>,
    below_since: Option<Instant>,
    last_sample: Option<Instant>,
    warned: bool,
    critical: bool,
}
impl Detector {
    pub fn unavailable(&mut self) {
        self.over_since = None;
        self.below_since = None;
        self.last_sample = None;
        // An outage cannot re-arm a warning: require fresh, genuinely clear readings.
    }
    pub fn tick(&mut self, now: Instant, amps: f32) -> Option<Tone> {
        if !amps.is_finite() || !(0.0..=30.0).contains(&amps) {
            self.unavailable();
            return None;
        }
        if self
            .last_sample
            .is_some_and(|t| now.duration_since(t) > Duration::from_secs(3))
        {
            self.unavailable();
        }
        self.last_sample = Some(now);
        if amps < 9.3 {
            self.over_since = None;
            if now.duration_since(*self.below_since.get_or_insert(now)) >= Duration::from_secs(1) {
                self.warned = false;
                self.critical = false;
            }
        } else {
            self.below_since = None;
        }
        if amps >= 13.0 && !self.critical {
            self.critical = true;
            self.warned = true;
            return Some(Tone::Critical);
        }
        if amps >= 9.5 {
            if now.duration_since(*self.over_since.get_or_insert(now)) >= Duration::from_secs(2)
                && !self.warned
            {
                self.warned = true;
                return Some(Tone::Warning);
            }
        } else {
            self.over_since = None;
        }
        None
    }
}

trait Output {
    fn play(&mut self, tone: Tone, looping: bool) -> Result<(), String>;
    fn stop(&mut self);
}
struct NativeOutput;
// SND_MEMORY borrows its buffer during asynchronous playback. Static, aligned
// buffers stay alive for the entire process; no filenames or DLL search paths.
#[repr(align(4))]
struct Wav<const N: usize>([u8; N]);
static WARNING: Wav<{ include_bytes!("../assets/sounds/warning.wav").len() }> =
    Wav(*include_bytes!("../assets/sounds/warning.wav"));
static LIMITED: Wav<{ include_bytes!("../assets/sounds/limited.wav").len() }> =
    Wav(*include_bytes!("../assets/sounds/limited.wav"));
static CRITICAL: Wav<{ include_bytes!("../assets/sounds/critical.wav").len() }> =
    Wav(*include_bytes!("../assets/sounds/critical.wav"));
static SHUTDOWN: Wav<{ include_bytes!("../assets/sounds/shutdown.wav").len() }> =
    Wav(*include_bytes!("../assets/sounds/shutdown.wav"));
impl Output for NativeOutput {
    fn play(&mut self, tone: Tone, looping: bool) -> Result<(), String> {
        let bytes: &'static [u8] = match tone {
            Tone::Warning => &WARNING.0,
            Tone::Limited => &LIMITED.0,
            Tone::Critical => &CRITICAL.0,
            Tone::Shutdown => &SHUTDOWN.0,
        };
        let flags = SND_MEMORY | SND_ASYNC | SND_NODEFAULT | if looping { SND_LOOP } else { 0 };
        if unsafe { PlaySoundW(bytes.as_ptr().cast(), std::ptr::null_mut(), flags) } == 0 {
            Err("Windows could not play alert audio; check the output device".into())
        } else {
            Ok(())
        }
    }
    fn stop(&mut self) {
        unsafe {
            PlaySoundW(std::ptr::null(), std::ptr::null_mut(), 0);
        }
    }
}

enum Command {
    Cue(Tone),
    CancelCountdown,
    Test,
    MuteChanged,
    Quit,
}
#[derive(Default)]
struct Player {
    active: Option<(Tone, Instant, bool)>,
    deferred: Option<Tone>,
    preview: VecDeque<Tone>,
}
impl Player {
    fn play(
        &mut self,
        now: Instant,
        tone: Tone,
        looping: bool,
        output: &mut impl Output,
    ) -> Result<(), String> {
        self.active = None;
        if let Err(error) = output.play(tone, looping) {
            output.stop();
            self.deferred = None;
            self.preview.clear();
            return Err(error);
        }
        self.active = Some((
            tone,
            now + if looping {
                Duration::from_secs(5)
            } else {
                tone.duration()
            },
            looping,
        ));
        Ok(())
    }
    fn command(
        &mut self,
        now: Instant,
        command: Command,
        muted: bool,
        countdown: bool,
        output: &mut impl Output,
    ) -> Result<(), String> {
        match command {
            Command::Cue(tone) => {
                if (tone == Tone::Warning && muted) || (tone == Tone::Shutdown && !countdown) {
                    return Ok(());
                }
                self.preview.clear();
                if self.active.is_some_and(|(active, _, _)| active > tone) {
                    if self.active.is_none_or(|(t, _, _)| t != Tone::Shutdown) {
                        self.deferred = Some(self.deferred.map_or(tone, |old| old.max(tone)));
                    }
                } else {
                    self.deferred = None;
                    self.play(now, tone, tone == Tone::Shutdown, output)?;
                }
            }
            Command::CancelCountdown => {
                if self
                    .active
                    .is_some_and(|(tone, _, looping)| tone == Tone::Shutdown && looping)
                {
                    output.stop();
                    self.active = None;
                }
            }
            Command::MuteChanged => {
                if muted {
                    if self
                        .active
                        .is_some_and(|(tone, _, _)| tone == Tone::Warning)
                    {
                        output.stop();
                        self.active = None;
                    }
                    if self.deferred == Some(Tone::Warning) {
                        self.deferred = None;
                    }
                }
            }
            Command::Test => {
                // Test playback is independent of protection, never masks a real alarm.
                if self.active.is_none() && self.deferred.is_none() {
                    self.preview = VecDeque::from([
                        Tone::Warning,
                        Tone::Critical,
                        Tone::Limited,
                        Tone::Shutdown,
                    ]);
                }
            }
            Command::Quit => {
                output.stop();
                self.active = None;
                self.deferred = None;
                self.preview.clear();
            }
        }
        self.advance(now, muted, countdown, output)
    }
    fn advance(
        &mut self,
        now: Instant,
        muted: bool,
        countdown: bool,
        output: &mut impl Output,
    ) -> Result<(), String> {
        if self.active.is_some_and(|(tone, end, looping)| {
            now >= end || (tone == Tone::Shutdown && looping && !countdown)
        }) {
            output.stop();
            self.active = None;
        }
        if self.active.is_none() {
            if let Some(tone) = self.deferred.take()
                && (tone != Tone::Warning || !muted)
            {
                return self.play(now, tone, false, output);
            }
            if let Some(tone) = self.preview.pop_front() {
                self.play(now, tone, false, output)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Audio {
    sender: mpsc::SyncSender<Command>,
    pub warning_muted: Arc<AtomicBool>,
    countdown: Arc<AtomicBool>,
    pub error: Arc<Mutex<Option<String>>>,
}
impl Audio {
    pub fn start(protection_armed: Arc<AtomicBool>) -> Self {
        let (sender, receiver) = mpsc::sync_channel(16);
        let audio = Self {
            sender,
            warning_muted: Arc::new(AtomicBool::new(false)),
            countdown: Arc::new(AtomicBool::new(false)),
            error: Arc::new(Mutex::new(None)),
        };
        let muted = audio.warning_muted.clone();
        let countdown = audio.countdown.clone();
        let error = audio.error.clone();
        std::thread::spawn(move || {
            let (mut player, mut output) = (Player::default(), NativeOutput);
            loop {
                let wait = player
                    .active
                    .map(|(_, end, _)| {
                        end.saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(250))
                    })
                    .unwrap_or(Duration::from_secs(60));
                let command = match receiver.recv_timeout(wait) {
                    Ok(Command::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                };
                let now = Instant::now();
                let result = match command {
                    Some(command) => player.command(
                        now,
                        command,
                        muted.load(Ordering::SeqCst),
                        countdown.load(Ordering::SeqCst) && protection_armed.load(Ordering::SeqCst),
                        &mut output,
                    ),
                    None => player.advance(
                        now,
                        muted.load(Ordering::SeqCst),
                        countdown.load(Ordering::SeqCst) && protection_armed.load(Ordering::SeqCst),
                        &mut output,
                    ),
                };
                if let Err(e) = result {
                    *error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                }
            }
            output.stop();
        });
        audio
    }
    pub fn cue(&self, tone: Tone) {
        if tone == Tone::Shutdown {
            self.countdown.store(true, Ordering::SeqCst);
        }
        self.send(Command::Cue(tone));
    }
    pub fn cancel_countdown(&self) {
        self.countdown.store(false, Ordering::SeqCst);
        self.send(Command::CancelCountdown);
    }
    pub fn toggle_warning_mute(&self) {
        self.warning_muted.fetch_xor(true, Ordering::SeqCst);
        self.send(Command::MuteChanged);
    }
    pub fn test(&self) {
        *self.error.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.send(Command::Test);
    }
    pub fn quit(&self) {
        self.countdown.store(false, Ordering::SeqCst);
        self.send(Command::Quit);
    }
    fn send(&self, command: Command) {
        if let Err(e) = self.sender.try_send(command) {
            *self.error.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(format!("Alert audio unavailable: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        played: Vec<(Tone, bool)>,
        stopped: u32,
        fail: bool,
    }
    impl Output for Fake {
        fn play(&mut self, tone: Tone, looping: bool) -> Result<(), String> {
            self.played.push((tone, looping));
            if self.fail {
                Err("no audio".into())
            } else {
                Ok(())
            }
        }
        fn stop(&mut self) {
            self.stopped += 1;
        }
    }
    #[test]
    fn warning_requires_two_seconds_and_does_not_repeat_per_sample() {
        let (mut d, t) = (Detector::default(), Instant::now());
        for half in 0..4 {
            assert_eq!(d.tick(t + Duration::from_millis(half * 500), 9.5), None);
        }
        assert_eq!(d.tick(t + Duration::from_secs(2), 9.5), Some(Tone::Warning));
        for half in 5..20 {
            assert_eq!(d.tick(t + Duration::from_millis(half * 500), 9.5), None);
        }
    }
    #[test]
    fn critical_escalates_immediately_and_clear_readings_rearm() {
        let (mut d, t) = (Detector::default(), Instant::now());
        assert_eq!(d.tick(t, 13.0), Some(Tone::Critical));
        assert_eq!(d.tick(t + Duration::from_millis(500), 13.0), None);
        d.tick(t + Duration::from_secs(1), 9.29);
        d.tick(t + Duration::from_secs(2), 9.29);
        assert_eq!(
            d.tick(t + Duration::from_secs(3), 13.0),
            Some(Tone::Critical)
        );
    }
    #[test]
    fn gaps_and_invalid_samples_do_not_count_as_sustained_or_clear() {
        let (mut d, t) = (Detector::default(), Instant::now());
        d.tick(t, 9.5);
        assert_eq!(d.tick(t + Duration::from_secs(20), 9.5), None);
        assert_eq!(
            d.tick(t + Duration::from_secs(22), 9.5),
            Some(Tone::Warning)
        );
        d.tick(t + Duration::from_secs(23), 1.0);
        d.unavailable();
        d.tick(t + Duration::from_secs(40), 1.0);
        assert_eq!(
            d.tick(t + Duration::from_millis(40500), 13.0),
            Some(Tone::Critical)
        );
        d.tick(t + Duration::from_secs(41), f32::NAN);
        assert_eq!(d.tick(t + Duration::from_secs(42), 13.0), None);
    }
    #[test]
    fn countdown_preempts_other_audio_and_expires_or_cancels() {
        let (mut p, mut o, t) = (Player::default(), Fake::default(), Instant::now());
        p.command(t, Command::Cue(Tone::Critical), false, false, &mut o)
            .unwrap();
        p.command(t, Command::Cue(Tone::Shutdown), false, true, &mut o)
            .unwrap();
        p.command(t, Command::Cue(Tone::Warning), false, true, &mut o)
            .unwrap();
        assert_eq!(o.played, [(Tone::Critical, false), (Tone::Shutdown, true)]);
        p.advance(t + Duration::from_secs(5), false, true, &mut o)
            .unwrap();
        assert!(p.active.is_none());
        p.command(t, Command::Cue(Tone::Shutdown), false, true, &mut o)
            .unwrap();
        p.command(t, Command::CancelCountdown, false, false, &mut o)
            .unwrap();
        assert!(p.active.is_none());
    }
    #[test]
    fn mute_only_suppresses_warning_and_intervention_waits_for_critical() {
        let (mut p, mut o, t) = (Player::default(), Fake::default(), Instant::now());
        p.command(t, Command::Cue(Tone::Warning), true, false, &mut o)
            .unwrap();
        assert!(o.played.is_empty());
        p.command(t, Command::Cue(Tone::Critical), true, false, &mut o)
            .unwrap();
        p.command(t, Command::Cue(Tone::Limited), true, false, &mut o)
            .unwrap();
        assert_eq!(o.played, [(Tone::Critical, false)]);
        p.advance(t + Duration::from_millis(660), true, false, &mut o)
            .unwrap();
        assert_eq!(o.played.last(), Some(&(Tone::Limited, false)));
    }
    #[test]
    fn test_sequence_never_starts_a_countdown_and_real_alerts_interrupt_it() {
        let (mut p, mut o, t) = (Player::default(), Fake::default(), Instant::now());
        p.command(t, Command::Test, false, false, &mut o).unwrap();
        for ms in [440, 1100, 1640, 2640] {
            p.advance(t + Duration::from_millis(ms), false, false, &mut o)
                .unwrap();
        }
        assert_eq!(
            o.played,
            [
                (Tone::Warning, false),
                (Tone::Critical, false),
                (Tone::Limited, false),
                (Tone::Shutdown, false)
            ]
        );
        assert!(p.active.is_none());
        p.command(t, Command::Test, false, false, &mut o).unwrap();
        p.command(t, Command::Cue(Tone::Critical), false, false, &mut o)
            .unwrap();
        assert!(p.preview.is_empty());
    }
    #[test]
    fn playback_failure_is_contained_and_can_be_retried() {
        let (mut p, mut o, t) = (
            Player::default(),
            Fake {
                fail: true,
                ..Default::default()
            },
            Instant::now(),
        );
        assert!(
            p.command(t, Command::Cue(Tone::Critical), false, false, &mut o)
                .is_err()
        );
        assert!(p.active.is_none());
        o.fail = false;
        p.command(t, Command::Cue(Tone::Critical), false, false, &mut o)
            .unwrap();
        assert!(p.active.is_some());
    }
}
