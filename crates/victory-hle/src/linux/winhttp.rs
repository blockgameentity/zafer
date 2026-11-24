//! WinHTTP, offline, as `modules::net` makes it on Windows: sessions open,
//! but `WinHttpConnect` fails with `ERROR_WINHTTP_CANNOT_CONNECT` for every
//! server, so no request handle ever exists and the request functions see
//! invalid handles. `WinHttpCrackUrl` is real: titles parse URLs with it.
// ponytail: loopback servers are not reachable either (Windows lets those
// through); add a connection when a title talks HTTP to itself.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use victory_loader::linux::teb;

use crate::{HleModule, hle_export};

type Hinternet = usize;
type Callback = extern "win64" fn(Hinternet, usize, u32, *mut c_void, u32);

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_WINHTTP_INVALID_URL: u32 = 12005;
const ERROR_WINHTTP_UNRECOGNIZED_SCHEME: u32 = 12006;
const ERROR_WINHTTP_INCORRECT_HANDLE_TYPE: u32 = 12018;
const ERROR_WINHTTP_CANNOT_CONNECT: u32 = 12029;

const WINHTTP_OPTION_CONTEXT_VALUE: u32 = 45;
const WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING: u32 = 0x800;
/// `WINHTTP_INVALID_STATUS_CALLBACK`.
const INVALID_CALLBACK: usize = usize::MAX;

#[derive(Default)]
struct Session {
    callback: usize,
    notifications: u32,
    context: usize,
}

static SESSIONS: Mutex<Option<HashMap<Hinternet, Session>>> = Mutex::new(None);

fn sessions<R>(f: impl FnOnce(&mut HashMap<Hinternet, Session>) -> R) -> R {
    let mut s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    f(s.get_or_insert_with(HashMap::new))
}

fn fail<T>(code: u32, value: T) -> T {
    teb::set_last_error(code);
    value
}

pub extern "win64" fn WinHttpOpen(
    _agent: *const u16,
    _access: u32,
    _proxy: *const u16,
    _bypass: *const u16,
    _flags: u32,
) -> Hinternet {
    static NEXT: AtomicUsize = AtomicUsize::new(0x5100);
    let h = NEXT.fetch_add(4, Ordering::Relaxed);
    sessions(|s| s.insert(h, Session::default()));
    h
}

pub extern "win64" fn WinHttpCloseHandle(h: Hinternet) -> i32 {
    let Some(session) = sessions(|s| s.remove(&h)) else {
        return fail(ERROR_INVALID_HANDLE, 0);
    };
    if session.callback != 0 && session.notifications & WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING != 0 {
        // SAFETY: the title's WINHTTP_STATUS_CALLBACK.
        let cb: Callback = unsafe { std::mem::transmute::<usize, Callback>(session.callback) };
        let mut handle = h;
        cb(h, session.context, WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING, (&mut handle as *mut usize).cast(), 8);
    }
    1
}

/// No network: there is no server to reach.
pub extern "win64" fn WinHttpConnect(session: Hinternet, server: *const u16, port: u16, _reserved: u32) -> Hinternet {
    if !sessions(|s| s.contains_key(&session)) {
        return fail(ERROR_INVALID_HANDLE, 0);
    }
    let name = wide(server).unwrap_or_default();
    tracing::info!(server = %name, port, "WinHttpConnect refused: no network");
    fail(ERROR_WINHTTP_CANNOT_CONNECT, 0)
}

pub extern "win64" fn WinHttpSetStatusCallback(h: Hinternet, callback: usize, notifications: u32, _reserved: usize) -> usize {
    sessions(|s| match s.get_mut(&h) {
        Some(session) => {
            session.notifications = notifications;
            std::mem::replace(&mut session.callback, callback)
        }
        None => fail(ERROR_INVALID_HANDLE, INVALID_CALLBACK),
    })
}

pub extern "win64" fn WinHttpSetOption(h: Hinternet, option: u32, buffer: *const c_void, len: u32) -> i32 {
    sessions(|s| match s.get_mut(&h) {
        Some(session) => {
            if option == WINHTTP_OPTION_CONTEXT_VALUE && !buffer.is_null() && len as usize >= size_of::<usize>() {
                // SAFETY: a DWORD_PTR from the title.
                session.context = unsafe { buffer.cast::<usize>().read_unaligned() };
            }
            1
        }
        None => fail(ERROR_INVALID_HANDLE, 0),
    })
}

pub extern "win64" fn WinHttpQueryOption(h: Hinternet, _option: u32, _buffer: *mut c_void, _len: *mut u32) -> i32 {
    let known = sessions(|s| s.contains_key(&h));
    fail(if known { ERROR_WINHTTP_INCORRECT_HANDLE_TYPE } else { ERROR_INVALID_HANDLE }, 0)
}

pub extern "win64" fn WinHttpSetTimeouts(h: Hinternet, _resolve: i32, _connect: i32, _send: i32, _receive: i32) -> i32 {
    if sessions(|s| s.contains_key(&h)) { 1 } else { fail(ERROR_INVALID_HANDLE, 0) }
}

// No connection handle exists, so no request handle can: every request
// function sees an invalid handle.

pub extern "win64" fn WinHttpOpenRequest(
    _connect: Hinternet,
    _verb: *const u16,
    _object: *const u16,
    _version: *const u16,
    _referrer: *const u16,
    _accept: *const *const u16,
    _flags: u32,
) -> Hinternet {
    fail(ERROR_INVALID_HANDLE, 0)
}

pub extern "win64" fn WinHttpAddRequestHeaders(_request: Hinternet, _headers: *const u16, _len: u32, _modifiers: u32) -> i32 {
    fail(ERROR_INVALID_HANDLE, 0)
}

pub extern "win64" fn WinHttpSendRequest(
    _request: Hinternet,
    _headers: *const u16,
    _headers_len: u32,
    _optional: *const c_void,
    _optional_len: u32,
    _total: u32,
    _context: usize,
) -> i32 {
    fail(ERROR_INVALID_HANDLE, 0)
}

pub extern "win64" fn WinHttpReceiveResponse(_request: Hinternet, _reserved: *mut c_void) -> i32 {
    fail(ERROR_INVALID_HANDLE, 0)
}

pub extern "win64" fn WinHttpQueryHeaders(
    _request: Hinternet,
    _level: u32,
    _name: *const u16,
    _buffer: *mut c_void,
    _len: *mut u32,
    _index: *mut u32,
) -> i32 {
    fail(ERROR_INVALID_HANDLE, 0)
}

pub extern "win64" fn WinHttpQueryDataAvailable(_request: Hinternet, _available: *mut u32) -> i32 {
    fail(ERROR_INVALID_HANDLE, 0)
}

pub extern "win64" fn WinHttpReadData(_request: Hinternet, _buffer: *mut c_void, _len: u32, _read: *mut u32) -> i32 {
    fail(ERROR_INVALID_HANDLE, 0)
}

pub extern "win64" fn WinHttpWriteData(_request: Hinternet, _buffer: *const c_void, _len: u32, _written: *mut u32) -> i32 {
    fail(ERROR_INVALID_HANDLE, 0)
}

// ---- URLs ----

fn wide(text: *const u16) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!text.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    })
}

/// `URL_COMPONENTS`: a pointer and a length per part, plus the scheme
/// number and the port.
#[repr(C)]
pub struct UrlComponents {
    size: u32,
    scheme: *mut u16,
    scheme_len: u32,
    scheme_id: i32,
    host: *mut u16,
    host_len: u32,
    port: u16,
    user: *mut u16,
    user_len: u32,
    password: *mut u16,
    password_len: u32,
    path: *mut u16,
    path_len: u32,
    extra: *mut u16,
    extra_len: u32,
}

/// Where each part of a URL is, in UTF-16 units: `(start, len)`.
#[derive(Debug, PartialEq)]
struct Parts {
    scheme: (usize, usize),
    scheme_id: i32,
    user: (usize, usize),
    password: (usize, usize),
    host: (usize, usize),
    port: u16,
    path: (usize, usize),
    extra: (usize, usize),
}

fn crack(url: &[u16]) -> Result<Parts, u32> {
    let find = |from: usize, to: usize, c: u8| (from..to).find(|&i| url[i] == u16::from(c));
    let colon = find(0, url.len(), b':').ok_or(ERROR_WINHTTP_UNRECOGNIZED_SCHEME)?;
    let scheme = String::from_utf16_lossy(&url[..colon]).to_ascii_lowercase();
    let (scheme_id, default_port) = match scheme.as_str() {
        "http" => (1, 80),
        "https" => (2, 443),
        _ => return Err(ERROR_WINHTTP_UNRECOGNIZED_SCHEME),
    };
    if url.get(colon + 1..colon + 3) != Some(&[u16::from(b'/'), u16::from(b'/')][..]) {
        return Err(ERROR_WINHTTP_INVALID_URL);
    }
    let start = colon + 3;
    let end = (start..url.len()).find(|&i| b"/?#".iter().any(|&c| url[i] == u16::from(c))).unwrap_or(url.len());
    let (mut user, mut password) = ((start, 0), (start, 0));
    let mut host_start = start;
    if let Some(at) = (start..end).rev().find(|&i| url[i] == u16::from(b'@')) {
        match find(start, at, b':') {
            Some(c) => {
                user = (start, c - start);
                password = (c + 1, at - c - 1);
            }
            None => user = (start, at - start),
        }
        host_start = at + 1;
    }
    let (host, port) = match find(host_start, end, b':') {
        Some(c) => {
            let digits = String::from_utf16_lossy(&url[c + 1..end]);
            let port = digits.parse::<u16>().map_err(|_| ERROR_WINHTTP_INVALID_URL)?;
            ((host_start, c - host_start), port)
        }
        None => ((host_start, end - host_start), default_port),
    };
    if host.1 == 0 {
        return Err(ERROR_WINHTTP_INVALID_URL);
    }
    let extra_start = find(end, url.len(), b'?').or_else(|| find(end, url.len(), b'#')).unwrap_or(url.len());
    Ok(Parts {
        scheme: (0, colon),
        scheme_id,
        user,
        password,
        host,
        port,
        path: (end, extra_start - end),
        extra: (extra_start, url.len() - extra_start),
    })
}

pub extern "win64" fn WinHttpCrackUrl(url: *const u16, len: u32, _flags: u32, out: *mut UrlComponents) -> i32 {
    if url.is_null() || out.is_null() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    let len = if len == 0 {
        // SAFETY: a NUL-terminated URL when no length is given.
        (0..).take_while(|&i| unsafe { *url.add(i) } != 0).count()
    } else {
        len as usize
    };
    // SAFETY: `len` units of URL.
    let text = unsafe { std::slice::from_raw_parts(url, len) };
    let parts = match crack(text) {
        Ok(p) => p,
        Err(code) => return fail(code, 0),
    };
    // SAFETY: the title's URL_COMPONENTS.
    let c = unsafe { &mut *out };
    if (c.size as usize) < size_of::<UrlComponents>() {
        return fail(ERROR_INVALID_PARAMETER, 0);
    }
    c.scheme_id = parts.scheme_id;
    c.port = parts.port;
    let mut short = false;
    for (ptr, plen, (start, n)) in [
        (&mut c.scheme, &mut c.scheme_len, parts.scheme),
        (&mut c.host, &mut c.host_len, parts.host),
        (&mut c.user, &mut c.user_len, parts.user),
        (&mut c.password, &mut c.password_len, parts.password),
        (&mut c.path, &mut c.path_len, parts.path),
        (&mut c.extra, &mut c.extra_len, parts.extra),
    ] {
        if *plen == 0 {
            continue; // not wanted
        }
        if ptr.is_null() {
            // A pointer into the URL itself.
            *ptr = text[start..].as_ptr() as *mut u16;
            *plen = n as u32;
        } else if (*plen as usize) <= n {
            *plen = n as u32 + 1;
            short = true;
        } else {
            // SAFETY: the title's buffer of `*plen` units, more than `n`.
            unsafe {
                std::ptr::copy_nonoverlapping(text[start..].as_ptr(), *ptr, n);
                *ptr.add(n) = 0;
            }
            *plen = n as u32;
        }
    }
    if short { fail(ERROR_INSUFFICIENT_BUFFER, 0) } else { 1 }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "winhttp",
        dlls: &["winhttp.dll"],
        exports: vec![
            hle_export!(WinHttpOpen),
            hle_export!(WinHttpCloseHandle),
            hle_export!(WinHttpConnect),
            hle_export!(WinHttpSetStatusCallback),
            hle_export!(WinHttpSetOption),
            hle_export!(WinHttpQueryOption),
            hle_export!(WinHttpSetTimeouts),
            hle_export!(WinHttpOpenRequest),
            hle_export!(WinHttpAddRequestHeaders),
            hle_export!(WinHttpSendRequest),
            hle_export!(WinHttpReceiveResponse),
            hle_export!(WinHttpQueryHeaders),
            hle_export!(WinHttpQueryDataAvailable),
            hle_export!(WinHttpReadData),
            hle_export!(WinHttpWriteData),
            hle_export!(WinHttpCrackUrl),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn sessions_open_and_connections_fail() {
        let s = WinHttpOpen(std::ptr::null(), 1, std::ptr::null(), std::ptr::null(), 0x1000_0000);
        assert_ne!(s, 0);
        assert_eq!(WinHttpSetTimeouts(s, 0, 0, 0, 0), 1);
        assert_eq!(WinHttpConnect(s, w("example.com").as_ptr(), 443, 0), 0);
        assert_eq!(teb::last_error(), ERROR_WINHTTP_CANNOT_CONNECT);
        assert_eq!(WinHttpOpenRequest(0, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0), 0);
        assert_eq!(teb::last_error(), ERROR_INVALID_HANDLE);
        assert_eq!(WinHttpCloseHandle(s), 1);
        assert_eq!(WinHttpCloseHandle(s), 0);
    }

    static CLOSED: AtomicUsize = AtomicUsize::new(0);
    extern "win64" fn on_status(h: Hinternet, context: usize, status: u32, _info: *mut c_void, _len: u32) {
        assert_eq!((status, context), (WINHTTP_CALLBACK_STATUS_HANDLE_CLOSING, 77));
        CLOSED.store(h, Ordering::Relaxed);
    }

    #[test]
    fn closing_an_async_session_calls_back() {
        let s = WinHttpOpen(std::ptr::null(), 1, std::ptr::null(), std::ptr::null(), 0x1000_0000);
        assert_eq!(WinHttpSetStatusCallback(s, on_status as *const () as usize, 0x800, 0), 0);
        let context = 77usize;
        assert_eq!(WinHttpSetOption(s, WINHTTP_OPTION_CONTEXT_VALUE, (&context as *const usize).cast(), 8), 1);
        assert_eq!(WinHttpCloseHandle(s), 1);
        assert_eq!(CLOSED.load(Ordering::Relaxed), s);
    }

    #[test]
    fn urls_crack_into_their_parts() {
        let url: Vec<u16> = "https://user:pw@example.com:8443/a/b?q=1#f".encode_utf16().collect();
        let p = crack(&url).unwrap();
        let text = |(s, n): (usize, usize)| String::from_utf16_lossy(&url[s..s + n]);
        assert_eq!(p.scheme_id, 2);
        assert_eq!(p.port, 8443);
        assert_eq!(text(p.scheme), "https");
        assert_eq!(text(p.user), "user");
        assert_eq!(text(p.password), "pw");
        assert_eq!(text(p.host), "example.com");
        assert_eq!(text(p.path), "/a/b");
        assert_eq!(text(p.extra), "?q=1#f");
        assert_eq!(crack(&"ftp://x".encode_utf16().collect::<Vec<_>>()), Err(ERROR_WINHTTP_UNRECOGNIZED_SCHEME));
        let bare = crack(&"http://host".encode_utf16().collect::<Vec<_>>()).unwrap();
        assert_eq!((bare.port, bare.path.1, bare.extra.1), (80, 0, 0));

        // Through the API: one part pointed at, one copied, one too small.
        let full = w("http://example.com/path");
        let mut host = [0u16; 4];
        let mut c = UrlComponents {
            size: size_of::<UrlComponents>() as u32,
            scheme: std::ptr::null_mut(),
            scheme_len: 1,
            scheme_id: 0,
            host: host.as_mut_ptr(),
            host_len: 4,
            port: 0,
            user: std::ptr::null_mut(),
            user_len: 0,
            password: std::ptr::null_mut(),
            password_len: 0,
            path: std::ptr::null_mut(),
            path_len: 1,
            extra: std::ptr::null_mut(),
            extra_len: 0,
        };
        assert_eq!(WinHttpCrackUrl(full.as_ptr(), 0, 0, &mut c), 0);
        assert_eq!(teb::last_error(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(c.host_len, 12);
        assert_eq!((c.scheme, c.scheme_len), (full.as_ptr() as *mut u16, 4));
        assert_eq!((c.path_len, c.port, c.scheme_id), (5, 80, 1));
    }

    #[test]
    fn url_components_layout_is_windows() {
        assert_eq!(size_of::<UrlComponents>(), 104);
        assert_eq!(std::mem::offset_of!(UrlComponents, port), 36);
        assert_eq!(std::mem::offset_of!(UrlComponents, extra_len), 96);
    }
}
