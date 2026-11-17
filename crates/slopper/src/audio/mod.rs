//! Host sound output for the guest's sound card: a ring of guest-rate frames
//! that a player drains in real time, so the guest's DMA follows the host's
//! audio clock instead of its own instruction count.

mod coreaudio;
mod wav;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How much audio the guest may queue ahead of the speaker.
const LATENCY_MS: u32 = 60;
const RING_FRAMES: usize = 1 << 14;

/// Single-producer, single-consumer queue of stereo frames at `rate` Hz.
pub struct Ring {
    buf: Box<[AtomicU32]>,
    head: AtomicUsize,
    tail: AtomicUsize,
    rate: AtomicU32,
}

impl Ring {
    fn new() -> Self {
        Ring {
            buf: (0..RING_FRAMES).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            rate: AtomicU32::new(48000),
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

    fn pop(&self) -> Option<[f32; 2]> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        let v = self.buf[tail % RING_FRAMES].load(Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some([v as i16 as f32 / 32768.0, (v >> 16) as i16 as f32 / 32768.0])
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
    CoreAudio { _output: coreaudio::Output },
    /// Drains the ring by the wall clock, for when nothing should be heard.
    Clock(Arc<AtomicBool>),
    Idle,
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Player::Clock(stop) = self {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

pub struct HostAudio {
    ring: Arc<Ring>,
    player: Player,
    play: bool,
    wav: Option<wav::Writer>,
}

impl HostAudio {
    /// Output for the guest's sound card. With `play` false nothing is heard,
    /// but playback still runs at real-time speed. `wav` records what the
    /// guest plays.
    pub fn new(play: bool, wav: Option<&Path>) -> std::io::Result<Self> {
        let wav = wav.map(wav::Writer::create).transpose()?;
        Ok(HostAudio { ring: Arc::new(Ring::new()), player: Player::Idle, play, wav })
    }

    fn start(&mut self) {
        if self.play {
            match coreaudio::Output::start(self.ring.clone()) {
                Ok(o) => {
                    eprintln!("audio: playing on the default output device at {} Hz", o.device_rate());
                    self.player = Player::CoreAudio { _output: o };
                    return;
                }
                Err(e) => eprintln!("audio: cannot open the default output device ({e}); playing silently"),
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (ring, done) = (self.ring.clone(), stop.clone());
        std::thread::spawn(move || drain_by_clock(&ring, &done));
        self.player = Player::Clock(stop);
    }
}

fn drain_by_clock(ring: &Ring, stop: &AtomicBool) {
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
            ring.pop();
            drained += 1;
        }
    }
}

impl ac97::AudioOut for HostAudio {
    fn room(&mut self, rate: u32) -> usize {
        (rate * LATENCY_MS / 1000) as usize - self.ring.len().min((rate * LATENCY_MS / 1000) as usize)
    }

    fn push(&mut self, rate: u32, frames: &[[i16; 2]]) {
        if let Player::Idle = self.player {
            self.start();
        }
        self.ring.rate.store(rate, Ordering::Relaxed);
        self.ring.push(frames);
        if let Some(w) = self.wav.as_mut()
            && let Err(e) = w.write(rate, frames)
        {
            eprintln!("audio: cannot write the recording: {e}");
            self.wav = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_returns_frames_in_order_and_then_runs_dry() {
        let r = Ring::new();
        r.push(&[[16384, -16384], [0, 32767]]);
        assert_eq!(r.pop(), Some([0.5, -0.5]));
        assert_eq!(r.pop(), Some([0.0, 32767.0 / 32768.0]));
        assert_eq!(r.pop(), None);
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
