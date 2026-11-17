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

#[derive(Default)]
struct Played {
    room: usize,
    frames: Vec<[i16; 2]>,
    rate: u32,
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Played>>);

impl AudioOut for Sink {
    fn room(&mut self, _rate: u32) -> usize {
        self.0.lock().unwrap().room
    }

    fn push(&mut self, rate: u32, frames: &[[i16; 2]]) {
        let mut s = self.0.lock().unwrap();
        s.room -= frames.len();
        s.frames.extend_from_slice(frames);
        s.rate = rate;
    }
}

impl Sink {
    fn allow(&self, frames: usize) {
        self.0.lock().unwrap().room += frames;
    }

    fn take(&self) -> Vec<[i16; 2]> {
        std::mem::take(&mut self.0.lock().unwrap().frames)
    }
}

const BDL: u32 = 0x1000;
const PO_SR: u16 = PCM_OUT + bm::SR;

/// A controller with PCM out unmuted at 0 dB and buffers of `frames` stereo
/// frames each, the n-th filled with the sample value n + 1.
fn setup(buffers: &[(u32, u32)]) -> (Ac97, Ram, Sink) {
    let sink = Sink::default();
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
    (a, ram, sink)
}

fn start(a: &mut Ac97, ram: &mut Ram, lvi: u8, cr: u8) {
    a.nabm_write(PCM_OUT + bm::LVI, 1, lvi as u32, ram);
    a.nabm_write(PCM_OUT + bm::CR, 1, (cr | cr::RPBM) as u32, ram);
}

fn civ(a: &mut Ac97) -> u8 {
    a.nabm_read(PCM_OUT + bm::CIV, 1) as u8
}

fn status(a: &mut Ac97) -> u16 {
    a.nabm_read(PO_SR, 2) as u16
}

#[test]
fn channels_come_out_of_reset_halted() {
    let (mut a, _, _) = setup(&[]);
    for ch in [PCM_IN, PCM_OUT, MIC_IN] {
        assert_eq!(a.nabm_read(ch + bm::SR, 2), sr::DCH as u32);
        assert_eq!(a.nabm_read(ch + bm::CIV, 1), 0);
    }
    assert!(!a.irq());
}

#[test]
fn starting_loads_the_first_descriptor_into_picb() {
    let (mut a, mut ram, _) = setup(&[(100, 0)]);
    start(&mut a, &mut ram, 0, 0);
    assert_eq!(civ(&mut a), 0);
    assert_eq!(a.nabm_read(PCM_OUT + bm::PIV, 1), 1);
    assert_eq!(a.nabm_read(PCM_OUT + bm::PICB, 2), 200, "PICB counts samples, not frames");
    assert_eq!(status(&mut a) & sr::DCH, 0);
}

#[test]
fn playback_fetches_only_as_much_as_the_output_takes() {
    let (mut a, mut ram, sink) = setup(&[(100, 0)]);
    start(&mut a, &mut ram, 0, 0);
    sink.allow(30);
    a.run(0, &mut ram);
    assert_eq!(sink.take(), vec![[1, 1]; 30]);
    assert_eq!(a.nabm_read(PCM_OUT + bm::PICB, 2), 140);
    a.run(0, &mut ram);
    assert!(sink.take().is_empty());
}

#[test]
fn civ_walks_the_list_until_the_last_valid_index() {
    let (mut a, mut ram, sink) = setup(&[(10, 0), (10, 0), (10, 0)]);
    start(&mut a, &mut ram, 1, 0);
    sink.allow(15);
    a.run(0, &mut ram);
    assert_eq!(civ(&mut a), 1);
    sink.allow(100);
    a.run(0, &mut ram);
    let played = sink.take();
    assert_eq!(played.len(), 20, "buffer 2 lies past LVI");
    assert_eq!(played[19], [2, 2]);
    assert_eq!(civ(&mut a), 1);
    assert_eq!(status(&mut a) & (sr::DCH | sr::CELV | sr::LVBCI), sr::DCH | sr::CELV | sr::LVBCI);
}

#[test]
fn raising_lvi_after_the_end_resumes_with_the_next_buffer() {
    let (mut a, mut ram, sink) = setup(&[(10, 0), (10, 0)]);
    start(&mut a, &mut ram, 0, 0);
    sink.allow(100);
    a.run(0, &mut ram);
    assert_ne!(status(&mut a) & sr::DCH, 0);
    a.nabm_write(PCM_OUT + bm::LVI, 1, 1, &mut ram);
    assert_eq!(civ(&mut a), 1);
    assert_eq!(status(&mut a) & (sr::DCH | sr::CELV), 0);
    a.run(0, &mut ram);
    assert_eq!(sink.take()[10..], [[2, 2]; 10]);
}

#[test]
fn the_list_index_wraps_after_32_entries() {
    let mut list = vec![(1, 0); 32];
    list[31].1 = BD_IOC;
    let (mut a, mut ram, sink) = setup(&list);
    start(&mut a, &mut ram, 31, 0);
    sink.allow(31);
    a.run(0, &mut ram);
    assert_eq!(civ(&mut a), 31);
    a.nabm_write(PCM_OUT + bm::LVI, 1, 33, &mut ram);
    assert_eq!(a.nabm_read(PCM_OUT + bm::LVI, 1), 1);
    sink.allow(1);
    a.run(0, &mut ram);
    assert_eq!(civ(&mut a), 0);
    assert_eq!(a.nabm_read(PCM_OUT + bm::PIV, 1), 1);
}

#[test]
fn completing_a_buffer_with_ioc_sets_bcis_and_interrupts_when_enabled() {
    let (mut a, mut ram, sink) = setup(&[(10, BD_IOC), (10, 0)]);
    start(&mut a, &mut ram, 1, cr::IOCE);
    sink.allow(10);
    a.run(0, &mut ram);
    assert_ne!(status(&mut a) & sr::BCIS, 0);
    assert!(a.irq());
    assert_ne!(a.nabm_read(GLOB_STA, 4) & 1 << 6, 0, "PCM out interrupt in GLOB_STA");
}

#[test]
fn completion_without_ioc_or_without_ioce_does_not_interrupt() {
    let (mut a, mut ram, sink) = setup(&[(10, 0), (10, BD_IOC), (10, 0)]);
    start(&mut a, &mut ram, 2, 0);
    sink.allow(10);
    a.run(0, &mut ram);
    assert_eq!(status(&mut a) & sr::BCIS, 0);
    sink.allow(10);
    a.run(0, &mut ram);
    assert_ne!(status(&mut a) & sr::BCIS, 0);
    assert!(!a.irq(), "IOCE is off");
}

#[test]
fn writing_one_to_a_status_bit_clears_it_and_drops_the_interrupt() {
    let (mut a, mut ram, sink) = setup(&[(10, BD_IOC)]);
    start(&mut a, &mut ram, 0, cr::IOCE | cr::LVBIE);
    sink.allow(10);
    a.run(0, &mut ram);
    assert!(a.irq());
    a.nabm_write(PO_SR, 2, sr::BCIS as u32, &mut ram);
    assert!(a.irq(), "LVBCI is still set");
    a.nabm_write(PO_SR, 2, (sr::LVBCI | sr::DCH | sr::CELV) as u32, &mut ram);
    assert!(!a.irq());
    assert_eq!(a.nabm_read(GLOB_STA, 4) & 0xe0, 0);
    assert_eq!(status(&mut a), sr::DCH | sr::CELV, "DCH and CELV are read-only");
}

#[test]
fn reset_bit_stops_the_channel_but_keeps_interrupt_enables() {
    let (mut a, mut ram, sink) = setup(&[(10, BD_IOC), (10, 0)]);
    start(&mut a, &mut ram, 1, cr::IOCE);
    sink.allow(10);
    a.run(0, &mut ram);
    a.nabm_write(PCM_OUT + bm::CR, 1, cr::RR as u32, &mut ram);
    assert_eq!(status(&mut a), sr::DCH);
    assert_eq!(a.nabm_read(PCM_OUT + bm::CR, 1), cr::IOCE as u32);
    assert_eq!(a.nabm_read(PCM_OUT + bm::BDBAR, 4), 0);
    assert_eq!(civ(&mut a), 0);
    assert!(!a.irq());
}

#[test]
fn volume_scales_the_samples_sent_to_the_output() {
    let (mut a, mut ram, sink) = setup(&[(4, 0)]);
    a.nam_write(codec::reg::MASTER_VOLUME as u16, 2, 0x0404);
    for e in (0x2_0000..0x2_0010).step_by(2) {
        ram.0[e..e + 2].copy_from_slice(&1000i16.to_le_bytes());
    }
    start(&mut a, &mut ram, 0, 0);
    sink.allow(4);
    a.run(0, &mut ram);
    assert_eq!(sink.take()[0], [501, 501]);
}

#[test]
fn playback_uses_the_codec_sample_rate() {
    let (mut a, mut ram, sink) = setup(&[(4, 0)]);
    a.nam_write(codec::reg::FRONT_DAC_RATE as u16, 2, 22050);
    start(&mut a, &mut ram, 0, 0);
    sink.allow(4);
    a.run(0, &mut ram);
    assert_eq!(sink.0.lock().unwrap().rate, 22050);
}

#[test]
fn a_sample_split_across_buffers_is_joined() {
    let (mut a, mut ram, sink) = setup(&[(2, 0), (1, 0)]);
    for (n, picb) in [(0, 3u32), (1, 1)] {
        let e = BDL as usize + n * 8 + 4;
        ram.0[e..e + 4].copy_from_slice(&picb.to_le_bytes());
    }
    start(&mut a, &mut ram, 1, 0);
    sink.allow(2);
    a.run(0, &mut ram);
    assert_eq!(sink.take(), vec![[1, 1], [1, 2]]);
}

#[test]
fn pcm_in_records_silence_at_its_sample_rate() {
    let (mut a, mut ram, _) = setup(&[]);
    let buf = 0x8_0000u32;
    ram.0[BDL as usize..BDL as usize + 8].copy_from_slice(&[buf.to_le_bytes(), (BD_IOC | 9600).to_le_bytes()].concat());
    ram.0[buf as usize..buf as usize + 19200].fill(0xaa);
    a.nabm_write(PCM_IN + bm::BDBAR, 4, BDL, &mut ram);
    a.nabm_write(PCM_IN + bm::CR, 1, (cr::RPBM | cr::IOCE) as u32, &mut ram);
    a.run(1_000_000_000, &mut ram);
    a.run(1_050_000_000, &mut ram);
    assert_eq!(a.nabm_read(PCM_IN + bm::PICB, 2), 9600 - 4800, "50 ms of 48 kHz stereo");
    assert!(ram.0[buf as usize..buf as usize + 9600].iter().all(|&b| b == 0));
    a.run(1_100_000_000, &mut ram);
    assert!(a.irq());
    assert_ne!(a.nabm_read(GLOB_STA, 4) & 1 << 5, 0);
}

#[test]
fn glob_sta_reports_the_primary_codec_ready() {
    let (mut a, _, _) = setup(&[]);
    assert_ne!(a.nabm_read(GLOB_STA, 4) & 1 << 8, 0);
}

#[test]
fn the_codec_access_semaphore_is_taken_by_reading_it() {
    let (mut a, _, _) = setup(&[]);
    assert_eq!(a.nabm_read(CAS, 1), 0);
    assert_eq!(a.nabm_read(CAS, 1), 1);
    a.nam_read(0x7c, 2);
    assert_eq!(a.nabm_read(CAS, 1), 0);
}

#[test]
fn wide_reads_combine_neighbouring_registers() {
    let (mut a, mut ram, _) = setup(&[(100, 0)]);
    start(&mut a, &mut ram, 3, cr::IOCE);
    assert_eq!(a.nabm_read(PCM_OUT + bm::CIV, 4), 3 << 8);
    assert_eq!(a.nabm_read(PCM_OUT + bm::PICB, 4), 200 | 1 << 16 | ((cr::IOCE | cr::RPBM) as u32) << 24);
}
