//! XAudio2 (`xaudio2_9.dll`, `XAudio2Create` is ordinal 1): Bink plays a
//! video's sound through it.
//!
//! One engine thread per `IXAudio2` runs a pass every 10 ms, as the real
//! engine does: callbacks, then source voices (decoded from the title's
//! buffers and resampled), submix voices by processing stage, the
//! mastering voice, downmixed to stereo into an [`audio_out`] stream whose
//! blocking writes pace the passes. Every voice renders at the mastering
//! voice's rate. Callbacks run on the engine thread with no lock held, so
//! they may submit buffers.
//!
//! Formats: integer PCM (8 to 32 bits) and 32-bit float, plain or
//! extensible. Effects are kept but not run (their parameters read back as
//! zeros), filters are kept but not applied, and changes made with an
//! operation set apply at once.
// ponytail: linear resampling, no effects or filters; a windowed-sinc
// resampler and the XAPO chain when a title needs them.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use victory_loader::linux::thread as guest_thread;

use super::audio_out;
use super::combase::{
    self, Com, E_INVALIDARG, E_POINTER, Guid, HRESULT, S_OK, Vtbl, com_vtbl, guid, hand_out, put,
};
use super::mmdevapi::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WaveFormat, WaveFormatExtensible};
use crate::{HleExport, HleModule, hle_export};

const XAUDIO2_E_INVALID_CALL: HRESULT = 0x8896_0001_u32 as i32;
const XAUDIO2_MAX_QUEUED_BUFFERS: usize = 64;
const XAUDIO2_END_OF_STREAM: u32 = 0x40;
const XAUDIO2_LOOP_INFINITE: u32 = 255;
const XAUDIO2_VOICE_NOSAMPLESPLAYED: u32 = 0x100;
const XAUDIO2_MIN_FREQ_RATIO: f32 = 1.0 / 1024.0;
const XAUDIO2_DEFAULT_FREQ_RATIO: f32 = 2.0;
const XAUDIO2_MAX_FREQ_RATIO: f32 = 1024.0;
const DEFAULT_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: u32 = 2;

const IID_IXAUDIO2: Guid = guid(
    0x2B02_E3CF,
    0x2E0B,
    0x4EC3,
    [0xBE, 0x45, 0x1B, 0x2A, 0x3F, 0xE7, 0x21, 0x0D],
);

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_PCM: Guid = guid(
    0x0000_0001,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- guest structures ----

/// `XAUDIO2_BUFFER`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Buffer {
    flags: u32,
    audio_bytes: u32,
    audio_data: *const u8,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u32,
    context: *mut c_void,
}

// SAFETY: the title's audio and context pointers, read only on the
// engine thread under the mixer lock while the title keeps the buffer.
unsafe impl Send for Buffer {}

/// `XAUDIO2_VOICE_STATE`.
#[repr(C)]
struct VoiceStateOut {
    current_context: *mut c_void,
    buffers_queued: u32,
    samples_played: u64,
}

/// `XAUDIO2_VOICE_DETAILS`.
#[repr(C)]
struct VoiceDetails {
    creation_flags: u32,
    active_flags: u32,
    input_channels: u32,
    input_rate: u32,
}

/// `XAUDIO2_SEND_DESCRIPTOR`.
#[repr(C)]
struct SendDescriptor {
    flags: u32,
    output: *mut Voice,
}

/// `XAUDIO2_VOICE_SENDS`.
#[repr(C)]
struct Sends {
    count: u32,
    sends: *const SendDescriptor,
}

/// `XAUDIO2_EFFECT_CHAIN`.
#[repr(C)]
struct EffectChain {
    count: u32,
    effects: *const c_void,
}

/// `XAUDIO2_FILTER_PARAMETERS`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Filter {
    kind: u32,
    frequency: f32,
    one_over_q: f32,
}

const DEFAULT_FILTER: Filter = Filter {
    kind: 0,
    frequency: 1.0,
    one_over_q: 1.0,
};

// ---- the engine ----

#[repr(C)]
struct Engine {
    vtbl: &'static Vtbl<13>,
    refs: AtomicU32,
    shared: Arc<Shared>,
    exited: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Com for Engine {
    const IIDS: &'static [Guid] = &[IID_IXAUDIO2];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

struct Shared {
    mix: Mutex<Mix>,
    /// Held for a whole pass: a voice is destroyed, or the engine stopped,
    /// only between passes.
    pass: Mutex<()>,
    running: AtomicBool,
    quit: AtomicBool,
    wake: (Mutex<()>, Condvar),
    /// The engine thread's id, once it runs.
    thread: AtomicU32,
}

impl Shared {
    fn on_engine_thread(&self) -> bool {
        self.thread.load(Ordering::Acquire) == guest_thread::current_id()
    }

    /// The pass lock, unless this is the engine thread (inside a callback).
    fn between_passes(&self) -> Option<MutexGuard<'_, ()>> {
        (!self.on_engine_thread()).then(|| lock(&self.pass))
    }

    fn wake(&self) {
        let _g = lock(&self.wake.0);
        self.wake.1.notify_all();
    }
}

#[derive(Default)]
struct Mix {
    voices: HashMap<usize, VoiceState>,
    master: Option<usize>,
    callbacks: Vec<usize>,
}

static ENGINE: Vtbl<13> = com_vtbl!(Engine;
    register_callbacks, unregister_callbacks, create_source_voice, create_submix_voice,
    create_mastering_voice, start_engine, stop_engine, commit_changes, performance_data,
    debug_configuration);

fn engine<'a>(this: *mut Engine) -> &'a Shared {
    &combase::this(this).shared
}

pub extern "win64" fn XAudio2Create(out: *mut *mut c_void, _flags: u32, _processor: u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    let shared = Arc::new(Shared {
        mix: Mutex::default(),
        pass: Mutex::new(()),
        running: AtomicBool::new(true),
        quit: AtomicBool::new(false),
        wake: (Mutex::new(()), Condvar::new()),
        thread: AtomicU32::new(0),
    });
    let (tx, rx) = mpsc::channel();
    let param = Arc::into_raw(shared.clone()) as *mut c_void;
    let spawned = guest_thread::spawn(
        engine_thread,
        param,
        0,
        Box::new(move |_| {
            let _ = tx.send(());
        }),
    );
    if let Err(e) = spawned {
        // SAFETY: the thread never started, so the reference is still ours.
        drop(unsafe { Arc::from_raw(param as *const Shared) });
        tracing::warn!(%e, "XAudio2Create: no engine thread");
        return combase::E_FAIL;
    }
    let e = combase::new(Engine {
        vtbl: &ENGINE,
        refs: AtomicU32::new(0),
        shared,
        exited: Mutex::new(Some(rx)),
    });
    hand_out(e, &IID_IXAUDIO2, out)
}

pub extern "win64" fn XAudio2CreateWithVersionInfo(
    out: *mut *mut c_void,
    flags: u32,
    processor: u32,
    _version: u32,
) -> HRESULT {
    XAudio2Create(out, flags, processor)
}

impl Drop for Engine {
    /// Releasing the engine destroys every voice.
    fn drop(&mut self) {
        let s = &self.shared;
        s.quit.store(true, Ordering::Release);
        s.wake();
        if !s.on_engine_thread()
            && let Some(rx) = lock(&self.exited).take()
        {
            let _ = rx.recv();
        }
        let voices: Vec<usize> = lock(&s.mix).voices.drain().map(|(k, _)| k).collect();
        for v in voices {
            // SAFETY: a voice header from `Box::into_raw`, not yet destroyed.
            drop(unsafe { Box::from_raw(v as *mut Voice) });
        }
    }
}

extern "win64" fn register_callbacks(this: *mut Engine, callback: *mut c_void) -> HRESULT {
    if callback.is_null() {
        return E_INVALIDARG;
    }
    let mut mix = lock(&engine(this).mix);
    if !mix.callbacks.contains(&(callback as usize)) {
        mix.callbacks.push(callback as usize);
    }
    S_OK
}

extern "win64" fn unregister_callbacks(this: *mut Engine, callback: *mut c_void) {
    let s = engine(this);
    let _between = s.between_passes();
    lock(&s.mix).callbacks.retain(|&c| c != callback as usize);
}

extern "win64" fn start_engine(this: *mut Engine) -> HRESULT {
    let s = engine(this);
    s.running.store(true, Ordering::Release);
    s.wake();
    S_OK
}

extern "win64" fn stop_engine(this: *mut Engine) {
    let s = engine(this);
    let _between = s.between_passes();
    s.running.store(false, Ordering::Release);
}

extern "win64" fn commit_changes(_this: *mut Engine, _operation_set: u32) -> HRESULT {
    S_OK
}

/// `XAUDIO2_PERFORMANCE_DATA`: counts only.
extern "win64" fn performance_data(this: *mut Engine, out: *mut [u32; 16]) {
    let mix = lock(&engine(this).mix);
    let sources = mix.voices.values().filter(|v| matches!(v.kind, Kind::Source(_))).count() as u32;
    let mut data = [0u32; 16];
    data[7] = (mixer_frames(&mix) * 2) as u32; // CurrentLatencyInSamples: two passes
    data[9] = sources; // ActiveSourceVoiceCount
    data[10] = sources; // TotalSourceVoiceCount
    put(out, data);
}

extern "win64" fn debug_configuration(_this: *mut Engine, _config: *const c_void, _reserved: *mut c_void) {}

fn mixer_frames(mix: &Mix) -> usize {
    let rate = mix.master.and_then(|m| mix.voices.get(&m)).map_or(DEFAULT_RATE, |v| v.rate);
    (rate / 100) as usize
}

// ---- voices ----

/// What the title holds: an `IXAudio2Voice` (no `IUnknown`).
#[repr(C)]
struct Voice {
    vtbl: *const (),
    shared: Arc<Shared>,
}

struct VoiceState {
    kind: Kind,
    flags: u32,
    channels: usize,
    rate: u32,
    stage: u32,
    volume: f32,
    channel_volumes: Vec<f32>,
    sends: Vec<Route>,
    effects: Vec<bool>,
    filter: Filter,
    /// What other voices sent this pass.
    input: Vec<f32>,
}

struct Route {
    to: usize,
    filter: Filter,
    /// `[destination channel][source channel]`.
    matrix: Vec<f32>,
}

enum Kind {
    Source(Box<Source>),
    Submix,
    Master { mask: u32 },
}

struct Source {
    format: Sample,
    block: usize,
    callback: usize,
    started: bool,
    ratio: f32,
    max_ratio: f32,
    queue: VecDeque<Queued>,
    played: u64,
    /// Interpolation: the frames around the read position.
    prev: Vec<f32>,
    next: Vec<f32>,
    frac: f64,
    /// Events from calls between passes (flushes), fired next pass.
    pending: Vec<Event>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Sample {
    U8,
    I16,
    I24,
    I32,
    F32,
}

struct Queued {
    buffer: Buffer,
    pos: u32,
    end: u32,
    loop_begin: u32,
    loop_end: u32,
    loops: u32,
    begun: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Event {
    BufferStart(usize),
    BufferEnd(usize),
    LoopEnd(usize),
    StreamEnd,
}

/// Which sends a new voice gets: the title's list, or the mastering voice.
fn initial_sends(mix: &Mix, channels: usize, list: *const Sends) -> Result<Vec<Route>, HRESULT> {
    if list.is_null() {
        let Some(m) = mix.master else {
            return Err(XAUDIO2_E_INVALID_CALL);
        };
        return Ok(vec![send_to(mix, channels, m)]);
    }
    // SAFETY: checked non-null; `count` descriptors from the title.
    let list = unsafe { &*list };
    if list.count != 0 && list.sends.is_null() {
        return Err(E_INVALIDARG);
    }
    (0..list.count as usize)
        .map(|i| {
            // SAFETY: within the title's array.
            let to = unsafe { (*list.sends.add(i)).output } as usize;
            if mix.voices.get(&to).is_none_or(|v| matches!(v.kind, Kind::Source(_))) {
                return Err(E_INVALIDARG);
            }
            Ok(send_to(mix, channels, to))
        })
        .collect()
}

fn send_to(mix: &Mix, channels: usize, to: usize) -> Route {
    let dest = mix.voices.get(&to).map_or(DEFAULT_CHANNELS as usize, |v| v.channels);
    Route {
        to,
        filter: DEFAULT_FILTER,
        matrix: default_matrix(channels, dest),
    }
}

/// XAudio2's default mapping: identity, mono to every channel of stereo,
/// stereo averaged to mono.
fn default_matrix(src: usize, dst: usize) -> Vec<f32> {
    let mut m = vec![0.0; src * dst];
    match (src, dst) {
        (1, 2) => m.fill(1.0),
        (2, 1) => m.fill(0.5),
        _ => (0..src.min(dst)).for_each(|i| m[i * src + i] = 1.0),
    }
    m
}

fn effect_count(chain: *const EffectChain) -> Result<usize, HRESULT> {
    if chain.is_null() {
        return Ok(0);
    }
    // SAFETY: checked non-null.
    let chain = unsafe { &*chain };
    if chain.count != 0 && chain.effects.is_null() {
        return Err(E_INVALIDARG);
    }
    Ok(chain.count as usize)
}

fn new_voice(shared: &Arc<Shared>, mix: &mut Mix, vtbl: *const (), state: VoiceState) -> *mut Voice {
    let v = Box::into_raw(Box::new(Voice {
        vtbl,
        shared: shared.clone(),
    }));
    mix.voices.insert(v as usize, state);
    v
}

fn base_state(kind: Kind, flags: u32, channels: usize, rate: u32) -> VoiceState {
    VoiceState {
        kind,
        flags,
        channels,
        rate,
        stage: 0,
        volume: 1.0,
        channel_volumes: vec![1.0; channels],
        sends: Vec::new(),
        effects: Vec::new(),
        filter: DEFAULT_FILTER,
        input: Vec::new(),
    }
}

/// The sample type of a supported format, and its block size.
fn sample_format(format: *const WaveFormat) -> Option<(Sample, usize, usize, u32)> {
    if format.is_null() {
        return None;
    }
    // SAFETY: a WAVEFORMATEX from the title; its extensible part is read
    // only when the tag says it is there.
    let (f, sub) = unsafe {
        let f = format.read_unaligned();
        let sub = (f.tag == WAVE_FORMAT_EXTENSIBLE && f.extra >= 22)
            .then(|| (format as *const WaveFormatExtensible).read_unaligned().sub_format);
        (f, sub)
    };
    let float = match (f.tag, sub) {
        (WAVE_FORMAT_PCM, _) => false,
        (WAVE_FORMAT_IEEE_FLOAT, _) => true,
        (WAVE_FORMAT_EXTENSIBLE, Some(KSDATAFORMAT_SUBTYPE_PCM)) => false,
        (WAVE_FORMAT_EXTENSIBLE, Some(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)) => true,
        _ => return None,
    };
    let sample = match (float, f.bits) {
        (true, 32) => Sample::F32,
        (false, 8) => Sample::U8,
        (false, 16) => Sample::I16,
        (false, 24) => Sample::I24,
        (false, 32) => Sample::I32,
        _ => return None,
    };
    let channels = usize::from(f.channels);
    let block = usize::from(f.block_align);
    let width = usize::from(f.bits / 8);
    if channels == 0 || channels > 64 || f.rate == 0 || block < channels * width {
        return None;
    }
    Some((sample, channels, block, f.rate))
}

extern "win64" fn create_source_voice(
    this: *mut Engine,
    out: *mut *mut Voice,
    format: *const WaveFormat,
    flags: u32,
    max_ratio: f32,
    callback: *mut c_void,
    sends: *const Sends,
    effects: *const EffectChain,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    let Some((sample, channels, block, rate)) = sample_format(format) else {
        // SAFETY: a WAVEFORMATEX or null; only its tag is read.
        let tag = (!format.is_null()).then(|| unsafe { format.read_unaligned().tag });
        tracing::warn!(?tag, "CreateSourceVoice: format not supported");
        return XAUDIO2_E_INVALID_CALL;
    };
    let max_ratio = if max_ratio <= 0.0 { XAUDIO2_DEFAULT_FREQ_RATIO } else { max_ratio }
        .clamp(XAUDIO2_MIN_FREQ_RATIO, XAUDIO2_MAX_FREQ_RATIO);
    let shared = &combase::this(this).shared;
    let mut mix = lock(&shared.mix);
    let sends = match initial_sends(&mix, channels, sends) {
        Ok(s) => s,
        Err(hr) => return hr,
    };
    let effects = match effect_count(effects) {
        Ok(n) => n,
        Err(hr) => return hr,
    };
    let source = Source {
        format: sample,
        block,
        callback: callback as usize,
        started: false,
        ratio: 1.0,
        max_ratio,
        queue: VecDeque::new(),
        played: 0,
        prev: vec![0.0; channels],
        next: vec![0.0; channels],
        frac: 1.0,
        pending: Vec::new(),
    };
    let mut state = base_state(Kind::Source(Box::new(source)), flags, channels, rate);
    state.sends = sends;
    state.effects = vec![true; effects];
    let v = new_voice(shared, &mut mix, &SOURCE_VOICE as *const _ as *const (), state);
    put(out, v);
    S_OK
}

extern "win64" fn create_submix_voice(
    this: *mut Engine,
    out: *mut *mut Voice,
    channels: u32,
    rate: u32,
    flags: u32,
    stage: u32,
    sends: *const Sends,
    effects: *const EffectChain,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    if channels == 0 || channels > 64 || rate == 0 {
        return XAUDIO2_E_INVALID_CALL;
    }
    let shared = &combase::this(this).shared;
    let mut mix = lock(&shared.mix);
    let sends = match initial_sends(&mix, channels as usize, sends) {
        Ok(s) => s,
        Err(hr) => return hr,
    };
    let effects = match effect_count(effects) {
        Ok(n) => n,
        Err(hr) => return hr,
    };
    let mut state = base_state(Kind::Submix, flags, channels as usize, rate);
    state.stage = stage;
    state.sends = sends;
    state.effects = vec![true; effects];
    let v = new_voice(shared, &mut mix, &SUBMIX_VOICE as *const _ as *const (), state);
    put(out, v);
    S_OK
}

/// The `SPEAKER_*` mask XAudio2 picks for a channel count.
fn channel_mask(channels: u32) -> u32 {
    match channels {
        1 => 0x4,
        2 => 0x3,
        3 => 0x7,
        4 => 0x33,
        5 => 0x37,
        6 => 0x3F,
        7 => 0x13F,
        8 => 0x63F,
        _ => 0,
    }
}

extern "win64" fn create_mastering_voice(
    this: *mut Engine,
    out: *mut *mut Voice,
    channels: u32,
    rate: u32,
    flags: u32,
    _device: *const u16,
    effects: *const EffectChain,
    _category: u32,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    let channels = if channels == 0 { DEFAULT_CHANNELS } else { channels };
    let rate = if rate == 0 { DEFAULT_RATE } else { rate };
    if channels > 64 || !(1000..=200_000).contains(&rate) {
        return XAUDIO2_E_INVALID_CALL;
    }
    let shared = &combase::this(this).shared;
    let mut mix = lock(&shared.mix);
    if mix.master.is_some() {
        return XAUDIO2_E_INVALID_CALL;
    }
    let effects = match effect_count(effects) {
        Ok(n) => n,
        Err(hr) => return hr,
    };
    let kind = Kind::Master {
        mask: channel_mask(channels),
    };
    let mut state = base_state(kind, flags, channels as usize, rate);
    state.effects = vec![true; effects];
    let v = new_voice(shared, &mut mix, &MASTER_VOICE as *const _ as *const (), state);
    mix.master = Some(v as usize);
    tracing::info!(channels, rate, "XAudio2 mastering voice");
    put(out, v);
    S_OK
}

/// Runs `f` on `this` voice's state under the mixer lock.
fn with<R>(this: *mut Voice, default: R, f: impl FnOnce(&mut VoiceState) -> R) -> R {
    // SAFETY: a voice header the title got from us and has not destroyed.
    let shared = unsafe { &(*this).shared };
    let mut mix = lock(&shared.mix);
    mix.voices.get_mut(&(this as usize)).map_or(default, f)
}

fn with_source<R>(this: *mut Voice, default: R, f: impl FnOnce(&mut Source) -> R) -> R {
    with(this, Some(default), |v| match &mut v.kind {
        Kind::Source(s) => Some(f(s)),
        _ => None,
    })
    .expect("a default")
}

extern "win64" fn voice_details(this: *mut Voice, out: *mut VoiceDetails) {
    let d = with(this, None, |v| {
        Some(VoiceDetails {
            creation_flags: v.flags,
            active_flags: 0,
            input_channels: v.channels as u32,
            input_rate: v.rate,
        })
    });
    if let Some(d) = d {
        put(out, d);
    }
}

extern "win64" fn set_output_voices(this: *mut Voice, sends: *const Sends) -> HRESULT {
    // SAFETY: as in `with`.
    let shared = unsafe { &(*this).shared };
    let mut mix = lock(&shared.mix);
    let Some(channels) = mix.voices.get(&(this as usize)).map(|v| v.channels) else {
        return XAUDIO2_E_INVALID_CALL;
    };
    if mix.master == Some(this as usize) {
        return XAUDIO2_E_INVALID_CALL;
    }
    let list = match initial_sends(&mix, channels, sends) {
        Ok(s) => s,
        Err(hr) => return hr,
    };
    if let Some(v) = mix.voices.get_mut(&(this as usize)) {
        v.sends = list;
    }
    S_OK
}

extern "win64" fn set_effect_chain(this: *mut Voice, chain: *const EffectChain) -> HRESULT {
    match effect_count(chain) {
        Ok(n) => with(this, XAUDIO2_E_INVALID_CALL, |v| {
            v.effects = vec![true; n];
            S_OK
        }),
        Err(hr) => hr,
    }
}

fn set_effect(this: *mut Voice, index: u32, on: bool) -> HRESULT {
    with(this, XAUDIO2_E_INVALID_CALL, |v| match v.effects.get_mut(index as usize) {
        Some(e) => {
            *e = on;
            S_OK
        }
        None => E_INVALIDARG,
    })
}

extern "win64" fn enable_effect(this: *mut Voice, index: u32, _operation_set: u32) -> HRESULT {
    set_effect(this, index, true)
}

extern "win64" fn disable_effect(this: *mut Voice, index: u32, _operation_set: u32) -> HRESULT {
    set_effect(this, index, false)
}

extern "win64" fn effect_state(this: *mut Voice, index: u32, out: *mut i32) {
    let on = with(this, false, |v| v.effects.get(index as usize).copied().unwrap_or(false));
    put(out, on as i32);
}

extern "win64" fn set_effect_parameters(
    this: *mut Voice,
    index: u32,
    _params: *const c_void,
    _size: u32,
    _operation_set: u32,
) -> HRESULT {
    with(this, XAUDIO2_E_INVALID_CALL, |v| {
        if (index as usize) < v.effects.len() { S_OK } else { E_INVALIDARG }
    })
}

extern "win64" fn get_effect_parameters(this: *mut Voice, index: u32, out: *mut u8, size: u32) -> HRESULT {
    let hr = set_effect_parameters(this, index, std::ptr::null(), 0, 0);
    if hr == S_OK && !out.is_null() {
        // SAFETY: the title gives `size` writable bytes.
        unsafe { out.write_bytes(0, size as usize) };
    }
    hr
}

extern "win64" fn set_filter(this: *mut Voice, filter: *const Filter, _operation_set: u32) -> HRESULT {
    if filter.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    let f = unsafe { *filter };
    with(this, XAUDIO2_E_INVALID_CALL, |v| {
        v.filter = f;
        S_OK
    })
}

extern "win64" fn get_filter(this: *mut Voice, out: *mut Filter) {
    put(out, with(this, DEFAULT_FILTER, |v| v.filter));
}

extern "win64" fn set_output_filter(
    this: *mut Voice,
    to: *mut Voice,
    filter: *const Filter,
    _operation_set: u32,
) -> HRESULT {
    if filter.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    let f = unsafe { *filter };
    with(this, XAUDIO2_E_INVALID_CALL, |v| {
        match v.sends.iter_mut().find(|s| s.to == to as usize || to.is_null()) {
            Some(s) => {
                s.filter = f;
                S_OK
            }
            None => E_INVALIDARG,
        }
    })
}

extern "win64" fn get_output_filter(this: *mut Voice, to: *mut Voice, out: *mut Filter) {
    let f = with(this, DEFAULT_FILTER, |v| {
        v.sends.iter().find(|s| s.to == to as usize || to.is_null()).map_or(DEFAULT_FILTER, |s| s.filter)
    });
    put(out, f);
}

extern "win64" fn set_volume(this: *mut Voice, volume: f32, _operation_set: u32) -> HRESULT {
    if !volume.is_finite() || volume.abs() > 16_777_216.0 {
        return E_INVALIDARG;
    }
    with(this, XAUDIO2_E_INVALID_CALL, |v| {
        v.volume = volume;
        S_OK
    })
}

extern "win64" fn get_volume(this: *mut Voice, out: *mut f32) {
    put(out, with(this, 0.0, |v| v.volume));
}

extern "win64" fn set_channel_volumes(
    this: *mut Voice,
    channels: u32,
    volumes: *const f32,
    _operation_set: u32,
) -> HRESULT {
    if volumes.is_null() {
        return E_POINTER;
    }
    with(this, XAUDIO2_E_INVALID_CALL, |v| {
        if channels as usize != v.channels {
            return E_INVALIDARG;
        }
        // SAFETY: `channels` floats from the title.
        let new = unsafe { std::slice::from_raw_parts(volumes, v.channels) };
        v.channel_volumes.copy_from_slice(new);
        S_OK
    })
}

extern "win64" fn get_channel_volumes(this: *mut Voice, channels: u32, out: *mut f32) {
    if out.is_null() {
        return;
    }
    with(this, (), |v| {
        let n = v.channels.min(channels as usize);
        // SAFETY: room for `channels` floats.
        unsafe { std::ptr::copy_nonoverlapping(v.channel_volumes.as_ptr(), out, n) };
    });
}

extern "win64" fn set_output_matrix(
    this: *mut Voice,
    to: *mut Voice,
    src: u32,
    dst: u32,
    matrix: *const f32,
    _operation_set: u32,
) -> HRESULT {
    if matrix.is_null() {
        return E_POINTER;
    }
    // SAFETY: as in `with`.
    let shared = unsafe { &(*this).shared };
    let mut mix = lock(&shared.mix);
    let dest_channels = if to.is_null() {
        None
    } else {
        mix.voices.get(&(to as usize)).map(|v| v.channels)
    };
    let Some(v) = mix.voices.get_mut(&(this as usize)) else {
        return XAUDIO2_E_INVALID_CALL;
    };
    // A null destination means the voice's only send.
    let send = match to.is_null() {
        true if v.sends.len() == 1 => v.sends.first_mut(),
        true => None,
        false => v.sends.iter_mut().find(|s| s.to == to as usize),
    };
    let Some(send) = send else {
        return E_INVALIDARG;
    };
    if src as usize != v.channels || dest_channels.is_some_and(|d| d != dst as usize) {
        return E_INVALIDARG;
    }
    // SAFETY: `src * dst` floats from the title.
    let m = unsafe { std::slice::from_raw_parts(matrix, (src * dst) as usize) };
    send.matrix = m.to_vec();
    S_OK
}

extern "win64" fn get_output_matrix(this: *mut Voice, to: *mut Voice, src: u32, dst: u32, out: *mut f32) {
    if out.is_null() {
        return;
    }
    with(this, (), |v| {
        let send = v.sends.iter().find(|s| s.to == to as usize || (to.is_null() && v.sends.len() == 1));
        if let Some(s) = send.filter(|s| s.matrix.len() == (src * dst) as usize) {
            // SAFETY: room for `src * dst` floats.
            unsafe { std::ptr::copy_nonoverlapping(s.matrix.as_ptr(), out, s.matrix.len()) };
        }
    });
}

extern "win64" fn destroy_voice(this: *mut Voice) {
    if this.is_null() {
        return;
    }
    // SAFETY: a voice header the title got from us.
    let shared = unsafe { (*this).shared.clone() };
    let _between = shared.between_passes();
    {
        let mut mix = lock(&shared.mix);
        let id = this as usize;
        // A voice others still send to stays, as in XAudio2.
        if mix.voices.values().any(|v| v.sends.iter().any(|s| s.to == id)) {
            tracing::warn!("DestroyVoice: other voices still send to this one");
            return;
        }
        if mix.voices.remove(&id).is_none() {
            return;
        }
        if mix.master == Some(id) {
            mix.master = None;
        }
    }
    // SAFETY: removed from the mixer, so nothing else reaches it.
    drop(unsafe { Box::from_raw(this) });
}

// ---- source voices ----

extern "win64" fn start(this: *mut Voice, _flags: u32, _operation_set: u32) -> HRESULT {
    with_source(this, XAUDIO2_E_INVALID_CALL, |s| {
        s.started = true;
        S_OK
    })
}

extern "win64" fn stop(this: *mut Voice, _flags: u32, _operation_set: u32) -> HRESULT {
    with_source(this, XAUDIO2_E_INVALID_CALL, |s| {
        s.started = false;
        S_OK
    })
}

extern "win64" fn submit_buffer(this: *mut Voice, buffer: *const Buffer, _wma: *const c_void) -> HRESULT {
    if buffer.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    let b = unsafe { *buffer };
    with_source(this, XAUDIO2_E_INVALID_CALL, |s| {
        let frames = b.audio_bytes / s.block as u32;
        let end = if b.play_length == 0 { frames } else { b.play_begin.saturating_add(b.play_length) };
        let loop_end = if b.loop_length == 0 { end } else { b.loop_begin.saturating_add(b.loop_length) };
        let bad = b.audio_data.is_null()
            || b.audio_bytes == 0
            || !(b.audio_bytes as usize).is_multiple_of(s.block)
            || b.play_begin >= end
            || end > frames
            || (b.loop_count > 0 && (b.loop_begin >= loop_end || loop_end > end || b.loop_begin < b.play_begin))
            || (b.loop_count > XAUDIO2_LOOP_INFINITE);
        if bad {
            return XAUDIO2_E_INVALID_CALL;
        }
        if s.queue.len() >= XAUDIO2_MAX_QUEUED_BUFFERS {
            return XAUDIO2_E_INVALID_CALL;
        }
        s.queue.push_back(Queued {
            buffer: b,
            pos: b.play_begin,
            end,
            loop_begin: b.loop_begin,
            loop_end,
            loops: b.loop_count,
            begun: false,
        });
        S_OK
    })
}

extern "win64" fn flush_buffers(this: *mut Voice) -> HRESULT {
    with_source(this, XAUDIO2_E_INVALID_CALL, |s| {
        // A buffer that is playing stays, if the voice is running.
        let keep = usize::from(s.started && s.queue.front().is_some_and(|q| q.begun));
        let flushed: Vec<Queued> = s.queue.drain(keep..).collect();
        for q in flushed {
            s.pending.push(Event::BufferEnd(q.buffer.context as usize));
        }
        S_OK
    })
}

extern "win64" fn discontinuity(this: *mut Voice) -> HRESULT {
    with_source(this, XAUDIO2_E_INVALID_CALL, |s| {
        if let Some(q) = s.queue.back_mut() {
            q.buffer.flags |= XAUDIO2_END_OF_STREAM;
        }
        S_OK
    })
}

extern "win64" fn exit_loop(this: *mut Voice, _operation_set: u32) -> HRESULT {
    with_source(this, XAUDIO2_E_INVALID_CALL, |s| {
        if let Some(q) = s.queue.front_mut() {
            q.loops = 0;
        }
        S_OK
    })
}

extern "win64" fn get_state(this: *mut Voice, out: *mut VoiceStateOut, flags: u32) {
    let state = with_source(this, None, |s| {
        Some(VoiceStateOut {
            current_context: s.queue.front().map_or(std::ptr::null_mut(), |q| q.buffer.context),
            buffers_queued: s.queue.len() as u32,
            samples_played: if flags & XAUDIO2_VOICE_NOSAMPLESPLAYED != 0 { 0 } else { s.played },
        })
    });
    if let Some(state) = state {
        put(out, state);
    }
}

extern "win64" fn set_frequency_ratio(this: *mut Voice, ratio: f32, _operation_set: u32) -> HRESULT {
    with_source(this, XAUDIO2_E_INVALID_CALL, |s| {
        s.ratio = ratio.clamp(XAUDIO2_MIN_FREQ_RATIO, s.max_ratio);
        S_OK
    })
}

extern "win64" fn get_frequency_ratio(this: *mut Voice, out: *mut f32) {
    put(out, with_source(this, 1.0, |s| s.ratio));
}

extern "win64" fn set_source_rate(this: *mut Voice, rate: u32) -> HRESULT {
    if !(1000..=200_000).contains(&rate) {
        return XAUDIO2_E_INVALID_CALL;
    }
    with(this, XAUDIO2_E_INVALID_CALL, |v| match &v.kind {
        Kind::Source(s) if s.queue.is_empty() => {
            v.rate = rate;
            S_OK
        }
        _ => XAUDIO2_E_INVALID_CALL,
    })
}

extern "win64" fn master_channel_mask(this: *mut Voice, out: *mut u32) -> HRESULT {
    let mask = with(this, None, |v| match v.kind {
        Kind::Master { mask } => Some(mask),
        _ => None,
    });
    match mask {
        Some(m) if put(out, m) => S_OK,
        Some(_) => E_POINTER,
        None => XAUDIO2_E_INVALID_CALL,
    }
}

macro_rules! voice_vtbl {
    ($($extra:expr),* $(,)?) => {
        Vtbl([
            voice_details as *const (),
            set_output_voices as *const (),
            set_effect_chain as *const (),
            enable_effect as *const (),
            disable_effect as *const (),
            effect_state as *const (),
            set_effect_parameters as *const (),
            get_effect_parameters as *const (),
            set_filter as *const (),
            get_filter as *const (),
            set_output_filter as *const (),
            get_output_filter as *const (),
            set_volume as *const (),
            get_volume as *const (),
            set_channel_volumes as *const (),
            get_channel_volumes as *const (),
            set_output_matrix as *const (),
            get_output_matrix as *const (),
            destroy_voice as *const (),
            $($extra as *const ()),*
        ])
    };
}

static SUBMIX_VOICE: Vtbl<19> = voice_vtbl!();
static MASTER_VOICE: Vtbl<20> = voice_vtbl!(master_channel_mask);
static SOURCE_VOICE: Vtbl<29> = voice_vtbl!(
    start,
    stop,
    submit_buffer,
    flush_buffers,
    discontinuity,
    exit_loop,
    get_state,
    set_frequency_ratio,
    get_frequency_ratio,
    set_source_rate,
);

// ---- the pass ----

impl Source {
    /// One frame at the read position, then moves on; `None` when starved.
    fn pull(&mut self, events: &mut Vec<Event>) -> Option<()> {
        let q = self.queue.front_mut()?;
        let context = q.buffer.context as usize;
        if !q.begun {
            q.begun = true;
            events.push(Event::BufferStart(context));
        }
        let at = q.pos as usize * self.block;
        let width = match self.format {
            Sample::U8 => 1,
            Sample::I16 => 2,
            Sample::I24 => 3,
            Sample::I32 | Sample::F32 => 4,
        };
        for (c, out) in self.next.iter_mut().enumerate() {
            // SAFETY: within the title's buffer (checked at submission),
            // which it keeps until the buffer ends.
            *out = unsafe {
                let p = q.buffer.audio_data.add(at + c * width);
                match self.format {
                    Sample::U8 => (f32::from(*p) - 128.0) / 128.0,
                    Sample::I16 => f32::from(p.cast::<i16>().read_unaligned()) / 32_768.0,
                    Sample::I24 => {
                        let v = i32::from_le_bytes([0, *p, *p.add(1), *p.add(2)]) >> 8;
                        v as f32 / 8_388_608.0
                    }
                    Sample::I32 => p.cast::<i32>().read_unaligned() as f32 / 2_147_483_648.0,
                    Sample::F32 => p.cast::<f32>().read_unaligned(),
                }
            };
        }
        self.played += 1;
        q.pos += 1;
        if q.loops > 0 && q.pos == q.loop_end {
            q.pos = q.loop_begin;
            if q.loops != XAUDIO2_LOOP_INFINITE {
                q.loops -= 1;
            }
            events.push(Event::LoopEnd(context));
        } else if q.pos == q.end {
            let end_of_stream = q.buffer.flags & XAUDIO2_END_OF_STREAM != 0;
            self.queue.pop_front();
            events.push(Event::BufferEnd(context));
            if end_of_stream {
                events.push(Event::StreamEnd);
            }
        }
        Some(())
    }

    /// `frames` output frames, `step` input frames apart, linearly
    /// interpolated.
    fn render(&mut self, frames: usize, step: f64, out: &mut [f32], events: &mut Vec<Event>) {
        let channels = self.next.len();
        for f in 0..frames {
            while self.frac >= 1.0 {
                std::mem::swap(&mut self.prev, &mut self.next);
                if self.pull(events).is_none() {
                    self.next.fill(0.0);
                }
                self.frac -= 1.0;
            }
            let t = self.frac as f32;
            for c in 0..channels {
                out[f * channels + c] = self.prev[c] + (self.next[c] - self.prev[c]) * t;
            }
            self.frac += step;
        }
    }

    /// Bytes the title should add so this pass does not starve.
    fn bytes_required(&self, frames: usize, step: f64) -> u32 {
        let need = (frames as f64 * step).ceil() as u64;
        let have: u64 = self.queue.iter().map(|q| u64::from(q.end - q.pos)).sum();
        (need.saturating_sub(have) * self.block as u64) as u32
    }
}

/// Mixes `out` (`src` channels) into `into` (`dst` channels) through `m`.
fn mix_into(out: &[f32], src: usize, into: &mut [f32], dst: usize, m: &[f32]) {
    for (frame, target) in out.chunks_exact(src).zip(into.chunks_exact_mut(dst)) {
        for (d, t) in target.iter_mut().enumerate() {
            *t += (0..src).map(|s| frame[s] * m[d * src + s]).sum::<f32>();
        }
    }
}

/// What the engine thread does for one pass, between taking the title's
/// buffers and handing over the master output.
struct Pass {
    /// `(voice callback, events)` to fire after rendering.
    events: Vec<(usize, Vec<Event>)>,
    /// Stereo frames.
    out: Vec<f32>,
    rate: u32,
}

impl Mix {
    /// Voices with callbacks and the bytes each needs this pass.
    fn pass_starts(&self) -> Vec<(usize, u32)> {
        let frames = mixer_frames(self);
        let rate = f64::from(frames as u32 * 100);
        self.voices
            .values()
            .filter_map(|v| match &v.kind {
                Kind::Source(s) if s.started && s.callback != 0 => {
                    let step = f64::from(v.rate) * f64::from(s.ratio) / rate;
                    Some((s.callback, s.bytes_required(frames, step)))
                }
                _ => None,
            })
            .collect()
    }

    fn render(&mut self) -> Option<Pass> {
        let master = self.master?;
        let frames = mixer_frames(self);
        let rate = (frames * 100) as u32;
        for v in self.voices.values_mut() {
            v.input.clear();
            v.input.resize(frames * v.channels, 0.0);
        }
        // Sources first, then submixes by stage, the master last.
        let mut order: Vec<(u32, usize)> = self
            .voices
            .iter()
            .filter(|(id, _)| **id != master)
            .map(|(&id, v)| (if matches!(v.kind, Kind::Source(_)) { 0 } else { 1 + v.stage }, id))
            .collect();
        order.sort_unstable();
        let mut events = Vec::new();
        let mut out = Vec::new();
        for (_, id) in order {
            let Some(v) = self.voices.get_mut(&id) else { continue };
            out.clear();
            out.resize(frames * v.channels, 0.0);
            match &mut v.kind {
                Kind::Source(s) => {
                    let mut fired = std::mem::take(&mut s.pending);
                    if s.started {
                        let step = f64::from(v.rate) * f64::from(s.ratio) / f64::from(rate);
                        s.render(frames, step, &mut out, &mut fired);
                    }
                    if !fired.is_empty() && s.callback != 0 {
                        events.push((s.callback, fired));
                    }
                    if !s.started {
                        continue;
                    }
                }
                _ => out.copy_from_slice(&v.input),
            }
            let channels = v.channels;
            for (f, x) in out.iter_mut().enumerate() {
                *x *= v.volume * v.channel_volumes[f % channels];
            }
            let sends: Vec<(usize, Vec<f32>)> = v.sends.iter().map(|s| (s.to, s.matrix.clone())).collect();
            for (to, m) in sends {
                if let Some(dest) = self.voices.get_mut(&to)
                    && m.len() == channels * dest.channels
                {
                    let dst = dest.channels;
                    mix_into(&out, channels, &mut dest.input, dst, &m);
                }
            }
        }
        let m = self.voices.get(&master)?;
        let Kind::Master { mask } = m.kind else { return None };
        let speakers: Vec<(f32, f32)> = speaker_bits(mask, m.channels)
            .map(audio_out::stereo_gains)
            .collect();
        let mut stereo = vec![0.0f32; frames * 2];
        for (frame, target) in m.input.chunks_exact(m.channels).zip(stereo.as_chunks_mut::<2>().0.iter_mut()) {
            for (c, &x) in frame.iter().enumerate() {
                let (l, r) = speakers[c];
                let x = x * m.volume * m.channel_volumes[c];
                target[0] += x * l;
                target[1] += x * r;
            }
        }
        Some(Pass {
            events,
            out: stereo,
            rate,
        })
    }
}

/// The speaker of each channel of a mask; channels past the mask count
/// as centred.
fn speaker_bits(mask: u32, channels: usize) -> impl Iterator<Item = u32> {
    (0..32)
        .map(|b| 1u32 << b)
        .filter(move |b| mask & b != 0)
        .chain(std::iter::repeat(0))
        .take(channels)
}

/// Calls slot `n` of a title callback object (no `IUnknown`).
unsafe fn callback<F: Copy>(object: usize, n: usize) -> F {
    // SAFETY: the caller names a slot of IXAudio2VoiceCallback or
    // IXAudio2EngineCallback holding a function of type F.
    unsafe {
        let vtbl = *(object as *const *const *const ());
        std::mem::transmute_copy(&*vtbl.add(n))
    }
}

fn fire(cb: usize, event: Event) {
    type Ctx = extern "win64" fn(usize, usize);
    // SAFETY: slots 2 to 5 of IXAudio2VoiceCallback.
    unsafe {
        match event {
            Event::StreamEnd => callback::<extern "win64" fn(usize)>(cb, 2)(cb),
            Event::BufferStart(c) => callback::<Ctx>(cb, 3)(cb, c),
            Event::BufferEnd(c) => callback::<Ctx>(cb, 4)(cb, c),
            Event::LoopEnd(c) => callback::<Ctx>(cb, 5)(cb, c),
        }
    }
}

/// One pass; `None` with no mastering voice.
fn run_pass(shared: &Shared) -> Option<(Vec<f32>, u32)> {
    let _pass = lock(&shared.pass);
    let (engine_callbacks, starts) = {
        let mix = lock(&shared.mix);
        mix.master?;
        (mix.callbacks.clone(), mix.pass_starts())
    };
    type Plain = extern "win64" fn(usize);
    for &cb in &engine_callbacks {
        // SAFETY: slot 0 of IXAudio2EngineCallback, OnProcessingPassStart.
        unsafe { callback::<Plain>(cb, 0)(cb) };
    }
    for &(cb, bytes) in &starts {
        // SAFETY: slot 0 of IXAudio2VoiceCallback, OnVoiceProcessingPassStart.
        unsafe { callback::<extern "win64" fn(usize, u32)>(cb, 0)(cb, bytes) };
    }
    let pass = lock(&shared.mix).render()?;
    for (cb, events) in &pass.events {
        events.iter().for_each(|&e| fire(*cb, e));
    }
    for &(cb, _) in &starts {
        // SAFETY: slot 1 of IXAudio2VoiceCallback, OnVoiceProcessingPassEnd.
        unsafe { callback::<Plain>(cb, 1)(cb) };
    }
    for &cb in &engine_callbacks {
        // SAFETY: slot 1 of IXAudio2EngineCallback, OnProcessingPassEnd.
        unsafe { callback::<Plain>(cb, 1)(cb) };
    }
    Some((pass.out, pass.rate))
}

unsafe extern "win64" fn engine_thread(param: *mut c_void) -> u32 {
    // SAFETY: the reference `XAudio2Create` handed this thread.
    let shared = unsafe { Arc::from_raw(param as *const Shared) };
    shared.thread.store(guest_thread::current_id(), Ordering::Release);
    let mut sink: Option<(u32, Box<dyn audio_out::Sink>)> = None;
    let idle = Duration::from_millis(10);
    while !shared.quit.load(Ordering::Acquire) {
        if !shared.running.load(Ordering::Acquire) {
            let g = lock(&shared.wake.0);
            let _ = shared.wake.1.wait_timeout(g, idle);
            continue;
        }
        let Some((frames, rate)) = run_pass(&shared) else {
            std::thread::sleep(idle);
            continue;
        };
        if sink.as_ref().is_none_or(|(r, _)| *r != rate) {
            sink = Some((rate, audio_out::open("XAudio2", rate, 2)));
        }
        if let Some((_, s)) = &mut sink {
            s.write(&frames);
        }
    }
    0
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xaudio2",
        dlls: &["xaudio2_9.dll", "xaudio2_9redist.dll"],
        exports: vec![
            hle_export!(XAudio2Create),
            hle_export!(XAudio2CreateWithVersionInfo),
            HleExport {
                name: "#1",
                address: XAudio2Create as *const () as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn layouts_are_windows() {
        assert_eq!(size_of::<Buffer>(), 48);
        assert_eq!(std::mem::offset_of!(Buffer, context), 40);
        assert_eq!(size_of::<VoiceStateOut>(), 24);
        assert_eq!(size_of::<VoiceDetails>(), 16);
        assert_eq!(size_of::<SendDescriptor>(), 16);
        assert_eq!(size_of::<Filter>(), 12);
    }

    #[test]
    fn default_matrices() {
        assert_eq!(default_matrix(1, 2), [1.0, 1.0]);
        assert_eq!(default_matrix(2, 1), [0.5, 0.5]);
        assert_eq!(default_matrix(2, 2), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(default_matrix(6, 2), [1., 0., 0., 0., 0., 0., 0., 1., 0., 0., 0., 0.]);
    }

    /// A title-side IXAudio2VoiceCallback recording what it is told.
    #[repr(C)]
    struct Recorder {
        vtbl: &'static [usize; 7],
        starts: AtomicUsize,
        ends: Mutex<Vec<usize>>,
        stream_ends: AtomicUsize,
        voice: AtomicUsize,
        submitted: AtomicUsize,
    }

    extern "win64" fn pass_start(this: &Recorder, _bytes: u32) {
        this.starts.fetch_add(1, Ordering::Relaxed);
    }
    extern "win64" fn nothing(_this: &Recorder) {}
    extern "win64" fn stream_end(this: &Recorder) {
        this.stream_ends.fetch_add(1, Ordering::Relaxed);
    }
    extern "win64" fn buffer_nothing(_this: &Recorder, _ctx: usize) {}
    extern "win64" fn buffer_end(this: &Recorder, ctx: usize) {
        lock(&this.ends).push(ctx);
        // Resubmitting from the callback, as streaming titles do.
        if ctx == 1 && this.submitted.fetch_add(1, Ordering::Relaxed) == 0 {
            let b = buffer(3, true);
            let v = this.voice.load(Ordering::Relaxed) as *mut Voice;
            assert_eq!(submit_buffer(v, &b, std::ptr::null()), S_OK);
        }
    }
    extern "win64" fn voice_error(_this: &Recorder, _ctx: usize, _hr: HRESULT) {}

    static SAMPLES: [i16; 960] = [16_384; 960];

    fn buffer(context: usize, end: bool) -> Buffer {
        Buffer {
            flags: if end { XAUDIO2_END_OF_STREAM } else { 0 },
            audio_bytes: (SAMPLES.len() * 2) as u32,
            audio_data: SAMPLES.as_ptr().cast(),
            play_begin: 0,
            play_length: 0,
            loop_begin: 0,
            loop_length: 0,
            loop_count: 0,
            context: context as *mut c_void,
        }
    }

    fn pcm16_mono() -> WaveFormat {
        WaveFormat {
            tag: WAVE_FORMAT_PCM,
            channels: 1,
            rate: 48_000,
            bytes_per_second: 96_000,
            block_align: 2,
            bits: 16,
            extra: 0,
        }
    }

    #[test]
    fn source_voices_play_their_buffers_and_call_back() {
        let vtbl: &'static [usize; 7] = Box::leak(Box::new([
            pass_start as *const () as usize,
            nothing as *const () as usize,
            stream_end as *const () as usize,
            buffer_nothing as *const () as usize,
            buffer_end as *const () as usize,
            buffer_nothing as *const () as usize,
            voice_error as *const () as usize,
        ]));
        let recorder = Box::leak(Box::new(Recorder {
            vtbl,
            starts: AtomicUsize::new(0),
            ends: Mutex::new(Vec::new()),
            stream_ends: AtomicUsize::new(0),
            voice: AtomicUsize::new(0),
            submitted: AtomicUsize::new(0),
        }));

        let mut xa = std::ptr::null_mut();
        assert_eq!(XAudio2Create(&mut xa, 0, 1), S_OK);
        let e = xa as *mut Engine;
        let mut source = std::ptr::null_mut();
        let format = pcm16_mono();
        let cb = recorder as *mut Recorder as *mut c_void;
        // No mastering voice yet: nowhere to send.
        let hr = create_source_voice(e, &mut source, &format, 0, 2.0, cb, std::ptr::null(), std::ptr::null());
        assert_eq!(hr, XAUDIO2_E_INVALID_CALL);

        let mut master = std::ptr::null_mut();
        let hr = create_mastering_voice(e, &mut master, 0, 0, 0, std::ptr::null(), std::ptr::null(), 6);
        assert_eq!(hr, S_OK);
        let mut mask = 0;
        assert_eq!(master_channel_mask(master, &mut mask), S_OK);
        assert_eq!(mask, 3);
        let mut again = std::ptr::null_mut();
        let hr = create_mastering_voice(e, &mut again, 0, 0, 0, std::ptr::null(), std::ptr::null(), 6);
        assert_eq!(hr, XAUDIO2_E_INVALID_CALL);

        let hr = create_source_voice(e, &mut source, &format, 0, 2.0, cb, std::ptr::null(), std::ptr::null());
        assert_eq!(hr, S_OK);
        recorder.voice.store(source as usize, Ordering::Relaxed);
        let mut details = VoiceDetails { creation_flags: 9, active_flags: 9, input_channels: 0, input_rate: 0 };
        voice_details(source, &mut details);
        assert_eq!((details.input_channels, details.input_rate), (1, 48_000));

        assert_eq!(submit_buffer(source, &buffer(1, false), std::ptr::null()), S_OK);
        assert_eq!(submit_buffer(source, &buffer(2, false), std::ptr::null()), S_OK);
        let mut bad = buffer(9, false);
        bad.audio_bytes = 3;
        assert_eq!(submit_buffer(source, &bad, std::ptr::null()), XAUDIO2_E_INVALID_CALL);

        let mut state = VoiceStateOut { current_context: std::ptr::null_mut(), buffers_queued: 0, samples_played: 7 };
        get_state(source, &mut state, 0);
        assert_eq!((state.current_context as usize, state.buffers_queued, state.samples_played), (1, 2, 0));

        assert_eq!(start(source, 0, 0), S_OK);
        // 3 buffers of 10 ms each (one resubmitted from OnBufferEnd).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while recorder.stream_ends.load(Ordering::Relaxed) == 0 {
            assert!(std::time::Instant::now() < deadline, "no OnStreamEnd");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(*lock(&recorder.ends), [1, 2, 3]);
        assert!(recorder.starts.load(Ordering::Relaxed) >= 3);
        get_state(source, &mut state, 0);
        assert_eq!((state.buffers_queued, state.samples_played), (0, 3 * 960));

        // Flushing a stopped voice ends its buffers on the next pass.
        assert_eq!(stop(source, 0, 0), S_OK);
        assert_eq!(submit_buffer(source, &buffer(4, false), std::ptr::null()), S_OK);
        assert_eq!(flush_buffers(source), S_OK);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while lock(&recorder.ends).len() < 4 {
            assert!(std::time::Instant::now() < deadline, "flushed buffer never ended");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(lock(&recorder.ends)[3], 4);

        // The master cannot go while a voice sends to it.
        destroy_voice(master);
        assert!(lock(&engine(e).mix).master.is_some());
        destroy_voice(source);
        destroy_voice(master);
        assert!(lock(&engine(e).mix).voices.is_empty());
        combase::release_unknown(xa);
    }

    #[test]
    fn a_pass_resamples_and_routes_through_a_submix() {
        let shared = Arc::new(Shared {
            mix: Mutex::default(),
            pass: Mutex::new(()),
            running: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            wake: (Mutex::new(()), Condvar::new()),
            thread: AtomicU32::new(0),
        });
        let mut mix = Mix::default();
        let m = new_voice(&shared, &mut mix, std::ptr::null(), base_state(Kind::Master { mask: 3 }, 0, 2, 48_000));
        mix.master = Some(m as usize);
        let mut sub = base_state(Kind::Submix, 0, 1, 48_000);
        sub.volume = 0.5;
        sub.sends = vec![send_to(&mix, 1, m as usize)];
        let sub = new_voice(&shared, &mut mix, std::ptr::null(), sub);
        // A 24 kHz source: each input frame lasts two output frames.
        let source = Source {
            format: Sample::I16,
            block: 2,
            callback: 0,
            started: true,
            ratio: 1.0,
            max_ratio: 2.0,
            queue: VecDeque::new(),
            played: 0,
            prev: vec![0.0],
            next: vec![0.0],
            frac: 1.0,
            pending: Vec::new(),
        };
        let mut src = base_state(Kind::Source(Box::new(source)), 0, 1, 24_000);
        src.sends = vec![send_to(&mix, 1, sub as usize)];
        let src = new_voice(&shared, &mut mix, std::ptr::null(), src);
        if let Some(VoiceState { kind: Kind::Source(s), .. }) = mix.voices.get_mut(&(src as usize)) {
            s.queue.push_back(Queued {
                buffer: buffer(5, false),
                pos: 0,
                end: 960,
                loop_begin: 0,
                loop_end: 960,
                loops: 0,
                begun: false,
            });
        }
        let pass = mix.render().expect("a pass");
        assert_eq!(pass.rate, 48_000);
        assert_eq!(pass.out.len(), 960);
        assert!(pass.events.is_empty()); // no callback
        // Steady state: 0.5 * 0.5 (submix volume), mono to both sides.
        assert_eq!((pass.out[100], pass.out[101]), (0.25, 0.25));
        // 480 output frames at half rate read 240 input frames.
        match &mix.voices[&(src as usize)].kind {
            Kind::Source(s) => assert_eq!(s.played, 240),
            _ => unreachable!(),
        }
        for v in [src, sub, m] {
            mix.voices.remove(&(v as usize));
            // SAFETY: headers from new_voice, removed from the mixer.
            drop(unsafe { Box::from_raw(v) });
        }
    }

    #[test]
    fn unsupported_formats_are_refused() {
        let mut f = pcm16_mono();
        assert!(sample_format(&f).is_some());
        f.tag = 0x166; // XMA2
        assert!(sample_format(&f).is_none());
        f.tag = WAVE_FORMAT_IEEE_FLOAT;
        assert!(sample_format(&f).is_none()); // 16-bit float
        f.bits = 32;
        f.block_align = 4;
        assert_eq!(sample_format(&f).map(|s| s.0), Some(Sample::F32));
    }
}
