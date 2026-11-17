//! Host sound output for the guest's sound card: a ring of guest-rate frames
//! that a player drains in real time, so the guest's DMA follows the host's
//! audio clock instead of its own instruction count. A recording taps the
//! player, so it has a gap wherever the speaker would.

mod coreaudio;
mod wav;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How much audio to keep queued ahead of the speaker by default. It has
/// to cover the player's buffer and the sound card thread's wakeups.
pub const LATENCY_MS: u32 = 60;
const RING_FRAMES: usize = 1 << 14;

/// Single-producer, single-consumer queue of stereo frames at `rate` Hz.
pub struct Ring {
    buf: Box<[AtomicU32]>,
    head: AtomicUsize,
    tail: AtomicUsize,
    rate: AtomicU32,
    /// Frames the player wanted while the ring was empty, after it first had some.
    starved: AtomicU64,
    /// How many times the ring ran dry.
    dry_spells: AtomicU64,
    was_dry: AtomicBool,
}

impl Ring {
    fn new() -> Self {
        Ring {
            buf: (0..RING_FRAMES).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            rate: AtomicU32::new(48000),
            starved: AtomicU64::new(0),
            dry_spells: AtomicU64::new(0),
            was_dry: AtomicBool::new(true),
        }
    }

    fn len(&self) -> usize {
        self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))
    }

    fn push(&self, frames: &[[i16; 2]]) {
        let head = self.head.load(Ordering::Relaxed);
        let n = frames.len().min(RING_FRAMES - self.len());
        for (i, f) in frames[..n].iter().enumerate() {
            let v = f[0] as u16 as u32 | (f[1] as u16 as u32) << 16;
            self.buf[head.wrapping_add(i) % RING_FRAMES].store(v, Ordering::Relaxed);
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
    }

    /// The next frame, or silence if the ring is empty.
    fn pop(&self) -> [i16; 2] {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            if !self.was_dry.swap(true, Ordering::Relaxed) {
                self.dry_spells.fetch_add(1, Ordering::Relaxed);
            }
            if self.dry_spells.load(Ordering::Relaxed) > 0 {
                self.starved.fetch_add(1, Ordering::Relaxed);
            }
            return [0; 2];
        }
        let v = self.buf[tail % RING_FRAMES].load(Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        self.was_dry.store(false, Ordering::Relaxed);
        [v as i16, (v >> 16) as i16]
    }

    pub fn rate(&self) -> u32 {
        self.rate.load(Ordering::Relaxed)
    }
}

/// Linear interpolation from one sample rate to another, fed one input frame at a time.
#[derive(Default)]
pub struct Resampler {
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    /// Feeds one input frame; `step` is input frames per output frame.
    /// Calls `out` for each output frame that falls before this input frame.
    pub fn feed(&mut self, step: f64, frame: [f32; 2], mut out: impl FnMut([f32; 2])) {
        while self.pos < 1.0 {
            let t = self.pos as f32;
            out([0, 1].map(|c| self.prev[c] + (frame[c] - self.prev[c]) * t));
            self.pos += step;
        }
        self.pos -= 1.0;
        self.prev = frame;
    }
}

enum Player {
    CoreAudio { _output: coreaudio::Output, _recorder: Arc<AtomicBool> },
    /// Drains the ring by the wall clock, for when nothing should be heard.
    Clock(Arc<AtomicBool>),
    Idle,
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Player::Clock(stop) | Player::CoreAudio { _recorder: stop, .. } = self {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

pub struct HostAudio {
    ring: Arc<Ring>,
    player: Player,
    play: bool,
    wav: Option<wav::Writer>,
    latency_ms: u32,
}


impl HostAudio {
    /// Output for the guest's sound card, `latency_ms` ahead of the speaker.
    /// With `play` false nothing is heard, but playback still runs at
    /// real-time speed. `wav` records what the speaker gets.
    pub fn new(play: bool, wav: Option<&Path>, latency_ms: u32) -> std::io::Result<Self> {
        let wav = wav.map(wav::Writer::create).transpose()?;
        Ok(HostAudio { ring: Arc::new(Ring::new()), player: Player::Idle, play, wav, latency_ms })
    }

    fn start(&mut self) {
        let stop = Arc::new(AtomicBool::new(false));
        let tap = self.wav.take().map(|w| {
            let tap = Arc::new(Ring::new());
            let (t, done) = (tap.clone(), stop.clone());
            std::thread::spawn(move || record(&t, w, &done));
            tap
        });
        if self.play {
            match coreaudio::Output::start(self.ring.clone(), tap.clone()) {
                Ok(o) => {
                    eprintln!("audio: playing on the default output device at {} Hz", o.device_rate());
                    self.player = Player::CoreAudio { _output: o, _recorder: stop };
                    return;
                }
                Err(e) => eprintln!("audio: cannot open the default output device ({e}); playing silently"),
            }
        }
        let (ring, done) = (self.ring.clone(), stop.clone());
        std::thread::spawn(move || drain_by_clock(&ring, tap.as_deref(), &done));
        self.player = Player::Clock(stop);
    }

    /// The ring's fill level and how often the player found it empty.
    pub fn describe(&self) -> String {
        let r = &self.ring;
        format!(
            "host ring {} frames at {} Hz, ran dry {} times, {} frames of silence",
            r.len(),
            r.rate(),
            r.dry_spells.load(Ordering::Relaxed),
            r.starved.load(Ordering::Relaxed)
        )
    }
}

fn drain_by_clock(ring: &Ring, tap: Option<&Ring>, stop: &AtomicBool) {
    let start = Instant::now();
    let mut drained = 0u64;
    let mut rate = ring.rate();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(5));
        if ring.rate() != rate {
            rate = ring.rate();
            drained = start.elapsed().as_nanos() as u64 * rate as u64 / 1_000_000_000;
        }
        let due = start.elapsed().as_nanos() as u64 * rate as u64 / 1_000_000_000;
        while drained < due {
            let f = ring.pop();
            if let Some(t) = tap {
                t.rate.store(rate, Ordering::Relaxed);
                t.push(&[f]);
            }
            drained += 1;
        }
    }
}

/// Writes what the player played to `wav` until `stop`.
fn record(tap: &Ring, mut wav: wav::Writer, stop: &AtomicBool) {
    let mut frames = Vec::new();
    loop {
        let done = stop.load(Ordering::Relaxed);
        frames.clear();
        while tap.len() > 0 {
            frames.push(tap.pop());
        }
        if !frames.is_empty()
            && let Err(e) = wav.write(tap.rate(), &frames)
        {
            eprintln!("audio: cannot write the recording: {e}");
            return;
        }
        if done {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl ac97::AudioOut for HostAudio {
    fn queued(&self) -> usize {
        self.ring.len()
    }

    fn latency(&self, rate: u32) -> usize {
        ((rate as u64 * self.latency_ms as u64 / 1000) as usize).clamp(1, RING_FRAMES / 2)
    }

    fn push(&mut self, rate: u32, frames: &[[i16; 2]]) {
        if let Player::Idle = self.player {
            self.start();
        }
        self.ring.rate.store(rate, Ordering::Relaxed);
        self.ring.push(frames);
    }

    fn describe(&self) -> String {
        HostAudio::describe(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_returns_frames_in_order_and_then_runs_dry() {
        let r = Ring::new();
        r.push(&[[16384, -16384], [0, 32767]]);
        assert_eq!(r.pop(), [16384, -16384]);
        assert_eq!(r.pop(), [0, 32767]);
        assert_eq!(r.pop(), [0, 0]);
    }

    #[test]
    fn ring_counts_the_frames_it_was_empty_for_after_it_first_had_some() {
        let r = Ring::new();
        r.pop();
        assert_eq!(r.starved.load(Ordering::Relaxed), 0, "empty before playback starts");
        r.push(&[[1, 1]]);
        r.pop();
        r.pop();
        r.pop();
        r.push(&[[1, 1]]);
        r.pop();
        r.pop();
        assert_eq!(r.starved.load(Ordering::Relaxed), 3);
        assert_eq!(r.dry_spells.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn ring_drops_frames_that_do_not_fit() {
        let r = Ring::new();
        r.push(&vec![[1, 1]; RING_FRAMES + 10]);
        assert_eq!(r.len(), RING_FRAMES);
    }

    #[test]
    fn resampler_doubles_the_frames_when_upsampling_by_two() {
        let mut r = Resampler::default();
        let mut out = Vec::new();
        for x in [1.0, 1.0, 1.0] {
            r.feed(0.5, [x, -x], |f| out.push(f));
        }
        assert_eq!(out.len(), 6);
        assert_eq!(out[3], [1.0, -1.0]);
    }

    #[test]
    fn resampler_keeps_the_frame_count_ratio_for_44100_to_48000() {
        let mut r = Resampler::default();
        let mut n = 0;
        for _ in 0..44100 {
            r.feed(44100.0 / 48000.0, [0.0; 2], |_| n += 1);
        }
        assert!((47999..=48001).contains(&n), "{n}");
    }
}
