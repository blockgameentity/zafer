//! Where our mixed audio leaves the process on Linux: a PulseAudio
//! playback stream (PipeWire serves the same protocol), through
//! `libpulse-simple` loaded at run time, so building needs no audio
//! headers and a machine without it still runs, silently. Unit tests never
//! open a stream.
//!
//! A [`Sink`] takes interleaved float frames and blocks about as long as
//! they take to play, which is what paces the mixers that feed it.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Interleaved 32-bit float frames out, at real-time pace.
pub(crate) trait Sink: Send {
    fn write(&mut self, frames: &[f32]);
}

/// A playback stream named `name`, or a silent sink if there is no sound
/// server.
pub(crate) fn open(name: &str, rate: u32, channels: u8) -> Box<dyn Sink> {
    if !cfg!(test) {
        match Pulse::open(name, rate, channels) {
            Ok(sink) => return Box::new(sink),
            Err(why) => tracing::warn!(%why, "no audio output; playing silently"),
        }
    }
    Box::new(Clock::new(rate, channels))
}

/// No device: keeps time only.
pub(crate) struct Clock {
    rate: u32,
    channels: usize,
    start: Instant,
    frames: u64,
}

impl Clock {
    pub(crate) fn new(rate: u32, channels: u8) -> Clock {
        Clock {
            rate: rate.max(1),
            channels: usize::from(channels.max(1)),
            start: Instant::now(),
            frames: 0,
        }
    }
}

impl Sink for Clock {
    fn write(&mut self, frames: &[f32]) {
        self.frames += (frames.len() / self.channels) as u64;
        let due = self.start + Duration::from_secs_f64(self.frames as f64 / f64::from(self.rate));
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > Duration::from_millis(200) {
            // Far behind (a stall): start over instead of rushing to catch up.
            self.start = now;
            self.frames = 0;
        }
    }
}

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type New = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type Write = unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int;
type Free = unsafe extern "C" fn(*mut c_void);

struct Library {
    new: New,
    write: Write,
    free: Free,
}

const PA_STREAM_PLAYBACK: c_int = 1;
const PA_SAMPLE_FLOAT32LE: c_int = 5;
/// Frames the server holds ahead of the speakers.
const LATENCY: Duration = Duration::from_millis(40);

fn library() -> Option<&'static Library> {
    static LIB: OnceLock<Option<Library>> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: loading a system library by soname, then looking up
        // three functions whose C signatures are the types above.
        unsafe {
            let lib = libc::dlopen(c"libpulse-simple.so.0".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let sym = |name: &CStr| libc::dlsym(lib, name.as_ptr());
            let (new, write, free) =
                (sym(c"pa_simple_new"), sym(c"pa_simple_write"), sym(c"pa_simple_free"));
            if new.is_null() || write.is_null() || free.is_null() {
                return None;
            }
            Some(Library {
                new: std::mem::transmute::<*mut c_void, New>(new),
                write: std::mem::transmute::<*mut c_void, Write>(write),
                free: std::mem::transmute::<*mut c_void, Free>(free),
            })
        }
    })
    .as_ref()
}

/// A `pa_simple` playback stream; falls back to keeping time if the
/// server goes away.
struct Pulse {
    lib: &'static Library,
    stream: *mut c_void,
    fallback: Option<Clock>,
    rate: u32,
    channels: u8,
}

// SAFETY: a pa_simple stream is used from one thread at a time (the
// mixer's), which `&mut self` guarantees.
unsafe impl Send for Pulse {}

impl Pulse {
    fn open(name: &str, rate: u32, channels: u8) -> Result<Pulse, String> {
        let lib = library().ok_or("libpulse-simple.so.0 not found")?;
        let app = CString::new("VIctory").expect("no NUL");
        let stream_name = CString::new(name).map_err(|e| e.to_string())?;
        let spec = SampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate,
            channels,
        };
        let frame = 4 * u32::from(channels);
        let ahead = (LATENCY.as_secs_f64() * f64::from(rate)) as u32 * frame;
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: ahead,
            prebuf: u32::MAX,
            minreq: ahead / 4,
            fragsize: u32::MAX,
        };
        let mut error = 0;
        // SAFETY: valid strings and structures for the duration of the call.
        let stream = unsafe {
            (lib.new)(
                std::ptr::null(),
                app.as_ptr(),
                PA_STREAM_PLAYBACK,
                std::ptr::null(),
                stream_name.as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut error,
            )
        };
        if stream.is_null() {
            return Err(format!("pa_simple_new failed ({error})"));
        }
        tracing::info!(name, rate, channels, "audio output open");
        Ok(Pulse {
            lib,
            stream,
            fallback: None,
            rate,
            channels,
        })
    }
}

impl Sink for Pulse {
    fn write(&mut self, frames: &[f32]) {
        if let Some(clock) = &mut self.fallback {
            return clock.write(frames);
        }
        let mut error = 0;
        // SAFETY: an open stream and `frames.len()` floats.
        let r = unsafe {
            (self.lib.write)(self.stream, frames.as_ptr().cast(), size_of_val(frames), &mut error)
        };
        if r < 0 {
            tracing::warn!(error, "audio output lost; playing silently");
            let mut clock = Clock::new(self.rate, self.channels);
            clock.write(frames);
            self.fallback = Some(clock);
        }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        // SAFETY: the stream from pa_simple_new, freed once.
        unsafe { (self.lib.free)(self.stream) };
    }
}

/// Downmix gains to stereo for each speaker of a channel mask
/// (`SPEAKER_*` bits, the order of interleaved channels): left, right.
pub(crate) fn stereo_gains(speaker: u32) -> (f32, f32) {
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match speaker {
        0x1 | 0x40 => (1.0, 0.0),                    // front left, left of centre
        0x2 | 0x80 => (0.0, 1.0),                    // front right, right of centre
        0x10 | 0x200 | 0x1000 | 0x8000 => (H, 0.0),  // back, side, top front, top back left
        0x20 | 0x400 | 0x4000 | 0x20000 => (0.0, H), // and right
        0x8 => (0.0, 0.0),                           // LFE: dropped, as in ITU downmixes
        _ => (H, H),                                 // the centres
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_keeps_real_time() {
        let mut clock = Clock::new(48_000, 2);
        let start = Instant::now();
        for _ in 0..5 {
            clock.write(&[0.0; 960]); // 10 ms each
        }
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(45), "{took:?}");
    }

    #[test]
    fn stereo_downmix_keeps_sides() {
        assert_eq!(stereo_gains(0x1), (1.0, 0.0));
        assert_eq!(stereo_gains(0x2), (0.0, 1.0));
        assert_eq!(stereo_gains(0x20).0, 0.0);
        assert_eq!(stereo_gains(0x8), (0.0, 0.0));
    }

    /// Plays half a second of a quiet tone through the sound server.
    #[test]
    #[ignore = "makes sound; needs a PulseAudio or PipeWire server"]
    fn a_tone_reaches_the_sound_server() {
        let mut sink = Pulse::open("test tone", 48_000, 2).expect("a sound server");
        let start = Instant::now();
        for block in 0..50 {
            let frames: Vec<f32> = (0..480)
                .flat_map(|i| {
                    let t = (block * 480 + i) as f32 / 48_000.0;
                    let x = 0.1 * (t * 440.0 * std::f32::consts::TAU).sin();
                    [x, x]
                })
                .collect();
            sink.write(&frames);
        }
        // Writes block as the server plays: at least the half second less
        // what it buffers ahead.
        assert!(start.elapsed() >= Duration::from_millis(400), "{:?}", start.elapsed());
    }
}
