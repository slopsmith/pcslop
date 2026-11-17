use std::sync::{Arc, Mutex};

use super::*;

struct Ram(Vec<u8>);

impl DmaMemory for Ram {
    fn read_phys(&mut self, addr: u32, buf: &mut [u8]) {
        buf.copy_from_slice(&self.0[addr as usize..addr as usize + buf.len()]);
    }

    fn write_phys(&mut self, addr: u32, buf: &[u8]) {
        self.0[addr as usize..addr as usize + buf.len()].copy_from_slice(buf);
    }
}

const LATENCY: usize = 1000;

/// A host output queue that the test drains by hand.
#[derive(Default)]
struct Played {
    frames: Vec<[i16; 2]>,
    rate: u32,
    queued: usize,
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Played>>);

impl AudioOut for Sink {
    fn queued(&self) -> usize {
        self.0.lock().unwrap().queued
    }

    fn latency(&self, _rate: u32) -> usize {
        LATENCY
    }

    fn push(&mut self, rate: u32, frames: &[[i16; 2]]) {
        let mut s = self.0.lock().unwrap();
        s.queued += frames.len();
        s.frames.extend_from_slice(frames);
        s.rate = rate;
    }
}

impl Sink {
    fn drain(&self, frames: usize) {
        let mut s = self.0.lock().unwrap();
        s.queued = s.queued.saturating_sub(frames);
    }

    fn take(&self) -> Vec<[i16; 2]> {
        std::mem::take(&mut self.0.lock().unwrap().frames)
    }
}

const BDL: u32 = 0x1000;
const PO_SR: u16 = PCM_OUT + bm::SR;

/// A controller, its RAM, its output and the host clock.
struct Rig {
    a: Ac97,
    ram: Ram,
    sink: Sink,
    now: u64,
}

impl Rig {
    /// A controller with PCM out unmuted at 0 dB and buffers of `frames`
    /// stereo frames each, the n-th filled with the sample value n + 1.
    fn new(buffers: &[(u32, u32)]) -> Rig {
        let sink = Sink::default();
        sink.0.lock().unwrap().queued = LATENCY;
        let mut a = Ac97::new(Box::new(sink.clone()));
        let mut ram = Ram(vec![0; 0x20_0000]);
        for (n, &(frames, ctl)) in buffers.iter().enumerate() {
            let addr = 0x2_0000 + n as u32 * 0x8000;
            let e = BDL as usize + n * 8;
            ram.0[e..e + 4].copy_from_slice(&addr.to_le_bytes());
            ram.0[e + 4..e + 8].copy_from_slice(&(ctl | (frames * 2)).to_le_bytes());
            for i in 0..frames * 2 {
                let at = (addr + 2 * i) as usize;
                ram.0[at..at + 2].copy_from_slice(&(n as i16 + 1).to_le_bytes());
            }
        }
        a.nam_write(codec::reg::MASTER_VOLUME as u16, 2, 0);
        a.nam_write(codec::reg::PCM_OUT_VOLUME as u16, 2, 0x0808);
        a.nabm_write(PCM_OUT + bm::BDBAR, 4, BDL, &mut ram);
        Rig { a, ram, sink, now: 1_000_000_000 }
    }

    fn nabm_write(&mut self, off: u16, size: u8, v: u32) {
        self.a.nabm_write(off, size, v, &mut self.ram);
    }

    fn run(&mut self) {
        self.a.run(self.now, &mut self.ram);
    }

    /// Starts PCM out with the output queue already at its latency.
    fn start(&mut self, lvi: u8, cr: u8) {
        self.nabm_write(PCM_OUT + bm::LVI, 1, lvi as u32);
        self.nabm_write(PCM_OUT + bm::CR, 1, (cr | cr::RPBM) as u32);
        self.run();
    }

    /// Plays `frames` from the output queue and runs the controller after
    /// that much host time.
    fn play(&mut self, frames: u64) {
        let rate = self.a.codec.dac_rate() as u64;
        self.sink.drain(frames as usize);
        self.now += (frames * 1_000_000_000).div_ceil(rate);
        self.run();
    }

    fn civ(&mut self) -> u8 {
        self.a.nabm_read(PCM_OUT + bm::CIV, 1) as u8
    }

    fn picb(&mut self) -> u32 {
        self.a.nabm_read(PCM_OUT + bm::PICB, 2)
    }

    fn status(&mut self) -> u16 {
        self.a.nabm_read(PO_SR, 2) as u16
    }
}

#[test]
fn channels_come_out_of_reset_halted() {
    let mut r = Rig::new(&[]);
    for ch in [PCM_IN, PCM_OUT, MIC_IN] {
        assert_eq!(r.a.nabm_read(ch + bm::SR, 2), sr::DCH as u32);
        assert_eq!(r.a.nabm_read(ch + bm::CIV, 1), 0);
    }
    assert!(!r.a.irq());
}

#[test]
fn starting_loads_the_first_descriptor_into_picb() {
    let mut r = Rig::new(&[(100, 0)]);
    r.start(0, 0);
    assert_eq!(r.civ(), 0);
    assert_eq!(r.a.nabm_read(PCM_OUT + bm::PIV, 1), 1);
    assert_eq!(r.picb(), 200, "PICB counts samples, not frames");
    assert_eq!(r.status() & sr::DCH, 0);
}

#[test]
fn playback_starts_by_filling_the_output_queue_from_the_guest() {
    let mut r = Rig::new(&[(8000, 0)]);
    r.sink.drain(LATENCY);
    r.start(0, 0);
    assert_eq!(r.sink.take(), vec![[1, 1]; LATENCY]);
    assert_eq!(r.picb(), 16000 - 2 * LATENCY as u32);
}

#[test]
fn playback_fetches_frames_at_the_sample_rate_in_host_time() {
    let mut r = Rig::new(&[(100, 0)]);
    r.start(0, 0);
    r.play(30);
    assert_eq!(r.sink.take(), vec![[1, 1]; 30]);
    assert_eq!(r.picb(), 140);
    r.run();
    assert!(r.sink.take().is_empty(), "no host time has passed");
}

#[test]
fn the_play_position_moves_smoothly_with_host_time() {
    let mut r = Rig::new(&[(8000, 0)]);
    r.start(0, 0);
    let mut picb = r.picb();
    for _ in 0..100 {
        r.sink.drain(48);
        r.now += 1_000_000;
        r.run();
        let p = r.picb();
        assert!((94..=98).contains(&(picb - p)), "a millisecond is about 48 frames, moved {} samples", picb - p);
        picb = p;
    }
}

#[test]
fn playback_holds_while_the_output_queue_is_full() {
    let mut r = Rig::new(&[(8000, 0)]);
    r.start(0, 0);
    r.now += 1_000_000_000;
    r.run();
    assert_eq!(r.sink.take().len(), LATENCY, "the queue takes up to twice the latency");
    r.now += 1_000_000_000;
    r.run();
    assert!(r.sink.take().is_empty());
}

#[test]
fn a_queue_that_ran_low_is_refilled_from_the_guest_at_once() {
    let mut r = Rig::new(&[(8000, 0)]);
    r.start(0, 0);
    r.sink.drain(LATENCY);
    r.now += 1_000_000;
    r.run();
    assert_eq!(r.sink.take(), vec![[1, 1]; LATENCY]);
}

/// Plays 60 s of a looping buffer list in 1 ms steps on a speaker whose
/// clock runs `drift` fast, and returns the lowest and the final queue level.
fn play_with_drift(drift: f64) -> (usize, usize) {
    let mut r = Rig::new(&[(8000, 0); 32]);
    r.start(31, 0);
    let mut owed = 0.0;
    let mut lowest = usize::MAX;
    for _ in 0..60_000 {
        owed += 48.0 * (1.0 + drift);
        r.sink.drain(owed as usize);
        owed -= owed.trunc();
        lowest = lowest.min(r.sink.queued());
        r.now += 1_000_000;
        r.run();
        let civ = r.civ();
        r.nabm_write(PCM_OUT + bm::LVI, 1, (civ as u32 + 31) % 32);
        r.sink.take();
    }
    (lowest, r.sink.queued())
}

#[test]
fn the_output_queue_stays_near_its_latency_when_the_speaker_clock_drifts() {
    for drift in [-0.002, 0.002] {
        let (lowest, last) = play_with_drift(drift);
        assert!(lowest > LATENCY / 2, "drift {drift}: ran down to {lowest}");
        assert!((LATENCY * 7 / 10..=LATENCY * 13 / 10).contains(&last), "drift {drift}: ended at {last}");
    }
}

#[test]
fn civ_walks_the_list_until_the_last_valid_index() {
    let mut r = Rig::new(&[(10, 0), (10, 0), (10, 0)]);
    r.start(1, 0);
    r.play(15);
    assert_eq!(r.civ(), 1);
    r.play(100);
    let played = r.sink.take();
    assert_eq!(played.len(), 20, "buffer 2 lies past LVI");
    assert_eq!(played[19], [2, 2]);
    assert_eq!(r.civ(), 1);
    assert_eq!(r.status() & (sr::DCH | sr::CELV | sr::LVBCI), sr::DCH | sr::CELV | sr::LVBCI);
}

#[test]
fn a_guest_underrun_sends_nothing_more_and_never_waits() {
    let mut r = Rig::new(&[(10, 0)]);
    r.start(0, 0);
    r.play(100);
    assert_eq!(r.sink.take().len(), 10);
    r.play(100);
    assert!(r.sink.take().is_empty());
    assert_ne!(r.status() & sr::DCH, 0);
}

#[test]
fn raising_lvi_after_the_end_resumes_with_the_next_buffer() {
    let mut r = Rig::new(&[(10, 0), (10, 0)]);
    r.start(0, 0);
    r.play(100);
    assert_ne!(r.status() & sr::DCH, 0);
    r.nabm_write(PCM_OUT + bm::LVI, 1, 1);
    assert_eq!(r.civ(), 1);
    assert_eq!(r.status() & (sr::DCH | sr::CELV), 0);
    r.run();
    r.play(10);
    let played = r.sink.take();
    assert_eq!(played[played.len() - 10..], [[2, 2]; 10]);
}

#[test]
fn resuming_after_an_underrun_refills_the_queue_from_the_guest_alone() {
    let mut r = Rig::new(&[(10, 0), (400, 0)]);
    r.start(0, 0);
    r.play(500);
    r.sink.take();
    r.now += 1_000_000;
    r.nabm_write(PCM_OUT + bm::LVI, 1, 1);
    r.run();
    assert_eq!(r.sink.take(), vec![[2, 2]; 400]);
}

#[test]
fn the_list_index_wraps_after_32_entries() {
    let mut list = vec![(1, 0); 32];
    list[31].1 = BD_IOC;
    let mut r = Rig::new(&list);
    r.start(31, 0);
    r.play(31);
    assert_eq!(r.civ(), 31);
    r.nabm_write(PCM_OUT + bm::LVI, 1, 33);
    assert_eq!(r.a.nabm_read(PCM_OUT + bm::LVI, 1), 1);
    r.play(1);
    assert_eq!(r.civ(), 0);
    assert_eq!(r.a.nabm_read(PCM_OUT + bm::PIV, 1), 1);
}

#[test]
fn completing_a_buffer_with_ioc_sets_bcis_and_interrupts_when_enabled() {
    let mut r = Rig::new(&[(10, BD_IOC), (10, 0)]);
    r.start(1, cr::IOCE);
    r.play(9);
    assert!(!r.a.irq(), "the buffer is not done yet");
    r.play(1);
    assert_ne!(r.status() & sr::BCIS, 0);
    assert!(r.a.irq());
    assert_ne!(r.a.nabm_read(GLOB_STA, 4) & 1 << 6, 0, "PCM out interrupt in GLOB_STA");
}

#[test]
fn completion_without_ioc_or_without_ioce_does_not_interrupt() {
    let mut r = Rig::new(&[(10, 0), (10, BD_IOC), (10, 0)]);
    r.start(2, 0);
    r.play(10);
    assert_eq!(r.status() & sr::BCIS, 0);
    r.play(10);
    assert_ne!(r.status() & sr::BCIS, 0);
    assert!(!r.a.irq(), "IOCE is off");
}

#[test]
fn writing_one_to_a_status_bit_clears_it_and_drops_the_interrupt() {
    let mut r = Rig::new(&[(10, BD_IOC)]);
    r.start(0, cr::IOCE | cr::LVBIE);
    r.play(10);
    assert!(r.a.irq());
    r.nabm_write(PO_SR, 2, sr::BCIS as u32);
    assert!(r.a.irq(), "LVBCI is still set");
    r.nabm_write(PO_SR, 2, (sr::LVBCI | sr::DCH | sr::CELV) as u32);
    assert!(!r.a.irq());
    assert_eq!(r.a.nabm_read(GLOB_STA, 4) & 0xe0, 0);
    assert_eq!(r.status(), sr::DCH | sr::CELV, "DCH and CELV are read-only");
}

#[test]
fn reset_bit_stops_the_channel_but_keeps_interrupt_enables() {
    let mut r = Rig::new(&[(10, BD_IOC), (10, 0)]);
    r.start(1, cr::IOCE);
    r.play(10);
    r.nabm_write(PCM_OUT + bm::CR, 1, cr::RR as u32);
    assert_eq!(r.status(), sr::DCH);
    assert_eq!(r.a.nabm_read(PCM_OUT + bm::CR, 1), cr::IOCE as u32);
    assert_eq!(r.a.nabm_read(PCM_OUT + bm::BDBAR, 4), 0);
    assert_eq!(r.civ(), 0);
    assert!(!r.a.irq());
}

#[test]
fn volume_scales_the_samples_sent_to_the_output() {
    let mut r = Rig::new(&[(4, 0)]);
    r.a.nam_write(codec::reg::MASTER_VOLUME as u16, 2, 0x0404);
    for e in (0x2_0000..0x2_0010).step_by(2) {
        r.ram.0[e..e + 2].copy_from_slice(&1000i16.to_le_bytes());
    }
    r.start(0, 0);
    r.play(4);
    assert_eq!(r.sink.take()[0], [501, 501]);
}

#[test]
fn playback_uses_the_codec_sample_rate() {
    let mut r = Rig::new(&[(400, 0)]);
    r.a.nam_write(codec::reg::FRONT_DAC_RATE as u16, 2, 22050);
    r.start(0, 0);
    r.sink.drain(22);
    r.now += 1_000_000;
    r.run();
    assert_eq!(r.sink.take().len(), 22, "a millisecond at 22050 Hz");
    assert_eq!(r.sink.0.lock().unwrap().rate, 22050);
}

#[test]
fn a_sample_split_across_buffers_is_joined() {
    let mut r = Rig::new(&[(2, 0), (1, 0)]);
    for (n, picb) in [(0, 3u32), (1, 1)] {
        let e = BDL as usize + n * 8 + 4;
        r.ram.0[e..e + 4].copy_from_slice(&picb.to_le_bytes());
    }
    r.start(1, 0);
    r.play(2);
    assert_eq!(r.sink.take(), vec![[1, 1], [1, 2]]);
}

#[test]
fn pcm_in_records_silence_at_its_sample_rate() {
    let mut r = Rig::new(&[]);
    let buf = 0x8_0000u32;
    r.ram.0[BDL as usize..BDL as usize + 8].copy_from_slice(&[buf.to_le_bytes(), (BD_IOC | 9600).to_le_bytes()].concat());
    r.ram.0[buf as usize..buf as usize + 19200].fill(0xaa);
    r.nabm_write(PCM_IN + bm::BDBAR, 4, BDL);
    r.nabm_write(PCM_IN + bm::CR, 1, (cr::RPBM | cr::IOCE) as u32);
    r.run();
    r.now += 50_000_000;
    r.run();
    assert_eq!(r.a.nabm_read(PCM_IN + bm::PICB, 2), 9600 - 4800, "50 ms of 48 kHz stereo");
    assert!(r.ram.0[buf as usize..buf as usize + 9600].iter().all(|&b| b == 0));
    r.now += 50_000_000;
    r.run();
    assert!(r.a.irq());
    assert_ne!(r.a.nabm_read(GLOB_STA, 4) & 1 << 5, 0);
}

#[test]
fn mic_in_records_mono_silence_at_its_sample_rate() {
    let mut r = Rig::new(&[]);
    let buf = 0x8_0000u32;
    r.ram.0[BDL as usize..BDL as usize + 8].copy_from_slice(&[buf.to_le_bytes(), 4800u32.to_le_bytes()].concat());
    r.ram.0[buf as usize..buf as usize + 9600].fill(0xaa);
    r.nabm_write(MIC_IN + bm::BDBAR, 4, BDL);
    r.nabm_write(MIC_IN + bm::CR, 1, cr::RPBM as u32);
    r.run();
    r.now += 50_000_000;
    r.run();
    assert_eq!(r.a.nabm_read(MIC_IN + bm::PICB, 2), 4800 - 2400, "50 ms of 48 kHz mono");
    assert!(r.ram.0[buf as usize..buf as usize + 4800].iter().all(|&b| b == 0));
    assert_eq!(r.ram.0[buf as usize + 4800], 0xaa);
}

#[test]
fn glob_sta_reports_the_primary_codec_ready() {
    let mut r = Rig::new(&[]);
    assert_ne!(r.a.nabm_read(GLOB_STA, 4) & 1 << 8, 0);
}

#[test]
fn the_codec_access_semaphore_is_taken_by_reading_it() {
    let mut r = Rig::new(&[]);
    assert_eq!(r.a.nabm_read(CAS, 1), 0);
    assert_eq!(r.a.nabm_read(CAS, 1), 1);
    r.a.nam_read(0x7c, 2);
    assert_eq!(r.a.nabm_read(CAS, 1), 0);
}

#[test]
fn wide_reads_combine_neighbouring_registers() {
    let mut r = Rig::new(&[(100, 0)]);
    r.start(3, cr::IOCE);
    assert_eq!(r.a.nabm_read(PCM_OUT + bm::CIV, 4), 3 << 8);
    assert_eq!(r.a.nabm_read(PCM_OUT + bm::PICB, 4), 200 | 1 << 16 | ((cr::IOCE | cr::RPBM) as u32) << 24);
}
