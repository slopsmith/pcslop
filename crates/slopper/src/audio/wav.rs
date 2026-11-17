//! Records 16-bit stereo PCM to a WAV file. The file keeps the rate of the
//! first frames it gets; later frames at another rate are resampled to it.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use super::Resampler;

const HEADER_LEN: u64 = 44;

pub struct Writer {
    file: BufWriter<File>,
    rate: u32,
    frames: u64,
    unsynced: u64,
    resampler: Resampler,
}

impl Writer {
    pub fn create(path: &Path) -> std::io::Result<Writer> {
        let file = BufWriter::new(File::create(path)?);
        Ok(Writer { file, rate: 0, frames: 0, unsynced: 0, resampler: Resampler::default() })
    }

    pub fn write(&mut self, rate: u32, frames: &[[i16; 2]]) -> std::io::Result<()> {
        if self.rate == 0 {
            self.rate = rate;
            self.file.write_all(&header(rate, 0))?;
        }
        let mut bytes = Vec::with_capacity(frames.len() * 4);
        if rate == self.rate {
            bytes.extend(frames.iter().flat_map(|f| [f[0].to_le_bytes(), f[1].to_le_bytes()].concat()));
        } else {
            let step = rate as f64 / self.rate as f64;
            for f in frames {
                self.resampler.feed(step, f.map(|s| s as f32), |o| {
                    bytes.extend(o.iter().flat_map(|&s| (s as i16).to_le_bytes()));
                });
            }
        }
        self.file.write_all(&bytes)?;
        let n = bytes.len() as u64 / 4;
        self.frames += n;
        self.unsynced += n;
        if self.unsynced >= self.rate as u64 / 2 {
            self.sync()?;
        }
        Ok(())
    }

    /// Rewrites the header for the frames written so far, so the file stays
    /// playable if the process is killed.
    fn sync(&mut self) -> std::io::Result<()> {
        self.unsynced = 0;
        let f = &mut self.file;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header(self.rate, self.frames))?;
        f.seek(SeekFrom::Start(HEADER_LEN + 4 * self.frames))?;
        f.flush()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.rate != 0 {
            let _ = self.sync();
        }
    }
}

fn header(rate: u32, frames: u64) -> [u8; HEADER_LEN as usize] {
    let data = (frames * 4).min(u32::MAX as u64 - 36) as u32;
    let mut h = [0u8; HEADER_LEN as usize];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&2u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_is_a_wav_file_at_the_first_rate() {
        let dir = std::env::temp_dir().join(format!("slopper-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.wav");
        {
            let mut w = Writer::create(&path).unwrap();
            w.write(44100, &[[1, -1], [2, -2]]).unwrap();
            w.write(22050, &[[3, 3]; 10]).unwrap();
        }
        let b = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 44100);
        let data = u32::from_le_bytes(b[40..44].try_into().unwrap()) as usize;
        assert_eq!(data, b.len() - 44);
        assert_eq!(data / 4, 2 + 20, "22050 Hz frames count twice at 44100 Hz");
        assert_eq!(&b[44..48], &[1, 0, 0xff, 0xff]);
    }
}
