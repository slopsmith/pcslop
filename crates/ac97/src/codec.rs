//! An AC'97 2.1 codec with variable-rate audio, reporting itself as a SigmaTel
//! STAC9700. The reset values and write masks follow QEMU's hw/audio/ac97.c
//! (InnoTek Systemberatung GmbH, from VirtualBox OSE), which XP and Linux
//! drivers are known to accept.

pub mod reg {
    pub const RESET: u8 = 0x00;
    pub const MASTER_VOLUME: u8 = 0x02;
    pub const HEADPHONE_VOLUME: u8 = 0x04;
    pub const MONO_VOLUME: u8 = 0x06;
    pub const PCM_OUT_VOLUME: u8 = 0x18;
    pub const RECORD_SELECT: u8 = 0x1a;
    pub const RECORD_GAIN: u8 = 0x1c;
    pub const POWERDOWN: u8 = 0x26;
    pub const EXT_AUDIO_ID: u8 = 0x28;
    pub const EXT_AUDIO_CTRL: u8 = 0x2a;
    pub const FRONT_DAC_RATE: u8 = 0x2c;
    pub const SURROUND_DAC_RATE: u8 = 0x2e;
    pub const LFE_DAC_RATE: u8 = 0x30;
    pub const LR_ADC_RATE: u8 = 0x32;
    pub const MIC_ADC_RATE: u8 = 0x34;
    pub const VENDOR_ID1: u8 = 0x7c;
    pub const VENDOR_ID2: u8 = 0x7e;
}

/// Variable rate audio for PCM out and PCM in (EXT_AUDIO_CTRL bit 0).
pub const VRA: u16 = 1;
/// Variable rate audio for the mic input (EXT_AUDIO_CTRL bit 3).
pub const VRM: u16 = 8;
const MUTE: u16 = 0x8000;
const FIXED_RATE: u16 = 48000;

/// Registers the codec does not implement; writes to them are dropped.
const READ_ONLY: &[u8] = &[
    reg::HEADPHONE_VOLUME,
    reg::MONO_VOLUME,
    0x08,
    0x0a,
    0x0c,
    0x0e,
    0x10,
    0x12,
    0x14,
    0x16,
    0x1e,
    0x20,
    0x22,
    reg::EXT_AUDIO_ID,
    0x6c,
    0x6e,
    reg::VENDOR_ID1,
    reg::VENDOR_ID2,
];

pub struct Codec {
    regs: [u16; 64],
}

impl Default for Codec {
    fn default() -> Self {
        let mut c = Codec { regs: [0; 64] };
        c.reset();
        c
    }
}

impl Codec {
    pub fn reset(&mut self) {
        self.regs = [0; 64];
        for (r, v) in [
            (reg::MASTER_VOLUME, MUTE),
            (reg::PCM_OUT_VOLUME, 0x8808),
            (reg::RECORD_GAIN, 0x8808),
            (reg::POWERDOWN, 0x000f),
            (reg::VENDOR_ID1, 0x8384),
            (reg::VENDOR_ID2, 0x7600),
            (reg::EXT_AUDIO_ID, 0x0809),
            (reg::EXT_AUDIO_CTRL, 0x0009),
            (reg::FRONT_DAC_RATE, FIXED_RATE),
            (reg::SURROUND_DAC_RATE, FIXED_RATE),
            (reg::LFE_DAC_RATE, FIXED_RATE),
            (reg::LR_ADC_RATE, FIXED_RATE),
            (reg::MIC_ADC_RATE, FIXED_RATE),
        ] {
            self.store(r, v);
        }
    }

    fn store(&mut self, r: u8, v: u16) {
        self.regs[(r as usize >> 1) & 63] = v;
    }

    pub fn read(&self, r: u8) -> u16 {
        self.regs[(r as usize >> 1) & 63]
    }

    pub fn write(&mut self, r: u8, v: u16) {
        let r = r & !1;
        let vra = self.read(reg::EXT_AUDIO_CTRL) & VRA != 0;
        let vrm = self.read(reg::EXT_AUDIO_CTRL) & VRM != 0;
        match r {
            reg::RESET => self.reset(),
            reg::POWERDOWN => self.store(r, (v & !0x800f) | (self.read(r) & 0xf)),
            reg::MASTER_VOLUME => self.store(r, v & 0xbf3f),
            reg::PCM_OUT_VOLUME => self.store(r, v & 0x9f1f),
            reg::RECORD_GAIN => self.store(r, v & 0x8f0f),
            reg::RECORD_SELECT => self.store(r, v & 0x0707),
            reg::EXT_AUDIO_CTRL => {
                if v & VRA == 0 {
                    self.store(reg::FRONT_DAC_RATE, FIXED_RATE);
                    self.store(reg::LR_ADC_RATE, FIXED_RATE);
                }
                if v & VRM == 0 {
                    self.store(reg::MIC_ADC_RATE, FIXED_RATE);
                }
                self.store(r, v);
            }
            reg::FRONT_DAC_RATE | reg::LR_ADC_RATE if vra => self.store(r, v),
            reg::MIC_ADC_RATE if vrm => self.store(r, v),
            reg::FRONT_DAC_RATE | reg::LR_ADC_RATE | reg::MIC_ADC_RATE => {}
            _ if READ_ONLY.contains(&r) => {}
            _ => self.store(r, v),
        }
    }

    /// PCM out sample rate in Hz; 0 means the guest set no usable rate.
    pub fn dac_rate(&self) -> u32 {
        self.read(reg::FRONT_DAC_RATE) as u32
    }

    pub fn adc_rate(&self) -> u32 {
        self.read(reg::LR_ADC_RATE) as u32
    }

    pub fn mic_rate(&self) -> u32 {
        self.read(reg::MIC_ADC_RATE) as u32
    }

    /// Linear gain for the left and right PCM out channels, from the master
    /// and PCM out volume registers. Each step is 1.5 dB, master volume
    /// attenuates from 0 dB and PCM out volume is 0 dB at 0x08.
    pub fn out_gain(&self) -> [f32; 2] {
        let master = self.read(reg::MASTER_VOLUME);
        let pcm = self.read(reg::PCM_OUT_VOLUME);
        if (master | pcm) & MUTE != 0 {
            return [0.0; 2];
        }
        [8, 0].map(|shift| {
            let db = -1.5 * ((master >> shift) & 0x3f) as f32 + 1.5 * (8 - ((pcm >> shift) & 0x1f) as i32) as f32;
            10f32.powf(db / 20.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_values_identify_a_stac9700_with_variable_rate() {
        let c = Codec::default();
        assert_eq!((c.read(reg::VENDOR_ID1), c.read(reg::VENDOR_ID2)), (0x8384, 0x7600));
        assert_eq!(c.read(reg::EXT_AUDIO_ID) & (VRA | VRM), VRA | VRM);
        assert_eq!(c.read(reg::POWERDOWN) & 0xf, 0xf, "ADC, DAC, analog and reference ready");
        assert_eq!(c.read(reg::MASTER_VOLUME), 0x8000);
        assert_eq!(c.read(reg::PCM_OUT_VOLUME), 0x8808);
        assert_eq!(c.dac_rate(), 48000);
    }

    #[test]
    fn writing_the_reset_register_restores_defaults() {
        let mut c = Codec::default();
        c.write(reg::MASTER_VOLUME, 0x0505);
        c.write(reg::FRONT_DAC_RATE, 22050);
        c.write(reg::RESET, 0);
        assert_eq!(c.read(reg::MASTER_VOLUME), 0x8000);
        assert_eq!(c.dac_rate(), 48000);
    }

    #[test]
    fn dac_rate_follows_the_guest_only_while_vra_is_on() {
        let mut c = Codec::default();
        c.write(reg::FRONT_DAC_RATE, 44100);
        assert_eq!(c.dac_rate(), 44100);
        c.write(reg::EXT_AUDIO_CTRL, 0);
        assert_eq!(c.dac_rate(), 48000);
        c.write(reg::FRONT_DAC_RATE, 22050);
        assert_eq!(c.dac_rate(), 48000);
        c.write(reg::EXT_AUDIO_CTRL, VRA);
        c.write(reg::FRONT_DAC_RATE, 22050);
        assert_eq!(c.dac_rate(), 22050);
    }

    #[test]
    fn identification_registers_ignore_writes() {
        let mut c = Codec::default();
        c.write(reg::VENDOR_ID1, 0);
        c.write(reg::EXT_AUDIO_ID, 0);
        assert_eq!(c.read(reg::VENDOR_ID1), 0x8384);
        assert_eq!(c.read(reg::EXT_AUDIO_ID), 0x0809);
    }

    #[test]
    fn master_volume_keeps_six_bits_so_drivers_detect_its_range() {
        let mut c = Codec::default();
        c.write(reg::MASTER_VOLUME, 0xffff);
        assert_eq!(c.read(reg::MASTER_VOLUME), 0xbf3f);
        c.write(reg::PCM_OUT_VOLUME, 0xffff);
        assert_eq!(c.read(reg::PCM_OUT_VOLUME), 0x9f1f);
    }

    #[test]
    fn output_is_silent_until_unmuted() {
        let mut c = Codec::default();
        assert_eq!(c.out_gain(), [0.0, 0.0]);
        c.write(reg::MASTER_VOLUME, 0);
        assert_eq!(c.out_gain(), [0.0, 0.0], "PCM out is still muted");
        c.write(reg::PCM_OUT_VOLUME, 0x0808);
        assert_eq!(c.out_gain(), [1.0, 1.0]);
    }

    #[test]
    fn volume_steps_are_one_and_a_half_decibels_per_channel() {
        let mut c = Codec::default();
        c.write(reg::PCM_OUT_VOLUME, 0x0808);
        c.write(reg::MASTER_VOLUME, 0x0400);
        let [l, r] = c.out_gain();
        assert!((20.0 * l.log10() + 6.0).abs() < 1e-3, "left {l}");
        assert_eq!(r, 1.0);
    }
}
