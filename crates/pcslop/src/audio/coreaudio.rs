//! Plays a [`Ring`] on the default output device through Apple's
//! DefaultOutput audio unit. The unit's render callback runs on CoreAudio's
//! real-time thread, pulls guest-rate frames and resamples them to the
//! device rate.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Ring;

type OsStatus = i32;
type AudioUnit = *mut c_void;

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    sub_type: u32,
    manufacturer: u32,
    flags: u32,
    flags_mask: u32,
}

#[repr(C)]
#[derive(Default)]
struct StreamDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    bytes: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type RenderProc = extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferList) -> OsStatus;

#[repr(C)]
struct RenderCallback {
    proc_: RenderProc,
    refcon: *mut c_void,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(after: *mut c_void, desc: *const AudioComponentDescription) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, out: *mut AudioUnit) -> OsStatus;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> OsStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OsStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OsStatus;
    fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OsStatus;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OsStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OsStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OsStatus;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const PROPERTY_STREAM_FORMAT: u32 = 8;
const PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const SCOPE_INPUT: u32 = 1;
const SCOPE_OUTPUT: u32 = 2;
const FORMAT_FLAG_FLOAT: u32 = 1;
const FORMAT_FLAG_PACKED: u32 = 8;

struct Render {
    ring: Arc<Ring>,
    tap: Option<Arc<Ring>>,
    device_rate: f64,
    pos: f64,
    prev: [f32; 2],
    cur: [f32; 2],
}

extern "C" fn render(refcon: *mut c_void, _flags: *mut u32, _time: *const c_void, _bus: u32, frames: u32, data: *mut AudioBufferList) -> OsStatus {
    let r = unsafe { &mut *(refcon as *mut Render) };
    let buf = unsafe { &mut (*data).buffers[0] };
    let out = unsafe { std::slice::from_raw_parts_mut(buf.data as *mut [f32; 2], frames as usize) };
    let step = r.ring.rate() as f64 / r.device_rate;
    for o in out {
        while r.pos >= 1.0 {
            r.prev = r.cur;
            let f = r.ring.pop();
            if let Some(t) = &r.tap {
                t.rate.store(r.ring.rate(), Ordering::Relaxed);
                t.push(&[f]);
            }
            r.cur = f.map(|s| s as f32 / 32768.0);
            r.pos -= 1.0;
        }
        let t = r.pos as f32;
        *o = [0, 1].map(|c| r.prev[c] + (r.cur[c] - r.prev[c]) * t);
        r.pos += step;
    }
    0
}

pub struct Output {
    unit: AudioUnit,
    render: *mut Render,
    device_rate: f64,
}

// The unit is only started, stopped and disposed of through this handle, and
// the render state is touched only by CoreAudio's thread until Drop stops it.
unsafe impl Send for Output {}

fn check(status: OsStatus, what: &str) -> Result<(), String> {
    if status == 0 { Ok(()) } else { Err(format!("{what} failed with OSStatus {status}")) }
}

impl Output {
    /// Plays `ring`, copying each frame it takes into `tap`.
    pub fn start(ring: Arc<Ring>, tap: Option<Arc<Ring>>) -> Result<Output, String> {
        let desc = AudioComponentDescription {
            component_type: fourcc(b"auou"),
            sub_type: fourcc(b"def "),
            manufacturer: fourcc(b"appl"),
            flags: 0,
            flags_mask: 0,
        };
        let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc) };
        if component.is_null() {
            return Err("no default output audio unit".into());
        }
        let mut unit = std::ptr::null_mut();
        check(unsafe { AudioComponentInstanceNew(component, &mut unit) }, "AudioComponentInstanceNew")?;
        let render = Box::into_raw(Box::new(Render { ring, tap, device_rate: 48000.0, pos: 1.0, prev: [0.0; 2], cur: [0.0; 2] }));
        let mut out = Output { unit, render, device_rate: 0.0 };
        out.configure()?;
        check(unsafe { AudioOutputUnitStart(out.unit) }, "AudioOutputUnitStart")?;
        Ok(out)
    }

    fn configure(&mut self) -> Result<(), String> {
        let mut hw = StreamDescription::default();
        let mut size = size_of::<StreamDescription>() as u32;
        check(
            unsafe { AudioUnitGetProperty(self.unit, PROPERTY_STREAM_FORMAT, SCOPE_OUTPUT, 0, &mut hw as *mut _ as *mut c_void, &mut size) },
            "reading the device format",
        )?;
        let rate = if hw.sample_rate > 0.0 { hw.sample_rate } else { 48000.0 };
        unsafe { (*self.render).device_rate = rate };
        self.device_rate = rate;
        let fmt = StreamDescription {
            sample_rate: rate,
            format_id: fourcc(b"lpcm"),
            format_flags: FORMAT_FLAG_FLOAT | FORMAT_FLAG_PACKED,
            bytes_per_packet: 8,
            frames_per_packet: 1,
            bytes_per_frame: 8,
            channels_per_frame: 2,
            bits_per_channel: 32,
            reserved: 0,
        };
        check(
            unsafe {
                AudioUnitSetProperty(self.unit, PROPERTY_STREAM_FORMAT, SCOPE_INPUT, 0, &fmt as *const _ as *const c_void, size_of::<StreamDescription>() as u32)
            },
            "setting the stream format",
        )?;
        let cb = RenderCallback { proc_: render, refcon: self.render as *mut c_void };
        check(
            unsafe {
                AudioUnitSetProperty(self.unit, PROPERTY_SET_RENDER_CALLBACK, SCOPE_INPUT, 0, &cb as *const _ as *const c_void, size_of::<RenderCallback>() as u32)
            },
            "setting the render callback",
        )?;
        check(unsafe { AudioUnitInitialize(self.unit) }, "AudioUnitInitialize")
    }

    pub fn device_rate(&self) -> f64 {
        self.device_rate
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
            drop(Box::from_raw(self.render));
        }
    }
}
