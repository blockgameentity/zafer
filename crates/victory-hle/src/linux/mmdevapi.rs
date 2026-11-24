//! The audio device API (`MMDeviceEnumerator`, made by `CoCreateInstance`)
//! with one output device, our [`audio_out`](super::audio_out) stream.
//!
//! Grand Theft Auto VI renders through Spatial Audio: it activates an
//! `ISpatialAudioClient` on the default device, opens a stream of static
//! objects (the speaker channels it wants, any of them) with an event, and
//! fills one 10 ms buffer per object each time the event is set. Our
//! device thread sets the event, waits for the title to finish the pass,
//! downmixes the objects to stereo and plays them. There are no dynamic
//! objects (as with spatial sound off on Windows).
//!
//! `IAudioClient` only answers its mix format: 48 kHz float stereo.
// ponytail: shared-mode IAudioClient streams are not rendered; add them
// when a title plays through one.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use super::audio_out;
use super::combase::{
    self, Com, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER, Guid, HRESULT, PropVariant,
    S_OK, VT_BLOB, VT_EMPTY, VT_LPWSTR, VT_UI4, Vtbl, com_vtbl, guid, hand_out, put, this,
};
use super::handles;

pub(crate) const RATE: u32 = 48_000;
/// Frames per pass: 10 ms.
pub(crate) const PERIOD: usize = 480;

const E_NOTFOUND: HRESULT = 0x8007_0490_u32 as i32;
const AUDCLNT_E_UNSUPPORTED_FORMAT: HRESULT = 0x8889_0008_u32 as i32;
const SPTLAUDCLNT_E_OUT_OF_ORDER: HRESULT = 0x8897_0101_u32 as i32;
const SPTLAUDCLNT_E_RESOURCES_INVALIDATED: HRESULT = 0x8897_0102_u32 as i32;
const SPTLAUDCLNT_E_NO_MORE_OBJECTS: HRESULT = 0x8897_0103_u32 as i32;
const SPTLAUDCLNT_E_PROPERTY_NOT_SUPPORTED: HRESULT = 0x8897_0104_u32 as i32;
const SPTLAUDCLNT_E_STATIC_OBJECT_NOT_AVAILABLE: HRESULT = 0x8897_010B_u32 as i32;
const SPTLAUDCLNT_E_OBJECT_ALREADY_ACTIVE: HRESULT = 0x8897_010C_u32 as i32;
const STG_E_ACCESSDENIED: HRESULT = 0x8003_0005_u32 as i32;

const CLSID_MM_DEVICE_ENUMERATOR: Guid = guid(
    0xBCDE_0395,
    0xE52F,
    0x467C,
    [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
);
const IID_IMM_DEVICE_ENUMERATOR: Guid = guid(
    0xA956_64D2,
    0x9614,
    0x4F35,
    [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
);
const IID_IMM_DEVICE_COLLECTION: Guid = guid(
    0x0BD7_A1BE,
    0x7A1A,
    0x44DB,
    [0x83, 0x97, 0xCC, 0x53, 0x92, 0x38, 0x7B, 0x5E],
);
const IID_IMM_DEVICE: Guid = guid(
    0xD666_063F,
    0x1587,
    0x4E43,
    [0x81, 0xF1, 0xB9, 0x48, 0xE8, 0x07, 0x36, 0x3F],
);
const IID_IPROPERTY_STORE: Guid = guid(
    0x886D_8EEB,
    0x8CF2,
    0x4446,
    [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99],
);
pub(crate) const IID_IAUDIO_CLIENT: Guid = guid(
    0x1CB9_AD4C,
    0xDBFA,
    0x4C32,
    [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
);
pub(crate) const IID_ISPATIAL_AUDIO_CLIENT: Guid = guid(
    0xBBF8_E066,
    0xAAAA,
    0x49BE,
    [0x9A, 0x4D, 0xFD, 0x2A, 0x85, 0x8E, 0xA2, 0x7F],
);
const IID_IAUDIO_FORMAT_ENUMERATOR: Guid = guid(
    0xDCDA_A858,
    0x895A,
    0x4A22,
    [0xA5, 0xEB, 0x67, 0xBD, 0xA5, 0x06, 0x09, 0x6D],
);
pub(crate) const IID_ISPATIAL_AUDIO_OBJECT_RENDER_STREAM: Guid = guid(
    0xBAB5_F473,
    0xB423,
    0x477B,
    [0x85, 0xF5, 0xB5, 0xA3, 0x32, 0xA0, 0x41, 0x53],
);
const IID_ISPATIAL_AUDIO_OBJECT_RENDER_STREAM_BASE: Guid = guid(
    0xFEAA_F403,
    0xC1D8,
    0x450D,
    [0xAA, 0x05, 0xE0, 0xCC, 0xEE, 0x75, 0x02, 0xA8],
);
const IID_ISPATIAL_AUDIO_OBJECT: Guid = guid(
    0xDDE2_8967,
    0x521B,
    0x46E5,
    [0x8F, 0x00, 0xBD, 0x6F, 0x2B, 0xC8, 0xAB, 0x1D],
);
const IID_ISPATIAL_AUDIO_OBJECT_BASE: Guid = guid(
    0xCCE0_B8F2,
    0x8D4D,
    0x4EFB,
    [0xA8, 0xCF, 0x3D, 0x6E, 0xCF, 0x1C, 0x30, 0xE0],
);

/// `PKEY_Device_FriendlyName`.
const PKEY_FRIENDLY_NAME: PropertyKey = PropertyKey(
    guid(
        0xA45C_254E,
        0xDF1C,
        0x4EFD,
        [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
    ),
    14,
);
const AUDIO_ENDPOINT: Guid = guid(
    0x1DA5_D803,
    0xD492,
    0x4EDD,
    [0x8C, 0x23, 0xE0, 0xC0, 0xFF, 0xEE, 0x7F, 0x0E],
);
/// `PKEY_AudioEndpoint_FormFactor`: speakers (1).
const PKEY_FORM_FACTOR: PropertyKey = PropertyKey(AUDIO_ENDPOINT, 0);
/// `PKEY_AudioEndpoint_PhysicalSpeakers`: front left and right.
const PKEY_PHYSICAL_SPEAKERS: PropertyKey = PropertyKey(AUDIO_ENDPOINT, 3);

const DEVICE_ID: &str = "{0.0.0.00000000}.{5649d5a0-0000-4000-8000-766963746f72}";
const DEVICE_NAME: &str = "Speakers (VIctory)";

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
struct PropertyKey(Guid, u32);

/// `CoCreateInstance` for the classes of this file.
pub(crate) fn create(clsid: &Guid, iid: *const Guid, out: *mut *mut c_void) -> Option<HRESULT> {
    (*clsid == CLSID_MM_DEVICE_ENUMERATOR).then(|| {
        let e = combase::new(Enumerator {
            vtbl: &ENUMERATOR,
            refs: AtomicU32::new(0),
        });
        hand_out(e, iid, out)
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- the enumerator, its collection and the device ----

#[repr(C)]
struct Enumerator {
    vtbl: &'static Vtbl<8>,
    refs: AtomicU32,
}

impl Com for Enumerator {
    const IIDS: &'static [Guid] = &[IID_IMM_DEVICE_ENUMERATOR];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

static ENUMERATOR: Vtbl<8> = com_vtbl!(Enumerator;
    enum_endpoints, default_endpoint, get_device, register_notifications, unregister_notifications);

const E_RENDER: u32 = 0;
const E_ALL: u32 = 2;
const DEVICE_STATE_ACTIVE: u32 = 1;

fn device(iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    let d = combase::new(Device {
        vtbl: &DEVICE,
        refs: AtomicU32::new(0),
    });
    hand_out(d, iid, out)
}

extern "win64" fn enum_endpoints(
    _this: *mut Enumerator,
    flow: u32,
    mask: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let count = u32::from((flow == E_RENDER || flow == E_ALL) && mask & DEVICE_STATE_ACTIVE != 0);
    let c = combase::new(Collection {
        vtbl: &COLLECTION,
        refs: AtomicU32::new(0),
        count,
    });
    hand_out(c, &IID_IMM_DEVICE_COLLECTION, out)
}

extern "win64" fn default_endpoint(
    _this: *mut Enumerator,
    flow: u32,
    _role: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    if flow != E_RENDER {
        return E_NOTFOUND;
    }
    device(&IID_IMM_DEVICE, out)
}

extern "win64" fn get_device(_this: *mut Enumerator, id: *const u16, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || id.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    // SAFETY: a NUL-terminated id from the title.
    let len = (0..).take_while(|&i| unsafe { *id.add(i) } != 0).count();
    // SAFETY: `len` units just read.
    let id = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(id, len) });
    if !id.eq_ignore_ascii_case(DEVICE_ID) {
        return E_NOTFOUND;
    }
    device(&IID_IMM_DEVICE, out)
}

/// The one device never changes, so notifications never come.
extern "win64" fn register_notifications(_this: *mut Enumerator, _client: *mut c_void) -> HRESULT {
    S_OK
}

extern "win64" fn unregister_notifications(_this: *mut Enumerator, _client: *mut c_void) -> HRESULT {
    S_OK
}

#[repr(C)]
struct Collection {
    vtbl: &'static Vtbl<5>,
    refs: AtomicU32,
    count: u32,
}

impl Com for Collection {
    const IIDS: &'static [Guid] = &[IID_IMM_DEVICE_COLLECTION];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

static COLLECTION: Vtbl<5> = com_vtbl!(Collection; collection_count, collection_item);

extern "win64" fn collection_count(this: *mut Collection, out: *mut u32) -> HRESULT {
    if put(out, self::this(this).count) { S_OK } else { E_POINTER }
}

extern "win64" fn collection_item(this: *mut Collection, index: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    if index >= self::this(this).count {
        return E_INVALIDARG;
    }
    device(&IID_IMM_DEVICE, out)
}

#[repr(C)]
struct Device {
    vtbl: &'static Vtbl<7>,
    refs: AtomicU32,
}

impl Com for Device {
    const IIDS: &'static [Guid] = &[IID_IMM_DEVICE];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

static DEVICE: Vtbl<7> = com_vtbl!(Device; activate, open_properties, device_id, device_state);

extern "win64" fn activate(
    _this: *mut Device,
    iid: *const Guid,
    _context: u32,
    _params: *const PropVariant,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    // SAFETY: checked non-null.
    match unsafe { *iid } {
        IID_IAUDIO_CLIENT => {
            let c = combase::new(AudioClient {
                vtbl: &AUDIO_CLIENT,
                refs: AtomicU32::new(0),
            });
            hand_out(c, iid, out)
        }
        IID_ISPATIAL_AUDIO_CLIENT => {
            let c = combase::new(SpatialClient {
                vtbl: &SPATIAL_CLIENT,
                refs: AtomicU32::new(0),
            });
            hand_out(c, iid, out)
        }
        other => {
            tracing::debug!(iid = %combase::guid_text(&other), "IMMDevice::Activate: no such interface");
            E_NOINTERFACE
        }
    }
}

extern "win64" fn open_properties(_this: *mut Device, _access: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let p = combase::new(Properties {
        vtbl: &PROPERTIES,
        refs: AtomicU32::new(0),
    });
    hand_out(p, &IID_IPROPERTY_STORE, out)
}

extern "win64" fn device_id(_this: *mut Device, out: *mut *mut u16) -> HRESULT {
    if put(out, combase::task_string(DEVICE_ID)) { S_OK } else { E_POINTER }
}

extern "win64" fn device_state(_this: *mut Device, out: *mut u32) -> HRESULT {
    if put(out, DEVICE_STATE_ACTIVE) { S_OK } else { E_POINTER }
}

#[repr(C)]
struct Properties {
    vtbl: &'static Vtbl<8>,
    refs: AtomicU32,
}

impl Com for Properties {
    const IIDS: &'static [Guid] = &[IID_IPROPERTY_STORE];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

static PROPERTIES: Vtbl<8> = com_vtbl!(Properties;
    property_count, property_at, property_value, property_set, property_commit);

const KEYS: [PropertyKey; 3] = [PKEY_FRIENDLY_NAME, PKEY_FORM_FACTOR, PKEY_PHYSICAL_SPEAKERS];

extern "win64" fn property_count(_this: *mut Properties, out: *mut u32) -> HRESULT {
    if put(out, KEYS.len() as u32) { S_OK } else { E_POINTER }
}

extern "win64" fn property_at(_this: *mut Properties, index: u32, out: *mut PropertyKey) -> HRESULT {
    match KEYS.get(index as usize) {
        Some(&key) if put(out, key) => S_OK,
        Some(_) => E_POINTER,
        None => E_INVALIDARG,
    }
}

extern "win64" fn property_value(
    _this: *mut Properties,
    key: *const PropertyKey,
    out: *mut PropVariant,
) -> HRESULT {
    if key.is_null() || out.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null.
    let key = unsafe { *key };
    let (vt, value) = match key {
        PKEY_FRIENDLY_NAME => (VT_LPWSTR, combase::task_string(DEVICE_NAME) as u64),
        PKEY_FORM_FACTOR => (VT_UI4, 1),
        PKEY_PHYSICAL_SPEAKERS => (VT_UI4, 3),
        _ => (VT_EMPTY, 0),
    };
    put(
        out,
        PropVariant {
            vt,
            reserved: [0; 3],
            value: [value, 0],
        },
    );
    S_OK
}

extern "win64" fn property_set(
    _this: *mut Properties,
    _key: *const PropertyKey,
    _value: *const PropVariant,
) -> HRESULT {
    STG_E_ACCESSDENIED
}

extern "win64" fn property_commit(_this: *mut Properties) -> HRESULT {
    STG_E_ACCESSDENIED
}

// ---- formats ----

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
pub(crate) const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: Guid = guid(
    0x0000_0003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `WAVEFORMATEX`.
#[repr(C, packed(2))]
#[derive(Clone, Copy)]
pub(crate) struct WaveFormat {
    pub(crate) tag: u16,
    pub(crate) channels: u16,
    pub(crate) rate: u32,
    pub(crate) bytes_per_second: u32,
    pub(crate) block_align: u16,
    pub(crate) bits: u16,
    pub(crate) extra: u16,
}

/// `WAVEFORMATEXTENSIBLE`.
#[repr(C, packed(2))]
#[derive(Clone, Copy)]
pub(crate) struct WaveFormatExtensible {
    pub(crate) format: WaveFormat,
    pub(crate) valid_bits: u16,
    pub(crate) channel_mask: u32,
    pub(crate) sub_format: Guid,
}

const fn float_format(channels: u16) -> WaveFormat {
    WaveFormat {
        tag: WAVE_FORMAT_IEEE_FLOAT,
        channels,
        rate: RATE,
        bytes_per_second: RATE * 4 * channels as u32,
        block_align: 4 * channels,
        bits: 32,
        extra: 0,
    }
}

/// The format of every audio object: mono float at 48 kHz.
static OBJECT_FORMAT: WaveFormat = float_format(1);

/// Whether `format` is [`OBJECT_FORMAT`].
fn is_object_format(format: *const WaveFormat) -> bool {
    if format.is_null() {
        return false;
    }
    // SAFETY: a WAVEFORMATEX from the title; the extensible part is read
    // only when its tag says it is there.
    unsafe {
        let f = format.read_unaligned();
        let float = match f.tag {
            WAVE_FORMAT_IEEE_FLOAT => true,
            WAVE_FORMAT_EXTENSIBLE if f.extra >= 22 => {
                (format as *const WaveFormatExtensible).read_unaligned().sub_format
                    == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            }
            _ => false,
        };
        float && f.channels == 1 && f.rate == RATE && f.bits == 32
    }
}

// ---- IAudioClient ----

#[repr(C)]
struct AudioClient {
    vtbl: &'static Vtbl<15>,
    refs: AtomicU32,
}

impl Com for AudioClient {
    const IIDS: &'static [Guid] = &[IID_IAUDIO_CLIENT];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

extern "win64" fn not_implemented() -> HRESULT {
    E_NOTIMPL
}

static AUDIO_CLIENT: Vtbl<15> = com_vtbl!(AudioClient;
    not_implemented, // Initialize
    not_implemented, // GetBufferSize
    not_implemented, // GetStreamLatency
    not_implemented, // GetCurrentPadding
    not_implemented, // IsFormatSupported
    mix_format,
    device_period,
    not_implemented, // Start
    not_implemented, // Stop
    not_implemented, // Reset
    not_implemented, // SetEventHandle
    not_implemented, // GetService
);

/// The device's format, in task memory: float stereo at 48 kHz.
extern "win64" fn mix_format(_this: *mut AudioClient, out: *mut *mut WaveFormatExtensible) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let f = WaveFormatExtensible {
        format: WaveFormat {
            tag: WAVE_FORMAT_EXTENSIBLE,
            extra: 22,
            ..float_format(2)
        },
        valid_bits: 32,
        channel_mask: 3,
        sub_format: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    };
    let p = combase::CoTaskMemAlloc(size_of::<WaveFormatExtensible>()) as *mut WaveFormatExtensible;
    if p.is_null() {
        return combase::E_OUTOFMEMORY;
    }
    // SAFETY: a fresh block of the right size.
    unsafe { p.write_unaligned(f) };
    put(out, p);
    S_OK
}

/// 10 ms, in 100 ns units, for both the default and the minimum.
extern "win64" fn device_period(_this: *mut AudioClient, default: *mut i64, minimum: *mut i64) -> HRESULT {
    put(default, 100_000);
    put(minimum, 100_000);
    S_OK
}

// ---- ISpatialAudioClient ----

#[repr(C)]
struct SpatialClient {
    vtbl: &'static Vtbl<11>,
    refs: AtomicU32,
}

impl Com for SpatialClient {
    const IIDS: &'static [Guid] = &[IID_ISPATIAL_AUDIO_CLIENT];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

static SPATIAL_CLIENT: Vtbl<11> = com_vtbl!(SpatialClient;
    static_position, native_mask, max_dynamic, formats, max_frames, format_supported,
    stream_available, activate_stream);

/// `AudioObjectType` bits of the static channels, `FrontLeft` (2) to
/// `BackCenter` (0x20000); bit 0 is `Dynamic`.
const STATIC_TYPES: u32 = 0x3_FFFE;
const FRONT_LEFT: u32 = 2;
const FRONT_RIGHT: u32 = 4;

extern "win64" fn static_position(
    _this: *mut SpatialClient,
    _kind: u32,
    x: *mut f32,
    y: *mut f32,
    z: *mut f32,
) -> HRESULT {
    put(x, 0.0);
    put(y, 0.0);
    put(z, 0.0);
    S_OK
}

extern "win64" fn native_mask(_this: *mut SpatialClient, out: *mut u32) -> HRESULT {
    if put(out, FRONT_LEFT | FRONT_RIGHT) { S_OK } else { E_POINTER }
}

extern "win64" fn max_dynamic(_this: *mut SpatialClient, out: *mut u32) -> HRESULT {
    if put(out, 0) { S_OK } else { E_POINTER }
}

extern "win64" fn formats(_this: *mut SpatialClient, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let f = combase::new(Formats {
        vtbl: &FORMATS,
        refs: AtomicU32::new(0),
    });
    hand_out(f, &IID_IAUDIO_FORMAT_ENUMERATOR, out)
}

extern "win64" fn max_frames(_this: *mut SpatialClient, format: *const WaveFormat, out: *mut u32) -> HRESULT {
    if !is_object_format(format) {
        return AUDCLNT_E_UNSUPPORTED_FORMAT;
    }
    if put(out, PERIOD as u32) { S_OK } else { E_POINTER }
}

extern "win64" fn format_supported(_this: *mut SpatialClient, format: *const WaveFormat) -> HRESULT {
    if is_object_format(format) { S_OK } else { AUDCLNT_E_UNSUPPORTED_FORMAT }
}

extern "win64" fn stream_available(
    _this: *mut SpatialClient,
    _iid: *const Guid,
    _params: *const PropVariant,
) -> HRESULT {
    S_OK
}

/// `SpatialAudioObjectRenderStreamActivationParams`.
#[repr(C)]
struct ActivationParams {
    format: *const WaveFormat,
    static_mask: u32,
    min_dynamic: u32,
    max_dynamic: u32,
    category: u32,
    event: usize,
    notify: *mut c_void,
}

extern "win64" fn activate_stream(
    _this: *mut SpatialClient,
    params: *const PropVariant,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || params.is_null() || iid.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    // SAFETY: checked non-null; a BLOB PROPVARIANT holds its size in the
    // low half of its first word and its data pointer in the second.
    let p = unsafe {
        let pv = &*params;
        let size = pv.value[0] as u32 as usize;
        let data = pv.value[1] as *const ActivationParams;
        if pv.vt != VT_BLOB || size < size_of::<ActivationParams>() || data.is_null() {
            return E_INVALIDARG;
        }
        data.read_unaligned()
    };
    if !is_object_format(p.format) {
        return AUDCLNT_E_UNSUPPORTED_FORMAT;
    }
    let s = Stream::new(p.static_mask & STATIC_TYPES, p.event);
    tracing::info!(mask = format_args!("{:#x}", p.static_mask), "spatial audio stream");
    hand_out(s, iid, out)
}

#[repr(C)]
struct Formats {
    vtbl: &'static Vtbl<5>,
    refs: AtomicU32,
}

impl Com for Formats {
    const IIDS: &'static [Guid] = &[IID_IAUDIO_FORMAT_ENUMERATOR];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

static FORMATS: Vtbl<5> = com_vtbl!(Formats; format_count, format_at);

extern "win64" fn format_count(_this: *mut Formats, out: *mut u32) -> HRESULT {
    if put(out, 1) { S_OK } else { E_POINTER }
}

/// The format stays ours (and alive for good), as on Windows.
extern "win64" fn format_at(_this: *mut Formats, index: u32, out: *mut *const WaveFormat) -> HRESULT {
    if index != 0 {
        return E_INVALIDARG;
    }
    if put(out, &OBJECT_FORMAT) { S_OK } else { E_POINTER }
}

// ---- the stream of static objects ----

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    running: bool,
    quit: bool,
    updating: bool,
    /// A pass the title finished: stereo frames.
    done: Option<Vec<f32>>,
    objects: Vec<ObjectState>,
}

struct ObjectState {
    kind: u32,
    active: bool,
    written: bool,
    /// Ends after this pass (`SetEndOfStream`).
    ending: bool,
    volume: f32,
    buffer: Box<[f32; PERIOD]>,
}

impl State {
    /// Mixes the objects the title wrote this pass.
    fn finish_pass(&mut self) -> Vec<f32> {
        let mut out = vec![0.0f32; PERIOD * 2];
        for o in self.objects.iter_mut().filter(|o| o.active && o.written) {
            let (l, r) = audio_out::stereo_gains(speaker(o.kind));
            let (l, r) = (l * o.volume, r * o.volume);
            for (frame, &x) in out.as_chunks_mut::<2>().0.iter_mut().zip(o.buffer.iter()) {
                frame[0] += x * l;
                frame[1] += x * r;
            }
            o.active = !o.ending;
        }
        out
    }
}

/// The `SPEAKER_*` bit of a static `AudioObjectType`.
fn speaker(kind: u32) -> u32 {
    match kind {
        0x2 => 0x1,           // front left
        0x4 => 0x2,           // front right
        0x8 => 0x4,           // front centre
        0x10 => 0x8,          // LFE
        0x20 => 0x200,        // side left
        0x40 => 0x400,        // side right
        0x80 => 0x10,         // back left
        0x100 => 0x20,        // back right
        0x200 => 0x1000,      // top front left
        0x400 => 0x4000,      // top front right
        0x800 => 0x8000,      // top back left
        0x1000 => 0x20000,    // top back right
        0x2000 | 0x8000 => 0x10, // bottom left: as back left
        0x4000 | 0x1_0000 => 0x20, // bottom right
        _ => 0x100,           // back centre
    }
}

#[repr(C)]
struct Stream {
    vtbl: &'static Vtbl<11>,
    refs: AtomicU32,
    mask: u32,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Com for Stream {
    const IIDS: &'static [Guid] = &[
        IID_ISPATIAL_AUDIO_OBJECT_RENDER_STREAM,
        IID_ISPATIAL_AUDIO_OBJECT_RENDER_STREAM_BASE,
    ];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

static STREAM: Vtbl<11> = com_vtbl!(Stream;
    available_dynamic, not_implemented, stream_start, stream_stop, stream_reset,
    begin_update, end_update, activate_object);

impl Stream {
    fn new(mask: u32, event: usize) -> *mut Stream {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
        });
        let device = shared.clone();
        let thread = std::thread::Builder::new()
            .name("victory spatial audio".into())
            .spawn(move || device_thread(&device, event))
            .ok();
        combase::new(Stream {
            vtbl: &STREAM,
            refs: AtomicU32::new(0),
            mask,
            shared,
            thread,
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        lock(&self.shared.state).quit = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Sets the title's event once per period and plays what it renders;
/// silence for a pass it does not finish in time.
fn device_thread(shared: &Shared, event: usize) {
    let mut sink: Option<Box<dyn audio_out::Sink>> = None;
    let period = Duration::from_millis(10);
    loop {
        {
            let mut state = lock(&shared.state);
            while !state.running && !state.quit {
                state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.quit {
                return;
            }
            state.done = None;
        }
        if let Some(e) = handles::get::<handles::Event>(event) {
            e.set();
        }
        let frames = {
            let state = lock(&shared.state);
            let (mut state, _) = shared
                .wake
                .wait_timeout_while(state, 2 * period, |s| s.done.is_none() && !s.quit)
                .unwrap_or_else(|e| e.into_inner());
            state.done.take()
        };
        let frames = frames.unwrap_or_else(|| vec![0.0; PERIOD * 2]);
        sink.get_or_insert_with(|| audio_out::open("spatial audio", RATE, 2))
            .write(&frames);
    }
}

fn stream<'a>(this: *mut Stream) -> &'a Stream {
    self::this(this)
}

extern "win64" fn available_dynamic(_this: *mut Stream, out: *mut u32) -> HRESULT {
    if put(out, 0) { S_OK } else { E_POINTER }
}

extern "win64" fn stream_start(this: *mut Stream) -> HRESULT {
    let s = stream(this);
    lock(&s.shared.state).running = true;
    s.shared.wake.notify_all();
    S_OK
}

extern "win64" fn stream_stop(this: *mut Stream) -> HRESULT {
    lock(&stream(this).shared.state).running = false;
    S_OK
}

extern "win64" fn stream_reset(this: *mut Stream) -> HRESULT {
    let mut state = lock(&stream(this).shared.state);
    state.running = false;
    state.done = None;
    S_OK
}

extern "win64" fn begin_update(this: *mut Stream, dynamic: *mut u32, frames: *mut u32) -> HRESULT {
    let mut state = lock(&stream(this).shared.state);
    if state.updating {
        return SPTLAUDCLNT_E_OUT_OF_ORDER;
    }
    state.updating = true;
    for o in state.objects.iter_mut().filter(|o| o.active) {
        o.written = false;
        o.buffer.fill(0.0);
    }
    put(dynamic, 0);
    put(frames, PERIOD as u32);
    S_OK
}

extern "win64" fn end_update(this: *mut Stream) -> HRESULT {
    let s = stream(this);
    let mut state = lock(&s.shared.state);
    if !state.updating {
        return SPTLAUDCLNT_E_OUT_OF_ORDER;
    }
    state.updating = false;
    let frames = state.finish_pass();
    state.done = Some(frames);
    s.shared.wake.notify_all();
    S_OK
}

extern "win64" fn activate_object(this: *mut Stream, kind: u32, out: *mut *mut AudioObject) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    let s = stream(this);
    if kind & STATIC_TYPES == 0 {
        return SPTLAUDCLNT_E_NO_MORE_OBJECTS;
    }
    if !kind.is_power_of_two() || kind & s.mask == 0 {
        return SPTLAUDCLNT_E_STATIC_OBJECT_NOT_AVAILABLE;
    }
    let mut state = lock(&s.shared.state);
    if state.objects.iter().any(|o| o.kind == kind && o.active) {
        return SPTLAUDCLNT_E_OBJECT_ALREADY_ACTIVE;
    }
    state.objects.retain(|o| o.kind != kind);
    state.objects.push(ObjectState {
        kind,
        active: true,
        written: false,
        ending: false,
        volume: 1.0,
        buffer: Box::new([0.0; PERIOD]),
    });
    let o = combase::new(AudioObject {
        vtbl: &AUDIO_OBJECT,
        refs: AtomicU32::new(0),
        kind,
        shared: s.shared.clone(),
    });
    put(out, o);
    S_OK
}

// ---- static objects ----

#[repr(C)]
struct AudioObject {
    vtbl: &'static Vtbl<9>,
    refs: AtomicU32,
    kind: u32,
    shared: Arc<Shared>,
}

impl Com for AudioObject {
    const IIDS: &'static [Guid] = &[IID_ISPATIAL_AUDIO_OBJECT, IID_ISPATIAL_AUDIO_OBJECT_BASE];
    fn refs(&self) -> &AtomicU32 {
        &self.refs
    }
}

impl Drop for AudioObject {
    /// A released object stops playing.
    fn drop(&mut self) {
        lock(&self.shared.state).objects.retain(|o| o.kind != self.kind);
    }
}

static AUDIO_OBJECT: Vtbl<9> = com_vtbl!(AudioObject;
    object_buffer, object_end, object_active, object_kind, object_position, object_volume);

/// Runs `f` on the object's state if it still exists.
fn with_object<R>(this: *mut AudioObject, f: impl FnOnce(&mut ObjectState, bool) -> R) -> Option<R> {
    let o = self::this(this);
    let mut state = lock(&o.shared.state);
    let updating = state.updating;
    state.objects.iter_mut().find(|s| s.kind == o.kind).map(|s| f(s, updating))
}

extern "win64" fn object_buffer(this: *mut AudioObject, out: *mut *mut u8, len: *mut u32) -> HRESULT {
    if out.is_null() || len.is_null() {
        return E_POINTER;
    }
    let r = with_object(this, |o, updating| {
        if !updating {
            return Err(SPTLAUDCLNT_E_OUT_OF_ORDER);
        }
        if !o.active {
            return Err(SPTLAUDCLNT_E_RESOURCES_INVALIDATED);
        }
        o.written = true;
        // The buffer is boxed: it stays put while the title writes it
        // between Begin- and EndUpdatingAudioObjects.
        Ok(o.buffer.as_mut_ptr() as *mut u8)
    });
    match r.unwrap_or(Err(SPTLAUDCLNT_E_RESOURCES_INVALIDATED)) {
        Ok(p) => {
            put(out, p);
            put(len, (PERIOD * 4) as u32);
            S_OK
        }
        Err(hr) => hr,
    }
}

extern "win64" fn object_end(this: *mut AudioObject, _frames: u32) -> HRESULT {
    let r = with_object(this, |o, updating| {
        if !updating {
            return SPTLAUDCLNT_E_OUT_OF_ORDER;
        }
        o.ending = true;
        S_OK
    });
    r.unwrap_or(SPTLAUDCLNT_E_RESOURCES_INVALIDATED)
}

extern "win64" fn object_active(this: *mut AudioObject, out: *mut i32) -> HRESULT {
    let active = with_object(this, |o, _| o.active).unwrap_or(false);
    if put(out, active as i32) { S_OK } else { E_POINTER }
}

extern "win64" fn object_kind(this: *mut AudioObject, out: *mut u32) -> HRESULT {
    if put(out, self::this(this).kind) { S_OK } else { E_POINTER }
}

/// Static objects sit at their speaker.
extern "win64" fn object_position(_this: *mut AudioObject, _x: f32, _y: f32, _z: f32) -> HRESULT {
    SPTLAUDCLNT_E_PROPERTY_NOT_SUPPORTED
}

extern "win64" fn object_volume(this: *mut AudioObject, volume: f32) -> HRESULT {
    if !(0.0..=1.0).contains(&volume) {
        return E_INVALIDARG;
    }
    with_object(this, |o, _| o.volume = volume);
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use combase::{IID_IUNKNOWN, release_unknown};

    /// Calls slot `n` of a COM object's vtable as `F`.
    unsafe fn method<F: Copy>(object: *mut c_void, n: usize) -> F {
        // SAFETY: the caller names a slot that holds a function of type F.
        unsafe {
            let vtbl = *(object as *const *const *const ());
            std::mem::transmute_copy(&*vtbl.add(n))
        }
    }

    type Out = extern "win64" fn(*mut c_void, *mut *mut c_void) -> HRESULT;

    fn enumerator() -> *mut c_void {
        let mut e = std::ptr::null_mut();
        let hr = combase::CoCreateInstance(
            &CLSID_MM_DEVICE_ENUMERATOR,
            std::ptr::null_mut(),
            0x17,
            &IID_IMM_DEVICE_ENUMERATOR,
            &mut e,
        );
        assert_eq!(hr, S_OK);
        e
    }

    #[test]
    fn layouts_are_windows() {
        assert_eq!(size_of::<WaveFormat>(), 18);
        assert_eq!(size_of::<WaveFormatExtensible>(), 40);
        assert_eq!(size_of::<ActivationParams>(), 40);
        assert_eq!(size_of::<PropertyKey>(), 20);
    }

    #[test]
    fn one_render_device_as_the_title_finds_it() {
        let e = enumerator();
        // SAFETY: slots of IMMDeviceEnumerator, IMMDeviceCollection,
        // IMMDevice, IPropertyStore and IAudioClient.
        unsafe {
            let enum_endpoints: extern "win64" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT =
                method(e, 3);
            let mut c = std::ptr::null_mut();
            assert_eq!(enum_endpoints(e, E_RENDER, DEVICE_STATE_ACTIVE, &mut c), S_OK);
            let count: extern "win64" fn(*mut c_void, *mut u32) -> HRESULT = method(c, 3);
            let mut n = 0;
            assert_eq!(count(c, &mut n), S_OK);
            assert_eq!(n, 1);
            release_unknown(c);

            let default: extern "win64" fn(*mut c_void, u32, u32, *mut *mut c_void) -> HRESULT =
                method(e, 4);
            let mut d = std::ptr::null_mut();
            assert_eq!(default(e, 1, 0, &mut d), E_NOTFOUND); // no capture device
            assert_eq!(default(e, E_RENDER, 0, &mut d), S_OK);

            let store: extern "win64" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT = method(d, 4);
            let mut p = std::ptr::null_mut();
            assert_eq!(store(d, 0, &mut p), S_OK);
            let value: extern "win64" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> HRESULT =
                method(p, 5);
            let mut pv = PropVariant { vt: 0, reserved: [0; 3], value: [0; 2] };
            assert_eq!(value(p, &PKEY_FORM_FACTOR, &mut pv), S_OK);
            assert_eq!((pv.vt, pv.value[0]), (VT_UI4, 1));
            assert_eq!(value(p, &PKEY_FRIENDLY_NAME, &mut pv), S_OK);
            assert_eq!(pv.vt, VT_LPWSTR);
            combase::PropVariantClear(&mut pv);
            release_unknown(p);

            let activate: extern "win64" fn(
                *mut c_void,
                *const Guid,
                u32,
                *const PropVariant,
                *mut *mut c_void,
            ) -> HRESULT = method(d, 3);
            let mut a = std::ptr::null_mut();
            assert_eq!(activate(d, &IID_IUNKNOWN, 1, std::ptr::null(), &mut a), E_NOINTERFACE);
            assert_eq!(activate(d, &IID_IAUDIO_CLIENT, 1, std::ptr::null(), &mut a), S_OK);
            let mix: extern "win64" fn(*mut c_void, *mut *mut WaveFormatExtensible) -> HRESULT =
                method(a, 8);
            let mut f = std::ptr::null_mut();
            assert_eq!(mix(a, &mut f), S_OK);
            let format = f.read_unaligned();
            assert_eq!({ format.format.channels }, 2);
            assert_eq!({ format.format.rate }, RATE);
            assert_eq!({ format.sub_format }, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
            combase::CoTaskMemFree(f.cast());
            release_unknown(a);
            release_unknown(d);
        }
        release_unknown(e);
    }

    #[test]
    fn a_spatial_stream_plays_what_the_title_renders_each_period() {
        let event = handles::insert(handles::Event::new(false, false));
        let mut client = std::ptr::null_mut();
        let d = device(&IID_IMM_DEVICE, &mut client);
        assert_eq!(d, S_OK);
        // SAFETY: slots of IMMDevice, ISpatialAudioClient, the stream and
        // its objects, called as the title does.
        unsafe {
            let activate: extern "win64" fn(
                *mut c_void,
                *const Guid,
                u32,
                *const PropVariant,
                *mut *mut c_void,
            ) -> HRESULT = method(client, 3);
            let mut sc = std::ptr::null_mut();
            assert_eq!(activate(client, &IID_ISPATIAL_AUDIO_CLIENT, 1, std::ptr::null(), &mut sc), S_OK);
            release_unknown(client);

            let formats: Out = method(sc, 6);
            let mut fe = std::ptr::null_mut();
            assert_eq!(formats(sc, &mut fe), S_OK);
            let at: extern "win64" fn(*mut c_void, u32, *mut *const WaveFormat) -> HRESULT = method(fe, 4);
            let mut format = std::ptr::null();
            assert_eq!(at(fe, 0, &mut format), S_OK);
            release_unknown(fe);

            let params = ActivationParams {
                format,
                static_mask: FRONT_LEFT | FRONT_RIGHT | 8,
                min_dynamic: 0,
                max_dynamic: 0,
                category: 6,
                event,
                notify: std::ptr::null_mut(),
            };
            let blob = PropVariant {
                vt: VT_BLOB,
                reserved: [0; 3],
                value: [size_of::<ActivationParams>() as u64, &params as *const _ as u64],
            };
            let open: extern "win64" fn(*mut c_void, *const PropVariant, *const Guid, *mut *mut c_void) -> HRESULT =
                method(sc, 10);
            let mut s = std::ptr::null_mut();
            assert_eq!(open(sc, &blob, &IID_ISPATIAL_AUDIO_OBJECT_RENDER_STREAM, &mut s), S_OK);
            release_unknown(sc);

            let object: extern "win64" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT = method(s, 10);
            let mut left = std::ptr::null_mut();
            assert_eq!(object(s, FRONT_LEFT, &mut left), S_OK);
            let mut again = std::ptr::null_mut();
            assert_eq!(object(s, FRONT_LEFT, &mut again), SPTLAUDCLNT_E_OBJECT_ALREADY_ACTIVE);
            assert_eq!(object(s, 0x20, &mut again), SPTLAUDCLNT_E_STATIC_OBJECT_NOT_AVAILABLE);
            assert_eq!(object(s, 1, &mut again), SPTLAUDCLNT_E_NO_MORE_OBJECTS);

            let start: extern "win64" fn(*mut c_void) -> HRESULT = method(s, 5);
            let begin: extern "win64" fn(*mut c_void, *mut u32, *mut u32) -> HRESULT = method(s, 8);
            let end: extern "win64" fn(*mut c_void) -> HRESULT = method(s, 9);
            let buffer: extern "win64" fn(*mut c_void, *mut *mut u8, *mut u32) -> HRESULT = method(left, 3);
            let (mut p, mut len) = (std::ptr::null_mut(), 0);
            assert_eq!(buffer(left, &mut p, &mut len), SPTLAUDCLNT_E_OUT_OF_ORDER);
            assert_eq!(start(s), S_OK);

            let waitable = handles::object(event).unwrap();
            for pass in 0..3 {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !waitable.as_waitable().unwrap().try_acquire() {
                    assert!(std::time::Instant::now() < deadline, "pass {pass}: no event");
                    std::thread::sleep(Duration::from_millis(1));
                }
                let (mut dynamic, mut frames) = (9, 0);
                assert_eq!(begin(s, &mut dynamic, &mut frames), S_OK);
                assert_eq!((dynamic, frames), (0, PERIOD as u32));
                assert_eq!(begin(s, &mut dynamic, &mut frames), SPTLAUDCLNT_E_OUT_OF_ORDER);
                assert_eq!(buffer(left, &mut p, &mut len), S_OK);
                assert_eq!(len, (PERIOD * 4) as u32);
                std::slice::from_raw_parts_mut(p as *mut f32, PERIOD).fill(0.5);
                assert_eq!(end(s), S_OK);
            }
            release_unknown(left);
            release_unknown(s);
        }
        handles::close(event);
    }

    #[test]
    fn objects_downmix_to_their_side() {
        let mut state = State::default();
        for kind in [FRONT_LEFT, 0x100] {
            state.objects.push(ObjectState {
                kind,
                active: true,
                written: true,
                ending: kind == 0x100,
                volume: 1.0,
                buffer: Box::new([1.0; PERIOD]),
            });
        }
        let out = state.finish_pass();
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!((out[0], out[1]), (1.0, h));
        assert!(!state.objects[1].active && state.objects[0].active);
    }
}
