//! Save states: a directory with `state.bin` and an APFS clone of the hard
//! disk image. docs/architecture.md describes the format and time policy.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use pc::{Board, Machine};
use snapshot::{Reader, Snap, Writer};
use x86::cpu::Cpu;

pub const STATE_FILE: &str = "state.bin";
const DISK_FILE: &str = "hda.img";
const RUN_DISK_FILE: &str = "hda.run.img";
const MAGIC: &[u8; 8] = b"86SLSNAP";
const VERSION: u32 = 1;
const CHUNK: usize = 16 << 20;
const PAGE: usize = 4096;

/// The disk image a running machine writes to. The ISO in the CD-ROM drive
/// is `Board::cdrom`.
#[derive(Default, Clone)]
pub struct Media {
    pub hda: Option<PathBuf>,
}

#[derive(Snap, Default)]
struct SavedMedia {
    hda_source: Option<String>,
    cdrom: Option<String>,
}

const CPU_SCHEMA: u64 = snapshot::mix(Cpu::SCHEMA, &[snapshot::fnv(b"tsc")]);

fn section(out: &mut Vec<u8>, tag: &[u8; 4], schema: u64, body: &[u8]) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&schema.to_le_bytes());
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(body);
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Makes `dst` an APFS clone of `src`, replacing what was there.
fn clone_file(src: &Path, dst: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn clonefile(src: *const std::ffi::c_char, dst: *const std::ffi::c_char, flags: u32) -> std::ffi::c_int;
    }
    match std::fs::remove_file(dst) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot replace {}: {e}", dst.display())),
    }
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("bad path {}", p.display()));
    let (s, d) = (c(src)?, c(dst)?);
    if unsafe { clonefile(s.as_ptr(), d.as_ptr(), 0) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(format!("cannot clone {} to {}: {e} (both must be on the same APFS volume)", src.display(), dst.display()));
    }
    Ok(())
}

fn workers(jobs: usize) -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).min(jobs).max(1)
}

fn compress_chunk(data: &[u8]) -> Vec<u8> {
    let pages = data.len().div_ceil(PAGE);
    let mut bitmap = vec![0u8; pages.div_ceil(8)];
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    for (i, page) in data.chunks(PAGE).enumerate() {
        if page.iter().any(|&b| b != 0) {
            bitmap[i / 8] |= 1 << (i % 8);
            enc.write_all(page).expect("deflate into memory");
        }
    }
    let mut out = bitmap;
    out.extend(enc.finish().expect("deflate into memory"));
    out
}

fn decompress_chunk(data: &[u8], out: &mut [u8]) -> Result<(), String> {
    let pages = out.len().div_ceil(PAGE);
    let (bitmap, deflated) = data.split_at_checked(pages.div_ceil(8)).ok_or("memory chunk too short")?;
    let mut dec = flate2::read::DeflateDecoder::new(deflated);
    for (i, page) in out.chunks_mut(PAGE).enumerate() {
        if bitmap[i / 8] & 1 << (i % 8) != 0 {
            dec.read_exact(page).map_err(|e| format!("memory image: {e}"))?;
        } else {
            page.fill(0);
        }
    }
    Ok(())
}

/// Compresses `mem` in 16 MiB chunks on all cores. Each chunk is a bitmap of
/// its non-zero pages followed by those pages, raw-deflated.
fn compress(mem: &[u8]) -> Vec<u8> {
    let chunks: Vec<&[u8]> = mem.chunks(CHUNK).collect();
    let mut packed: Vec<Vec<u8>> = vec![Vec::new(); chunks.len()];
    let per = chunks.len().div_ceil(workers(chunks.len()));
    std::thread::scope(|s| {
        for (outs, ins) in packed.chunks_mut(per).zip(chunks.chunks(per)) {
            s.spawn(move || outs.iter_mut().zip(ins).for_each(|(o, i)| *o = compress_chunk(i)));
        }
    });
    let mut w = Writer::new();
    w.u64(mem.len() as u64);
    w.u64(chunks.len() as u64);
    for p in &packed {
        w.blob(p);
    }
    w.buf
}

fn decompress(data: &[u8], mem: &mut [u8]) -> Result<(), String> {
    let mut r = Reader::new(data);
    let len = r.u64().map_err(|e| e.0)?;
    if len != mem.len() as u64 {
        return Err(format!("memory image is {len} bytes, this machine has {}", mem.len()));
    }
    let count = r.u64().map_err(|e| e.0)?;
    let mut chunks: Vec<&mut [u8]> = mem.chunks_mut(CHUNK).collect();
    if count != chunks.len() as u64 {
        return Err("memory image has the wrong number of chunks".into());
    }
    let mut packed = Vec::with_capacity(chunks.len());
    for _ in 0..count {
        packed.push(r.blob().map_err(|e| e.0)?);
    }
    let per = chunks.len().div_ceil(workers(chunks.len()));
    std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .chunks_mut(per)
            .zip(packed.chunks(per))
            .map(|(outs, ins)| s.spawn(move || outs.iter_mut().zip(ins).try_for_each(|(o, i)| decompress_chunk(i, o))))
            .collect();
        handles.into_iter().try_for_each(|h| h.join().unwrap_or_else(|_| Err("decompression panicked".into())))
    })
}

fn mib(n: usize) -> f64 {
    n as f64 / (1 << 20) as f64
}

/// Saves the machine into directory `dir`, creating it if needed, and
/// describes what it wrote. Call it only while the host owns the CPU
/// state, between runs of the JIT.
pub fn save(dir: &Path, cpu: &Cpu, board: &mut Board, media: &Media) -> Result<String, String> {
    let t0 = Instant::now();
    let mut devices = Writer::new();
    board.save_devices(&mut devices).map_err(|e| e.0)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    if let Some(hda) = &media.hda {
        board.flush_disks().map_err(|e| format!("cannot flush the disk: {e}"))?;
        clone_file(hda, &dir.join(DISK_FILE))?;
    } else {
        let _ = std::fs::remove_file(dir.join(DISK_FILE));
    }
    let mut cpu_state = Writer::new();
    cpu_state.put(cpu);
    cpu_state.u64(cpu.read_tsc());
    let mut mach = Writer::new();
    mach.put(&board.machine());
    let mut disk = Writer::new();
    disk.put(&SavedMedia { hda_source: media.hda.as_deref().map(path_str), cdrom: board.cdrom().map(str::to_string) });
    let ram = compress(board.ram());
    let vram = board.vram_mut().map(|v| compress(v)).unwrap_or_default();
    let mut out = Vec::with_capacity(ram.len() + vram.len() + devices.buf.len() + 4096);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    section(&mut out, b"MACH", Machine::SCHEMA, &mach.buf);
    section(&mut out, b"CPU ", CPU_SCHEMA, &cpu_state.buf);
    section(&mut out, b"DEVS", Board::devices_schema(), &devices.buf);
    section(&mut out, b"RAM ", 0, &ram);
    section(&mut out, b"VRAM", 0, &vram);
    section(&mut out, b"DISK", SavedMedia::SCHEMA, &disk.buf);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, &out).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(STATE_FILE)).map_err(|e| format!("cannot write {}: {e}", dir.display()))?;
    Ok(format!(
        "saved {} in {:.0} ms: {STATE_FILE} {:.1} MiB (RAM {:.0} MiB -> {:.1} MiB, VRAM {:.2} MiB){}",
        dir.display(),
        t0.elapsed().as_secs_f64() * 1e3,
        mib(out.len()),
        mib(board.ram().len()),
        mib(ram.len()),
        mib(vram.len()),
        if media.hda.is_some() { ", disk cloned" } else { "" }
    ))
}

/// A snapshot read from disk and checked for format and layout, not yet
/// applied to a machine.
pub struct Snapshot {
    pub dir: PathBuf,
    pub machine: Machine,
    cpu: Vec<u8>,
    devices: Vec<u8>,
    ram: Vec<u8>,
    vram: Vec<u8>,
    media: SavedMedia,
}

pub fn read(dir: &Path) -> Result<Snapshot, String> {
    let file = dir.join(STATE_FILE);
    let data = std::fs::read(&file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let mut r = Reader::new(&data);
    let bad = |e: snapshot::Error| format!("{}: {e}", file.display());
    if r.bytes(8).map_err(bad)? != MAGIC {
        return Err(format!("{} is not a pcslop snapshot", file.display()));
    }
    let version = r.u32().map_err(bad)?;
    if version != VERSION {
        return Err(format!("{} has format version {version}; this build reads version {VERSION}", file.display()));
    }
    let mut take = |tag: &[u8; 4], schema: u64| -> Result<Vec<u8>, String> {
        let name = String::from_utf8_lossy(tag);
        if r.bytes(4).map_err(bad)? != tag {
            return Err(format!("{}: expected section {name:?}", file.display()));
        }
        if r.u64().map_err(bad)? != schema {
            return Err(format!("{}: the {} state was saved by a build whose devices differ from this one's", file.display(), name.trim()));
        }
        Ok(r.blob().map_err(bad)?.to_vec())
    };
    let mach = take(b"MACH", Machine::SCHEMA)?;
    let cpu = take(b"CPU ", CPU_SCHEMA)?;
    let devices = take(b"DEVS", Board::devices_schema())?;
    let ram = take(b"RAM ", 0)?;
    let vram = take(b"VRAM", 0)?;
    let disk = take(b"DISK", SavedMedia::SCHEMA)?;
    let machine = snapshot::read_new(&mut Reader::new(&mach)).map_err(bad)?;
    let media = snapshot::read_new(&mut Reader::new(&disk)).map_err(bad)?;
    Ok(Snapshot { dir: dir.to_path_buf(), machine, cpu, devices, ram, vram, media })
}

/// Puts the machine in the state `snap` holds and returns the drives it now
/// uses, with a message. The disk becomes a fresh clone of the snapshot's,
/// so the snapshot itself never changes. The snapshot's ISO goes back in the
/// CD-ROM drive, unnoticed by the guest; a different `cdrom` goes in after
/// it as a disc change. The caller must then drop the JIT's translations.
pub fn restore(snap: &Snapshot, cpu: &mut Cpu, board: &mut Board, cdrom: Option<&Path>) -> Result<(Media, String), String> {
    let t0 = Instant::now();
    let diffs = snap.machine.differences(&board.machine());
    if !diffs.is_empty() {
        return Err(format!("the snapshot does not fit this machine:\n  {}", diffs.join("\n  ")));
    }
    let saved_iso = snap.media.cdrom.as_deref();
    let want_iso = cdrom.map(path_str);
    let want_iso = want_iso.as_deref().or(saved_iso);
    let iso_now = match saved_iso {
        Some(iso) if Path::new(iso).exists() => Some(iso),
        Some(iso) if cdrom.is_none() => return Err(format!("the CD-ROM image {iso} is gone; pass --cdrom with its new path")),
        _ => want_iso,
    };
    let mut media = Media { hda: None };
    if snap.media.hda_source.is_some() {
        let run = snap.dir.join(RUN_DISK_FILE);
        clone_file(&snap.dir.join(DISK_FILE), &run)?;
        media.hda = Some(run);
    }
    match &media.hda {
        Some(hda) => board.attach_disk(&path_str(hda)).map_err(|e| format!("cannot open {}: {e}", hda.display()))?,
        None => board.remove_disk(),
    }
    if board.cdrom() != iso_now {
        board.change_cdrom(iso_now).map_err(|e| format!("cannot open the CD-ROM image: {e}"))?;
    }
    let inconsistent = |e: String| format!("{e}; the machine is now in an inconsistent state, so load another snapshot or quit");
    board.load_devices(&mut Reader::new(&snap.devices)).map_err(|e| inconsistent(e.0))?;
    decompress(&snap.ram, board.ram_mut()).map_err(inconsistent)?;
    if let Some(v) = board.vram_mut() {
        decompress(&snap.vram, v).map_err(inconsistent)?;
    }
    let mut r = Reader::new(&snap.cpu);
    r.get(cpu).map_err(|e| inconsistent(e.0))?;
    let tsc = r.u64().map_err(|e| inconsistent(e.0))?;
    cpu.write_tsc(tsc);
    if want_iso != saved_iso {
        board.change_cdrom(want_iso).map_err(|e| format!("cannot open the CD-ROM image: {e}"))?;
    }
    let mut msg = format!("restored {} in {:.0} ms", snap.dir.display(), t0.elapsed().as_secs_f64() * 1e3);
    if let Some(hda) = &media.hda {
        msg += &format!(", disk on {}", hda.display());
    }
    Ok((media, msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_survives_compression_with_zero_and_data_pages() {
        let mut mem = vec![0u8; 3 * CHUNK + 5 * PAGE];
        for (i, b) in mem[CHUNK + 100..CHUNK + 3 * PAGE].iter_mut().enumerate() {
            *b = (i * 7) as u8;
        }
        *mem.last_mut().unwrap() = 9;
        let packed = compress(&mem);
        let mut back = vec![0xaau8; mem.len()];
        decompress(&packed, &mut back).unwrap();
        assert!(back == mem);
        assert!(packed.len() < 64 << 10, "{} bytes for three pages of data", packed.len());
    }

    #[test]
    fn a_memory_image_of_another_size_is_refused() {
        let packed = compress(&[1u8; 2 * PAGE]);
        assert!(decompress(&packed, &mut [0u8; 4 * PAGE]).is_err());
    }
}
