//! `combase` (`ole32`, `api-ms-win-core-com-*`, `api-ms-win-core-winrt-*`):
//! COM and WinRT start-up, task memory, GUIDs, `PROPVARIANT`s and
//! `HSTRING`s, plus the helpers our own COM objects are built from.
//!
//! There is no registry of classes: `CoCreateInstance` makes the classes
//! we implement (the audio device enumerator) and answers
//! `REGDB_E_CLASSNOTREG` for the rest, and no WinRT class can be
//! activated. Every thread is in the multithreaded apartment, so nothing
//! is ever marshalled.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{HleModule, hle_export};

#[allow(clippy::upper_case_acronyms)]
pub(crate) type HRESULT = i32;
pub(crate) const S_OK: HRESULT = 0;
pub(crate) const S_FALSE: HRESULT = 1;
pub(crate) const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub(crate) const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
pub(crate) const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
pub(crate) const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
pub(crate) const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
pub(crate) const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub(crate) const CLASS_E_NOAGGREGATION: HRESULT = 0x8004_0110_u32 as i32;
pub(crate) const REGDB_E_CLASSNOTREG: HRESULT = 0x8004_0154_u32 as i32;

/// A GUID in its in-memory (little-endian) layout.
pub(crate) type Guid = [u8; 16];

pub(crate) const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> Guid {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

pub(crate) const IID_IUNKNOWN: Guid = guid(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

// ---- our COM objects ----

/// A vtable of `N` win64 functions.
#[repr(transparent)]
pub(crate) struct Vtbl<const N: usize>(pub(crate) [*const (); N]);

// SAFETY: function addresses, never written.
unsafe impl<const N: usize> Sync for Vtbl<N> {}

/// One of our COM objects: `#[repr(C)]`, its vtable pointer first, then
/// its reference count. Lives in a `Box` from [`new`] until its last
/// `Release`.
pub(crate) trait Com: Sized + Send + Sync + 'static {
    /// The interfaces `QueryInterface` answers (besides `IUnknown`).
    const IIDS: &'static [Guid];
    fn refs(&self) -> &AtomicU32;
}

/// A new object with one reference.
pub(crate) fn new<T: Com>(object: T) -> *mut T {
    object.refs().store(1, Ordering::Relaxed);
    Box::into_raw(Box::new(object))
}

/// The object behind `this`.
pub(crate) fn this<'a, T: Com>(this: *mut T) -> &'a T {
    // SAFETY: title code calls our methods with the pointer we gave it,
    // which stays valid while it holds a reference.
    unsafe { &*this }
}

pub(crate) extern "win64" fn query_interface<T: Com>(
    this: *mut T,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null; a GUID and a pointer slot from the title.
    unsafe {
        if *iid == IID_IUNKNOWN || T::IIDS.contains(&*iid) {
            add_ref(this);
            *out = this.cast();
            S_OK
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

pub(crate) extern "win64" fn add_ref<T: Com>(this: *mut T) -> u32 {
    self::this(this).refs().fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) extern "win64" fn release<T: Com>(this: *mut T) -> u32 {
    let left = self::this(this).refs().fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference to a `Box` from `new`.
        drop(unsafe { Box::from_raw(this) });
    }
    left
}

/// `T`'s vtable: its `IUnknown` entries, then the methods given.
macro_rules! com_vtbl {
    ($t:ty; $($f:expr),* $(,)?) => {
        $crate::linux::combase::Vtbl([
            $crate::linux::combase::query_interface::<$t> as *const (),
            $crate::linux::combase::add_ref::<$t> as *const (),
            $crate::linux::combase::release::<$t> as *const (),
            $($f as *const ()),*
        ])
    };
}
pub(crate) use com_vtbl;

/// Hands `object` out through `out` as interface `iid`, then drops our
/// own reference.
pub(crate) fn hand_out<T: Com>(object: *mut T, iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    let hr = query_interface(object, iid, out);
    release(object);
    hr
}

/// Calls `Release` on a title (or our) COM interface.
pub(crate) fn release_unknown(unknown: *mut c_void) {
    if unknown.is_null() {
        return;
    }
    // SAFETY: a live COM interface: its first word is its vtable, slot 2 Release.
    unsafe {
        let vtbl = *(unknown as *const *const usize);
        let release: extern "win64" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
        release(unknown);
    }
}

/// Writes `value` through `out` if it is not null.
pub(crate) fn put<T>(out: *mut T, value: T) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: a non-null out-parameter from the title.
    unsafe { out.write(value) };
    true
}

// ---- start-up ----

thread_local! {
    static COM_INITS: Cell<u32> = const { Cell::new(0) };
    static RO_INITS: Cell<u32> = const { Cell::new(0) };
}

fn init(count: &'static std::thread::LocalKey<Cell<u32>>) -> HRESULT {
    let n = count.get();
    count.set(n + 1);
    if n == 0 { S_OK } else { S_FALSE }
}

fn uninit(count: &'static std::thread::LocalKey<Cell<u32>>) {
    count.set(count.get().saturating_sub(1));
}

pub extern "win64" fn CoInitializeEx(_reserved: *mut c_void, _model: u32) -> HRESULT {
    init(&COM_INITS)
}

pub extern "win64" fn CoUninitialize() {
    uninit(&COM_INITS)
}

pub extern "win64" fn RoInitialize(_kind: u32) -> HRESULT {
    init(&RO_INITS)
}

pub extern "win64" fn RoUninitialize() {
    uninit(&RO_INITS)
}

const APTTYPE_MTA: u32 = 1;

pub extern "win64" fn CoGetApartmentType(kind: *mut u32, qualifier: *mut u32) -> HRESULT {
    if kind.is_null() || qualifier.is_null() {
        return E_INVALIDARG;
    }
    put(kind, APTTYPE_MTA);
    put(qualifier, 0);
    S_OK
}

/// One context for the whole process: the multithreaded apartment.
pub extern "win64" fn CoGetContextToken(token: *mut usize) -> HRESULT {
    if put(token, 1) { S_OK } else { E_POINTER }
}

pub extern "win64" fn CoGetObjectContext(_iid: *const Guid, out: *mut *mut c_void) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub extern "win64" fn CoCreateFreeThreadedMarshaler(
    _outer: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub extern "win64" fn CoMarshalInterThreadInterfaceInStream(
    _iid: *const Guid,
    _unknown: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub extern "win64" fn CoGetInterfaceAndReleaseStream(
    stream: *mut c_void,
    _iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    release_unknown(stream);
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

static SERVER_REFS: AtomicU32 = AtomicU32::new(0);

pub extern "win64" fn CoAddRefServerProcess() -> u32 {
    SERVER_REFS.fetch_add(1, Ordering::Relaxed) + 1
}

pub extern "win64" fn CoReleaseServerProcess() -> u32 {
    let n = SERVER_REFS.load(Ordering::Relaxed).saturating_sub(1);
    SERVER_REFS.store(n, Ordering::Relaxed);
    n
}

// ---- classes ----

pub extern "win64" fn CoCreateInstance(
    clsid: *const Guid,
    outer: *mut c_void,
    _context: u32,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || clsid.is_null() {
        return E_POINTER;
    }
    put(out, std::ptr::null_mut());
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    // SAFETY: checked non-null.
    let clsid = unsafe { *clsid };
    if let Some(hr) = super::mmdevapi::create(&clsid, iid, out) {
        return hr;
    }
    tracing::debug!(clsid = %guid_text(&clsid), "CoCreateInstance: class not registered");
    REGDB_E_CLASSNOTREG
}

pub extern "win64" fn RoGetActivationFactory(
    _class: Hstring,
    _iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    put(out, std::ptr::null_mut());
    REGDB_E_CLASSNOTREG
}

/// Our process serves no WinRT classes to anyone else; registering is
/// accepted and forgotten.
pub extern "win64" fn RoRegisterActivationFactories(
    _classes: *const Hstring,
    _factories: *const c_void,
    _count: u32,
    cookie: *mut usize,
) -> HRESULT {
    put(cookie, 1);
    S_OK
}

pub extern "win64" fn RoRevokeActivationFactories(_cookie: usize) {}

pub extern "win64" fn RoGetApartmentIdentifier(id: *mut u64) -> HRESULT {
    if put(id, 1) { S_OK } else { E_POINTER }
}

/// The apartment never shuts down while the title runs.
pub extern "win64" fn RoRegisterForApartmentShutdown(
    _callback: *mut c_void,
    id: *mut u64,
    cookie: *mut usize,
) -> HRESULT {
    put(id, 1);
    put(cookie, 1);
    S_OK
}

pub extern "win64" fn RoUnregisterForApartmentShutdown(_cookie: usize) -> HRESULT {
    S_OK
}

// ---- WinRT errors: no error objects are kept ----

pub extern "win64" fn RoOriginateError(error: HRESULT, _message: Hstring) -> i32 {
    tracing::debug!(error = format_args!("{error:#010x}"), "RoOriginateError");
    1
}

pub extern "win64" fn RoTransformError(_old: HRESULT, _new: HRESULT, _message: Hstring) -> i32 {
    1
}

pub extern "win64" fn GetRestrictedErrorInfo(out: *mut *mut c_void) -> HRESULT {
    put(out, std::ptr::null_mut());
    S_FALSE
}

pub extern "win64" fn SetRestrictedErrorInfo(_info: *mut c_void) -> HRESULT {
    S_OK
}

// ---- task memory ----

pub extern "win64" fn CoTaskMemAlloc(size: usize) -> *mut c_void {
    // SAFETY: plain allocation; malloc(0) may return a unique pointer.
    unsafe { libc::malloc(size.max(1)) }
}

pub extern "win64" fn CoTaskMemRealloc(block: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: `block` is null or from CoTaskMemAlloc.
    unsafe { libc::realloc(block, size.max(1)) }
}

pub extern "win64" fn CoTaskMemFree(block: *mut c_void) {
    // SAFETY: `block` is null or from CoTaskMemAlloc.
    unsafe { libc::free(block) }
}

/// A NUL-terminated copy of `text` in task memory.
pub(crate) fn task_string(text: &str) -> *mut u16 {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let out = CoTaskMemAlloc(wide.len() * 2) as *mut u16;
    if !out.is_null() {
        // SAFETY: room for `wide.len()` units.
        unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), out, wide.len()) };
    }
    out
}

// ---- GUIDs ----

pub extern "win64" fn CoCreateGuid(out: *mut Guid) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let mut g = [0u8; 16];
    // SAFETY: 16 writable bytes.
    if unsafe { libc::getrandom(g.as_mut_ptr().cast(), 16, 0) } != 16 {
        return E_FAIL;
    }
    // A version 4 (random) GUID: Data3's high nibble, then the variant.
    g[7] = (g[7] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    put(out, g);
    S_OK
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, upper case.
pub(crate) fn guid_text(g: &Guid) -> String {
    let a = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let b = u16::from_le_bytes([g[4], g[5]]);
    let c = u16::from_le_bytes([g[6], g[7]]);
    let d: String = g[8..].iter().map(|x| format!("{x:02X}")).collect();
    format!("{{{a:08X}-{b:04X}-{c:04X}-{}-{}}}", &d[..4], &d[4..])
}

/// Returns the characters written including the NUL, 0 if they do not fit.
pub extern "win64" fn StringFromGUID2(g: *const Guid, out: *mut u16, capacity: i32) -> i32 {
    if g.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let text: Vec<u16> = guid_text(unsafe { &*g }).encode_utf16().chain([0]).collect();
    if (capacity as usize) < text.len() || capacity < 0 {
        return 0;
    }
    // SAFETY: the title gave room for `capacity` units.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    text.len() as i32
}

// ---- PROPVARIANT ----

pub(crate) const VT_EMPTY: u16 = 0;
pub(crate) const VT_BSTR: u16 = 8;
pub(crate) const VT_UNKNOWN: u16 = 13;
pub(crate) const VT_UI4: u16 = 19;
pub(crate) const VT_LPSTR: u16 = 30;
pub(crate) const VT_LPWSTR: u16 = 31;
pub(crate) const VT_BLOB: u16 = 65;
pub(crate) const VT_CLSID: u16 = 72;

/// `PROPVARIANT`: the type, three reserved words, then 16 bytes of value.
#[repr(C)]
pub(crate) struct PropVariant {
    pub(crate) vt: u16,
    pub(crate) reserved: [u16; 3],
    pub(crate) value: [u64; 2],
}

pub extern "win64" fn PropVariantClear(pv: *mut PropVariant) -> HRESULT {
    if pv.is_null() {
        return S_OK;
    }
    // SAFETY: a PROPVARIANT the title owns; what it points to is ours to free.
    let p = unsafe { &mut *pv };
    let first = p.value[0] as *mut c_void;
    match p.vt {
        VT_EMPTY => {}
        VT_LPSTR | VT_LPWSTR | VT_CLSID => CoTaskMemFree(first),
        VT_BLOB => CoTaskMemFree(p.value[1] as *mut c_void),
        VT_BSTR => super::oleaut32::SysFreeString(first.cast()),
        VT_UNKNOWN => release_unknown(first),
        _ => {}
    }
    // SAFETY: as above.
    unsafe { pv.write_bytes(0, 1) };
    S_OK
}

// ---- HSTRING ----

/// An `HSTRING`: null for the empty string, else one of these. A string
/// reference lives in the caller's 24-byte `HSTRING_HEADER`.
#[repr(C)]
pub(crate) struct HstringData {
    reference: u32,
    len: u32,
    text: *const u16,
    refs: AtomicU32,
}

pub(crate) type Hstring = *mut HstringData;

const _: () = assert!(std::mem::size_of::<HstringData>() <= 24);

fn hstring<'a>(h: Hstring) -> Option<&'a HstringData> {
    // SAFETY: null or an HSTRING from us.
    unsafe { h.as_ref() }
}

fn units<'a>(h: Hstring) -> &'a [u16] {
    match hstring(h) {
        // SAFETY: `len` units at `text`, kept alive by the string.
        Some(s) => unsafe { std::slice::from_raw_parts(s.text, s.len as usize) },
        None => &[],
    }
}

fn owned(text: &[u16]) -> Hstring {
    let copy: Box<[u16]> = text.iter().copied().chain([0]).collect();
    Box::into_raw(Box::new(HstringData {
        reference: 0,
        len: text.len() as u32,
        text: Box::into_raw(copy).cast(),
        refs: AtomicU32::new(1),
    }))
}

pub extern "win64" fn WindowsCreateString(text: *const u16, len: u32, out: *mut Hstring) -> HRESULT {
    if out.is_null() || (text.is_null() && len != 0) {
        return E_INVALIDARG;
    }
    if len == 0 {
        put(out, std::ptr::null_mut());
        return S_OK;
    }
    // SAFETY: the title passes `len` readable units.
    put(out, owned(unsafe { std::slice::from_raw_parts(text, len as usize) }));
    S_OK
}

pub extern "win64" fn WindowsCreateStringReference(
    text: *const u16,
    len: u32,
    header: *mut HstringData,
    out: *mut Hstring,
) -> HRESULT {
    if out.is_null() || header.is_null() || (text.is_null() && len != 0) {
        return E_INVALIDARG;
    }
    if len == 0 {
        put(out, std::ptr::null_mut());
        return S_OK;
    }
    // SAFETY: a reference needs the text NUL-terminated at `len`.
    if unsafe { *text.add(len as usize) } != 0 {
        return E_INVALIDARG;
    }
    let data = HstringData {
        reference: 1,
        len,
        text,
        refs: AtomicU32::new(0),
    };
    // SAFETY: the caller's HSTRING_HEADER, 24 bytes.
    unsafe { header.write(data) };
    put(out, header);
    S_OK
}

pub extern "win64" fn WindowsDuplicateString(h: Hstring, out: *mut Hstring) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let copy = match hstring(h) {
        None => std::ptr::null_mut(),
        Some(s) if s.reference != 0 => owned(units(h)),
        Some(s) => {
            s.refs.fetch_add(1, Ordering::Relaxed);
            h
        }
    };
    put(out, copy);
    S_OK
}

pub extern "win64" fn WindowsDeleteString(h: Hstring) -> HRESULT {
    if let Some(s) = hstring(h)
        && s.reference == 0
        && s.refs.fetch_sub(1, Ordering::AcqRel) == 1
    {
        // SAFETY: the last reference to a string from `owned`.
        unsafe {
            let data = Box::from_raw(h);
            let text = std::ptr::slice_from_raw_parts_mut(data.text as *mut u16, data.len as usize + 1);
            drop(Box::from_raw(text));
        }
    }
    S_OK
}

pub extern "win64" fn WindowsGetStringLen(h: Hstring) -> u32 {
    hstring(h).map_or(0, |s| s.len)
}

pub extern "win64" fn WindowsGetStringRawBuffer(h: Hstring, len: *mut u32) -> *const u16 {
    static EMPTY: u16 = 0;
    put(len, WindowsGetStringLen(h));
    hstring(h).map_or(&EMPTY, |s| s.text)
}

pub extern "win64" fn WindowsIsStringEmpty(h: Hstring) -> i32 {
    (WindowsGetStringLen(h) == 0) as i32
}

pub extern "win64" fn WindowsStringHasEmbeddedNull(h: Hstring, out: *mut i32) -> HRESULT {
    if put(out, units(h).contains(&0) as i32) { S_OK } else { E_INVALIDARG }
}

pub extern "win64" fn WindowsCompareStringOrdinal(a: Hstring, b: Hstring, out: *mut i32) -> HRESULT {
    if put(out, units(a).cmp(units(b)) as i32) { S_OK } else { E_INVALIDARG }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "combase",
        dlls: &[
            "combase.dll",
            "ole32.dll",
            "api-ms-win-core-com-*",
            "api-ms-win-core-winrt-*",
        ],
        exports: vec![
            hle_export!(CoInitializeEx),
            hle_export!(CoUninitialize),
            hle_export!(RoInitialize),
            hle_export!(RoUninitialize),
            hle_export!(CoGetApartmentType),
            hle_export!(CoGetContextToken),
            hle_export!(CoGetObjectContext),
            hle_export!(CoCreateFreeThreadedMarshaler),
            hle_export!(CoMarshalInterThreadInterfaceInStream),
            hle_export!(CoGetInterfaceAndReleaseStream),
            hle_export!(CoAddRefServerProcess),
            hle_export!(CoReleaseServerProcess),
            hle_export!(CoCreateInstance),
            hle_export!(RoGetActivationFactory),
            hle_export!(RoRegisterActivationFactories),
            hle_export!(RoRevokeActivationFactories),
            hle_export!(RoGetApartmentIdentifier),
            hle_export!(RoRegisterForApartmentShutdown),
            hle_export!(RoUnregisterForApartmentShutdown),
            hle_export!(RoOriginateError),
            hle_export!(RoTransformError),
            hle_export!(GetRestrictedErrorInfo),
            hle_export!(SetRestrictedErrorInfo),
            hle_export!(CoTaskMemAlloc),
            hle_export!(CoTaskMemRealloc),
            hle_export!(CoTaskMemFree),
            hle_export!(CoCreateGuid),
            hle_export!(StringFromGUID2),
            hle_export!(PropVariantClear),
            hle_export!(WindowsCreateString),
            hle_export!(WindowsCreateStringReference),
            hle_export!(WindowsDuplicateString),
            hle_export!(WindowsDeleteString),
            hle_export!(WindowsGetStringLen),
            hle_export!(WindowsGetStringRawBuffer),
            hle_export!(WindowsIsStringEmpty),
            hle_export!(WindowsStringHasEmbeddedNull),
            hle_export!(WindowsCompareStringOrdinal),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn unknown_classes_are_not_registered() {
        let mut out = std::ptr::dangling_mut::<c_void>();
        let clsid = guid(0x1234_5678, 1, 2, [3; 8]);
        let hr = CoCreateInstance(&clsid, std::ptr::null_mut(), 1, &IID_IUNKNOWN, &mut out);
        assert_eq!(hr, REGDB_E_CLASSNOTREG);
        assert!(out.is_null());
    }

    #[test]
    fn start_up_counts_per_thread() {
        assert_eq!(CoInitializeEx(std::ptr::null_mut(), 0), S_OK);
        assert_eq!(CoInitializeEx(std::ptr::null_mut(), 0), S_FALSE);
        CoUninitialize();
        CoUninitialize();
        assert_eq!(CoInitializeEx(std::ptr::null_mut(), 0), S_OK);
        CoUninitialize();
    }

    #[test]
    fn guids_are_random_version_4_and_print_like_windows() {
        let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
        assert_eq!(CoCreateGuid(&mut a), S_OK);
        assert_eq!(CoCreateGuid(&mut b), S_OK);
        assert_ne!(a, b);
        assert_eq!(a[7] >> 4, 4);
        assert_eq!(a[8] >> 6, 2);
        let mut text = [0u16; 39];
        assert_eq!(StringFromGUID2(&IID_IUNKNOWN, text.as_mut_ptr(), 39), 39);
        assert_eq!(text.to_vec(), wide("{00000000-0000-0000-C000-000000000046}"));
        assert_eq!(StringFromGUID2(&IID_IUNKNOWN, text.as_mut_ptr(), 38), 0);
    }

    #[test]
    fn propvariant_clear_frees_strings_and_zeroes() {
        let mut pv = PropVariant {
            vt: VT_LPWSTR,
            reserved: [7; 3],
            value: [task_string("speakers") as u64, 9],
        };
        assert_eq!(std::mem::size_of::<PropVariant>(), 24);
        assert_eq!(PropVariantClear(&mut pv), S_OK);
        assert_eq!((pv.vt, pv.reserved, pv.value), (VT_EMPTY, [0; 3], [0; 2]));
    }

    #[test]
    fn hstrings_copy_reference_and_compare() {
        let text = wide("abc");
        let mut a: Hstring = std::ptr::null_mut();
        assert_eq!(WindowsCreateString(text.as_ptr(), 3, &mut a), S_OK);
        assert_eq!(WindowsGetStringLen(a), 3);
        let mut len = 0;
        let raw = WindowsGetStringRawBuffer(a, &mut len);
        // SAFETY: an owned string is NUL-terminated.
        assert_eq!((len, unsafe { *raw.add(3) }), (3, 0));

        let mut header = std::mem::MaybeUninit::<[u64; 3]>::uninit();
        let mut r: Hstring = std::ptr::null_mut();
        let hr = WindowsCreateStringReference(text.as_ptr(), 3, header.as_mut_ptr().cast(), &mut r);
        assert_eq!(hr, S_OK);
        assert_eq!(WindowsGetStringRawBuffer(r, std::ptr::null_mut()), text.as_ptr());
        let mut order = 9;
        assert_eq!(WindowsCompareStringOrdinal(a, r, &mut order), S_OK);
        assert_eq!(order, 0);
        // A reference without its NUL is refused.
        assert_eq!(
            WindowsCreateStringReference(text.as_ptr(), 2, header.as_mut_ptr().cast(), &mut r),
            E_INVALIDARG
        );

        let mut d: Hstring = std::ptr::null_mut();
        assert_eq!(WindowsDuplicateString(a, &mut d), S_OK);
        assert_eq!(d, a);
        WindowsDeleteString(a);
        assert_eq!(WindowsGetStringLen(d), 3);
        WindowsDeleteString(d);

        let mut empty: Hstring = 1usize as Hstring;
        assert_eq!(WindowsCreateString(std::ptr::null(), 0, &mut empty), S_OK);
        assert!(empty.is_null() && WindowsIsStringEmpty(empty) == 1);
        assert_eq!(WindowsCompareStringOrdinal(empty, r, &mut order), S_OK);
        assert_eq!(order, -1);
    }
}
