//! ucrtbase: stdio (`api-ms-win-crt-stdio`): FILE, the descriptor table
//! (`_open`, `_read`, ...), and the printf and scanf entry points
//! (`ucrt_format`, `ucrt_scan`).
//!
//! Layering as in the UCRT: a FILE is a buffer over a descriptor, a
//! descriptor a host fd plus the text-mode bit, and CR LF translation
//! happens in the descriptor's read and write. Descriptors 0, 1 and 2 are
//! the host's standard streams, written without CR before LF (the host's
//! terminal speaks LF).
//!
//! The FILE is opaque but for `_get_stream_buffer_pointers`, through which
//! msvcp140's `basic_filebuf` works on `ptr`, `base` and `cnt` directly
//! (get area `[ptr, ptr + cnt)`, put area: `cnt` is the room behind `ptr`).
//! So those fields mean what they mean in the UCRT between any two calls,
//! and nothing here keeps a second copy: reading, `[ptr, ptr + cnt)` is
//! read-ahead; writing, `[base, ptr)` is unwritten output; fresh, all null
//! until the first read or write. The first 48 bytes are the UCRT's
//! `__crt_stdio_stream_data`. stdout and stderr keep nothing between calls,
//! so their output keeps its order with our log. A stream object is reused,
//! never freed: a FILE* kept after `fclose` is a closed stream.
//!
//! Wide characters on a text stream are UTF-8 on the host side; on a binary
//! stream two bytes each. The UCRT's Unicode text modes are refused.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CString, c_char};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use super::ucrt_filesystem::{narrow_path, wide_path};
use super::ucrt_format::to_vec;
use super::ucrt_runtime::{
    EBADF, EINVAL, EMFILE, ENOMEM, RecursiveLock, fail, fail_from_host, invalid, set_errno,
};
use crate::{HleModule, hle_export};

// fcntl.h of the UCRT.
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_APPEND: i32 = 8;
const O_TEMPORARY: i32 = 0x40;
const O_CREAT: i32 = 0x100;
const O_TRUNC: i32 = 0x200;
const O_EXCL: i32 = 0x400;
const O_TEXT: i32 = 0x4000;
const O_BINARY: i32 = 0x8000;
const O_UNICODE: i32 = 0x10000 | 0x20000 | 0x40000; // _O_WTEXT, _O_U16TEXT, _O_U8TEXT
const S_IWRITE: i32 = 0x80;
const EOF: i32 = -1;
const WEOF: u16 = 0xFFFF;

/// `_set_fmode`: the translation of an open that names none.
static FMODE: AtomicI32 = AtomicI32::new(O_TEXT);
/// `__p__commode`.
static COMMODE: AtomicI32 = AtomicI32::new(0);

// ---- descriptors --------------------------------------------------------------

#[derive(Clone, Copy)]
struct Fd {
    host: i32,
    /// Wide characters convert to and from text.
    text: bool,
    /// LF is written as CR LF, CR LF is read as LF.
    crlf: bool,
}

static FDS: LazyLock<Mutex<Vec<Option<Fd>>>> = LazyLock::new(|| {
    let standard = |host| Some(Fd { host, text: true, crlf: false });
    Mutex::new(vec![standard(0), standard(1), standard(2)])
});

fn fds() -> MutexGuard<'static, Vec<Option<Fd>>> {
    FDS.lock().unwrap_or_else(|e| e.into_inner())
}

fn fd(fd: i32) -> Option<Fd> {
    let fd = usize::try_from(fd).ok()?;
    fds().get(fd).copied().flatten()
}

fn bad_fd() -> i32 {
    set_errno(EBADF);
    -1
}

/// Opens a host path with the UCRT's `_O_*` bits; a new descriptor, or -1
/// with errno set.
pub(crate) fn open(path: &Path, oflag: i32, pmode: i32) -> i32 {
    if oflag & 3 == 3 || oflag & O_UNICODE != 0 {
        set_errno(EINVAL);
        return -1;
    }
    let mut flags = libc::O_CLOEXEC
        | match oflag & 3 {
            O_WRONLY => libc::O_WRONLY,
            O_RDWR => libc::O_RDWR,
            _ => libc::O_RDONLY,
        };
    for (ours, host) in [
        (O_CREAT, libc::O_CREAT),
        (O_TRUNC, libc::O_TRUNC),
        (O_EXCL, libc::O_EXCL),
        (O_APPEND, libc::O_APPEND),
    ] {
        if oflag & ours != 0 {
            flags |= host;
        }
    }
    let mode = if pmode & S_IWRITE != 0 { 0o666 } else { 0o444 };
    let Ok(name) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        set_errno(EINVAL);
        return -1;
    };
    // SAFETY: a NUL-terminated path.
    let host = unsafe { libc::open(name.as_ptr(), flags, mode as libc::c_uint) };
    if host < 0 {
        return fail_from_host();
    }
    if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        // SAFETY: the fd we just opened.
        unsafe { libc::close(host) };
        set_errno(super::ucrt_runtime::EACCES);
        return -1;
    }
    if oflag & O_TEMPORARY != 0 {
        let _ = std::fs::remove_file(path); // gone on close, as on Windows
    }
    let text = oflag & O_BINARY == 0 && (oflag & O_TEXT != 0 || FMODE.load(Ordering::Relaxed) & O_TEXT != 0);
    let entry = Some(Fd { host, text, crlf: text });
    let mut table = fds();
    let slot = match table.iter().position(Option::is_none) {
        Some(slot) => {
            table[slot] = entry;
            slot
        }
        None => {
            table.push(entry);
            table.len() - 1
        }
    };
    slot as i32
}

pub(crate) fn close(fd: i32) -> i32 {
    let Some(entry) = usize::try_from(fd).ok().and_then(|i| fds().get_mut(i).and_then(Option::take)) else {
        return bad_fd();
    };
    // SAFETY: a host fd of ours; the host's own standard streams stay open.
    if entry.host > 2 && unsafe { libc::close(entry.host) } != 0 {
        return fail_from_host();
    }
    0
}

/// Reads; text mode turns CR LF into LF and keeps a lone CR.
/// ponytail: Ctrl-Z does not end text input, and a CR that ends a read
/// from a pipe is kept even when LF follows.
fn read(fd_: i32, buffer: *mut u8, count: u32) -> i32 {
    let Some(entry) = fd(fd_) else { return bad_fd() };
    if count == 0 {
        return 0;
    }
    let count = count.min(i32::MAX as u32) as usize;
    // The kernel writes the buffer, and fails on blocks the GPU side guards.
    victory_gpu::kernel_will_write(buffer as usize, count);
    // SAFETY: the caller's buffer of count bytes.
    let n = unsafe { libc::read(entry.host, buffer.cast(), count) };
    if n < 0 {
        return fail_from_host();
    }
    let n = n as usize;
    if !entry.crlf || n == 0 {
        return n as i32;
    }
    // SAFETY: the n bytes just read.
    let bytes = unsafe { std::slice::from_raw_parts_mut(buffer, n) };
    let mut out = 0;
    for i in 0..n {
        if bytes[i] == b'\r' && i + 1 < n && bytes[i + 1] == b'\n' {
            continue;
        }
        bytes[out] = bytes[i];
        out += 1;
    }
    if bytes[out - 1] == b'\r' {
        let mut next = 0u8;
        // SAFETY: one byte of ours.
        if unsafe { libc::read(entry.host, (&mut next as *mut u8).cast(), 1) } == 1 {
            if next == b'\n' {
                bytes[out - 1] = b'\n';
            } else {
                // SAFETY: the host fd; give the byte back.
                unsafe { libc::lseek(entry.host, -1, libc::SEEK_CUR) };
            }
        }
    }
    out as i32
}

/// Writes all of it; text mode turns LF into CR LF. Answers with the
/// caller's byte count.
fn write(fd_: i32, buffer: *const u8, count: u32) -> i32 {
    let Some(entry) = fd(fd_) else { return bad_fd() };
    // SAFETY: the caller's buffer of count bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer, count as usize) };
    let expanded;
    let mut rest: &[u8] = if entry.crlf && bytes.contains(&b'\n') {
        expanded = bytes.iter().fold(Vec::with_capacity(bytes.len() * 2), |mut v, &b| {
            if b == b'\n' {
                v.push(b'\r');
            }
            v.push(b);
            v
        });
        &expanded
    } else {
        bytes
    };
    while !rest.is_empty() {
        // SAFETY: the bytes left to write.
        let n = unsafe { libc::write(entry.host, rest.as_ptr().cast(), rest.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return fail_from_host();
        }
        rest = &rest[n as usize..];
    }
    count as i32
}

fn seek(fd_: i32, offset: i64, whence: i32) -> i64 {
    let Some(entry) = fd(fd_) else { return i64::from(bad_fd()) };
    if !(0..=2).contains(&whence) {
        set_errno(EINVAL);
        return -1;
    }
    // SAFETY: a host fd of ours (SEEK_SET/CUR/END are 0/1/2 on both).
    let at = unsafe { libc::lseek(entry.host, offset, whence) };
    if at < 0 { i64::from(fail_from_host()) } else { at }
}

pub extern "win64" fn _open(path: *const c_char, oflag: i32, pmode: i32) -> i32 {
    match narrow_path(path) {
        Some(path) => open(&path, oflag, pmode),
        None => invalid(EINVAL).min(-1),
    }
}

pub extern "win64" fn _wopen(path: *const u16, oflag: i32, pmode: i32) -> i32 {
    match wide_path(path) {
        Some(path) => open(&path, oflag, pmode),
        None => invalid(EINVAL).min(-1),
    }
}

pub extern "win64" fn _close(fd: i32) -> i32 {
    close(fd)
}

pub extern "win64" fn _read(fd: i32, buffer: *mut u8, count: u32) -> i32 {
    if buffer.is_null() && count != 0 {
        invalid(EINVAL);
        return -1;
    }
    read(fd, buffer, count)
}

pub extern "win64" fn _write(fd: i32, buffer: *const u8, count: u32) -> i32 {
    if buffer.is_null() && count != 0 {
        invalid(EINVAL);
        return -1;
    }
    write(fd, buffer, count)
}

pub extern "win64" fn _lseeki64(fd: i32, offset: i64, whence: i32) -> i64 {
    seek(fd, offset, whence)
}

// ---- streams --------------------------------------------------------------

const BUFFER_SIZE: i32 = 4096; // _INTERNAL_BUFSIZ

// Stream::flags
const CAN_READ: i32 = 1;
const CAN_WRITE: i32 = 2;
const READING: i32 = 4; // the buffer holds read-ahead
const WRITING: i32 = 8; // the buffer holds output
const AT_EOF: i32 = 0x10;
const ERROR: i32 = 0x20;
const OWN_BUFFER: i32 = 0x40; // base is a malloc block of ours
const NO_BUFFER: i32 = 0x80; // base is charbuf and no output stays in it
const OPEN: i32 = 0x100;

/// The guest's FILE.
#[repr(C)]
pub struct Stream {
    // __crt_stdio_stream_data
    ptr: *mut u8,
    base: *mut u8,
    cnt: i32,
    flags: i32,
    file: i32,
    charbuf: i32,
    bufsiz: i32,
    tmpfname: *mut u8,
    // ours (the UCRT has a CRITICAL_SECTION here)
    lock: &'static RecursiveLock,
    claimed: AtomicBool,
    /// Text mode: the file offset the read-ahead came from.
    fill_position: i64,
    /// Half of a surrogate pair between two `fputwc`/`fgetwc` calls.
    put_high: u16,
    get_low: u16,
}

impl Stream {
    fn new() -> Self {
        Stream {
            ptr: std::ptr::null_mut(),
            base: std::ptr::null_mut(),
            cnt: 0,
            flags: 0,
            file: -1,
            charbuf: 0,
            bufsiz: 0,
            tmpfname: std::ptr::null_mut(),
            lock: Box::leak(Box::new(RecursiveLock::new())),
            claimed: AtomicBool::new(true),
            fill_position: -1,
            put_high: 0,
            get_low: 0,
        }
    }

    fn use_no_buffer(&mut self) {
        self.flags = (self.flags & !OWN_BUFFER) | NO_BUFFER;
        self.base = (&mut self.charbuf as *mut i32).cast();
        self.ptr = self.base;
        self.bufsiz = 2;
        self.cnt = 0;
    }

    fn allocate_buffer(&mut self) {
        // SAFETY: plain allocation.
        let block = unsafe { libc::malloc(BUFFER_SIZE as usize) }.cast::<u8>();
        if block.is_null() {
            self.use_no_buffer();
            return;
        }
        self.base = block;
        self.ptr = block;
        self.bufsiz = BUFFER_SIZE;
        self.cnt = 0;
        self.flags |= OWN_BUFFER;
    }

    fn pending(&self) -> usize {
        if self.base.is_null() { 0 } else { self.ptr as usize - self.base as usize }
    }

    /// Writes out a stream in write mode; false when not all of it went.
    fn flush(&mut self) -> bool {
        if self.flags & (READING | WRITING) != WRITING || self.base.is_null() {
            return true;
        }
        let pending = self.pending();
        self.ptr = self.base;
        self.cnt = 0;
        if self.flags & CAN_READ != 0 {
            self.flags &= !WRITING; // a "+" stream may be read after a flush
        }
        if pending == 0 || write(self.file, self.base, pending as u32) == pending as i32 {
            return true;
        }
        self.flags |= ERROR;
        false
    }

    /// The UCRT's `_filbuf`: the next buffer and its first byte.
    fn fill(&mut self) -> i32 {
        if self.flags & CAN_READ == 0 || self.flags & WRITING != 0 {
            if self.flags & CAN_READ == 0 {
                set_errno(EBADF);
            }
            self.flags |= ERROR;
            return EOF;
        }
        if self.base.is_null() {
            self.allocate_buffer();
        }
        self.flags |= READING;
        self.ptr = self.base;
        self.cnt = 0;
        if fd(self.file).is_some_and(|f| f.crlf) {
            self.fill_position = seek(self.file, 0, 1);
        }
        let n = read(self.file, self.base, self.bufsiz as u32);
        if n <= 0 {
            self.flags |= if n == 0 { AT_EOF } else { ERROR };
            return EOF;
        }
        self.cnt = n - 1;
        // SAFETY: n >= 1 bytes in the buffer.
        unsafe {
            let c = *self.ptr;
            self.ptr = self.ptr.add(1);
            i32::from(c)
        }
    }

    /// The UCRT's `_flsbuf`: makes room and stores one byte.
    fn flush_put(&mut self, c: i32) -> i32 {
        if self.flags & CAN_WRITE == 0 {
            set_errno(EBADF);
            self.flags |= ERROR;
            return EOF;
        }
        if self.flags & READING != 0 {
            // from reading to writing only at the end, or after a seek
            self.cnt = 0;
            if self.flags & AT_EOF == 0 {
                self.flags |= ERROR;
                return EOF;
            }
            self.ptr = self.base;
            self.flags &= !READING;
        }
        self.flags = (self.flags | WRITING) & !AT_EOF;
        self.cnt = 0;
        if self.base.is_null() {
            self.allocate_buffer();
        }
        let byte = c as u8;
        if self.flags & NO_BUFFER != 0 {
            if write(self.file, &byte, 1) == 1 {
                return i32::from(byte);
            }
            self.flags |= ERROR;
            return EOF;
        }
        let pending = self.pending();
        // SAFETY: bufsiz >= 2 bytes at base.
        unsafe {
            self.ptr = self.base.add(1);
        }
        self.cnt = self.bufsiz - 1;
        let ok = pending == 0 || write(self.file, self.base, pending as u32) == pending as i32;
        // SAFETY: base holds bufsiz bytes.
        unsafe { *self.base = byte };
        if ok {
            return i32::from(byte);
        }
        self.flags |= ERROR;
        EOF
    }

    fn get(&mut self) -> i32 {
        if self.flags & READING != 0 && self.cnt > 0 {
            self.cnt -= 1;
            // SAFETY: [ptr, ptr + cnt] is read-ahead.
            unsafe {
                let c = *self.ptr;
                self.ptr = self.ptr.add(1);
                return i32::from(c);
            }
        }
        self.fill()
    }

    fn put(&mut self, c: i32) -> i32 {
        if self.flags & WRITING != 0 && self.cnt > 0 {
            self.cnt -= 1;
            // SAFETY: cnt bytes of room behind ptr.
            unsafe {
                *self.ptr = c as u8;
                self.ptr = self.ptr.add(1);
            }
            return c & 0xFF;
        }
        self.flush_put(c)
    }

    fn unget(&mut self, c: i32) -> i32 {
        if c == EOF || self.flags & CAN_READ == 0 || self.flags & WRITING != 0 {
            return EOF;
        }
        if self.base.is_null() {
            self.allocate_buffer();
        }
        if self.ptr == self.base {
            if self.cnt != 0 {
                return EOF; // full, and nothing of it was read
            }
            // SAFETY: bufsiz >= 2.
            self.ptr = unsafe { self.ptr.add(1) };
        }
        // SAFETY: ptr > base.
        unsafe {
            self.ptr = self.ptr.sub(1);
            *self.ptr = c as u8;
        }
        self.cnt += 1;
        self.flags = (self.flags & !AT_EOF) | READING;
        c & 0xFF
    }

    /// The bytes taken; whole buffers go straight to the descriptor.
    fn write_bytes(&mut self, mut bytes: &[u8]) -> usize {
        let count = bytes.len();
        if self.flags & (NO_BUFFER | CAN_WRITE | READING) == NO_BUFFER | CAN_WRITE {
            self.flags = (self.flags | WRITING) & !AT_EOF;
        }
        while !bytes.is_empty() {
            if self.flags & WRITING != 0 && self.cnt > 0 {
                let n = bytes.len().min(self.cnt as usize);
                // SAFETY: cnt bytes of room behind ptr.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr, n);
                    self.ptr = self.ptr.add(n);
                }
                self.cnt -= n as i32;
                bytes = &bytes[n..];
            } else if self.flags & WRITING != 0
                && (self.flags & NO_BUFFER != 0 || bytes.len() >= self.bufsiz as usize)
            {
                if !self.flush() {
                    break;
                }
                self.flags |= WRITING;
                let mut n = bytes.len().min(i32::MAX as usize);
                if self.flags & NO_BUFFER == 0 {
                    n -= n % self.bufsiz as usize;
                }
                if write(self.file, bytes.as_ptr(), n as u32) != n as i32 {
                    self.flags |= ERROR;
                    break;
                }
                bytes = &bytes[n..];
            } else {
                if self.flush_put(i32::from(bytes[0])) == EOF {
                    break;
                }
                bytes = &bytes[1..];
            }
        }
        count - bytes.len()
    }

    fn read_bytes(&mut self, out: &mut [u8]) -> usize {
        let mut done = 0;
        while done < out.len() {
            let left = out.len() - done;
            if self.flags & READING != 0 && self.cnt > 0 {
                let n = left.min(self.cnt as usize);
                // SAFETY: n bytes of read-ahead at ptr.
                unsafe {
                    std::ptr::copy_nonoverlapping(self.ptr, out[done..].as_mut_ptr(), n);
                    self.ptr = self.ptr.add(n);
                }
                self.cnt -= n as i32;
                done += n;
            } else if self.flags & READING != 0 && self.flags & NO_BUFFER == 0 && left >= self.bufsiz as usize {
                let mut n = left.min(i32::MAX as usize);
                n -= n % self.bufsiz as usize;
                let got = read(self.file, out[done..].as_mut_ptr(), n as u32);
                self.ptr = self.base;
                self.fill_position = -1;
                if got <= 0 {
                    self.flags |= if got == 0 { AT_EOF } else { ERROR };
                    break;
                }
                done += got as usize;
            } else {
                let c = self.fill();
                if c == EOF {
                    break;
                }
                out[done] = c as u8;
                done += 1;
            }
        }
        done
    }

    /// The file position of the next byte read or written; -1 on failure.
    fn tell(&mut self) -> i64 {
        let crlf = fd(self.file).is_some_and(|f| f.crlf);
        let position = seek(self.file, 0, 1);
        if position < 0 {
            return -1;
        }
        if self.flags & WRITING != 0 {
            let pending = self.pending();
            if pending == 0 {
                return position;
            }
            let lines = if crlf {
                // SAFETY: [base, ptr) is unwritten output.
                unsafe { std::slice::from_raw_parts(self.base, pending) }.iter().filter(|&&b| b == b'\n').count()
            } else {
                0
            };
            return position + (pending + lines) as i64;
        }
        if self.flags & READING == 0 || self.cnt <= 0 {
            return position;
        }
        let consumed = self.pending() as i64;
        let raw_size = position - self.fill_position;
        if !crlf || self.fill_position < 0 || raw_size <= 0 {
            return position - i64::from(self.cnt);
        }
        // text: how many bytes of the file the consumed characters were
        let Some(entry) = fd(self.file) else { return -1 };
        let mut raw = vec![0u8; raw_size as usize];
        // SAFETY: a buffer of ours.
        let n = unsafe { libc::pread(entry.host, raw.as_mut_ptr().cast(), raw.len(), self.fill_position) };
        let n = n.max(0) as usize;
        let (mut at, mut characters) = (0usize, 0i64);
        while characters < consumed && at < n {
            at += if raw[at] == b'\r' && at + 1 < n && raw[at + 1] == b'\n' { 2 } else { 1 };
            characters += 1;
        }
        self.fill_position + at as i64
    }

    fn seek(&mut self, mut offset: i64, mut whence: i32) -> i32 {
        if !(0..=2).contains(&whence) {
            fail(EINVAL);
            return -1;
        }
        self.flags &= !AT_EOF;
        if whence == 1 {
            let at = self.tell();
            if at < 0 {
                return -1;
            }
            offset += at;
            whence = 0;
        }
        self.flush();
        self.ptr = self.base;
        self.cnt = 0;
        self.put_high = 0;
        self.get_low = 0;
        if self.flags & (CAN_READ | CAN_WRITE) == CAN_READ | CAN_WRITE {
            self.flags &= !(READING | WRITING);
        }
        if seek(self.file, offset, whence) < 0 { -1 } else { 0 }
    }

    fn close(&mut self) -> i32 {
        if self.flags & OPEN == 0 {
            fail(EINVAL);
            return EOF;
        }
        let mut result = if self.flush() { 0 } else { EOF };
        if self.flags & OWN_BUFFER != 0 {
            // SAFETY: our malloc block.
            unsafe { libc::free(self.base.cast()) };
        }
        if close(self.file) != 0 {
            result = EOF;
        }
        self.ptr = std::ptr::null_mut();
        self.base = std::ptr::null_mut();
        self.cnt = 0;
        self.flags = 0;
        self.bufsiz = 0;
        self.file = -1;
        self.fill_position = -1;
        self.put_high = 0;
        self.get_low = 0;
        result
    }

    fn is_text(&self) -> bool {
        fd(self.file).is_some_and(|f| f.text)
    }
}

/// Stream addresses, as integers: stdin, stdout, stderr, then every stream
/// `fopen` ever made.
static STREAMS: LazyLock<Mutex<Vec<usize>>> = LazyLock::new(|| {
    Mutex::new(
        (0..3)
            .map(|i| {
                let mut s = Stream::new();
                s.file = i;
                s.flags = OPEN | if i == 0 { CAN_READ } else { CAN_WRITE };
                let s = Box::leak(Box::new(s));
                if i != 0 {
                    s.use_no_buffer();
                }
                s as *mut Stream as usize
            })
            .collect(),
    )
});

fn streams() -> MutexGuard<'static, Vec<usize>> {
    STREAMS.lock().unwrap_or_else(|e| e.into_inner())
}

fn claim_stream() -> *mut Stream {
    let mut all = streams();
    for &s in all.iter().skip(3) {
        let s = s as *mut Stream;
        // SAFETY: a leaked stream of ours; `claimed` is atomic.
        if !unsafe { &(*s).claimed }.swap(true, Ordering::AcqRel) {
            return s;
        }
    }
    let s = Box::leak(Box::new(Stream::new())) as *mut Stream;
    all.push(s as usize);
    s
}

/// Runs `f` on the stream under its lock.
fn with<R>(s: *mut Stream, f: impl FnOnce(&mut Stream) -> R) -> Option<R> {
    if s.is_null() {
        return None;
    }
    // SAFETY: a FILE* of ours; the lock is a leaked field read once.
    let lock = unsafe { (*s).lock };
    let _guard = lock.guard();
    // SAFETY: as above; under its lock.
    Some(f(unsafe { &mut *s }))
}

/// `fflush(NULL)` and what exit owes the title.
pub(crate) fn flush_all() -> i32 {
    let all: Vec<usize> = streams().iter().skip(1).copied().collect(); // not stdin
    let mut result = 0;
    for s in all {
        if with(s as *mut Stream, |s| s.flush()) == Some(false) {
            result = EOF;
        }
    }
    result
}

pub(crate) fn lock_stream(s: *mut Stream) {
    if !s.is_null() {
        // SAFETY: a FILE* of ours.
        unsafe { (*s).lock }.lock();
    }
}

pub(crate) fn unlock_stream(s: *mut Stream) {
    if !s.is_null() {
        // SAFETY: a FILE* of ours.
        unsafe { (*s).lock }.unlock();
    }
}

/// The UCRT's mode string: r, w or a, then `+ b t c n S R T D N x` in any
/// order, each kind once. `ccs=` and anything else are refused.
fn parse_mode(mode: &[u8]) -> Option<(i32, i32)> {
    let mut mode = mode.iter().skip_while(|&&c| c == b' ');
    let (mut oflag, mut flags) = match mode.next()? {
        b'r' => (0, CAN_READ),
        b'w' => (O_WRONLY | O_CREAT | O_TRUNC, CAN_WRITE),
        b'a' => (O_WRONLY | O_CREAT | O_APPEND, CAN_WRITE),
        _ => return None,
    };
    let mut seen = 0u32;
    for &c in mode {
        let kind = match c {
            b' ' => continue,
            b'+' => {
                oflag = (oflag & !3) | O_RDWR;
                flags = CAN_READ | CAN_WRITE;
                1
            }
            b'b' => {
                oflag |= O_BINARY;
                2
            }
            b't' => {
                oflag |= O_TEXT;
                2
            }
            b'c' | b'n' => 4,
            b'S' | b'R' => 8,
            b'T' => 16,
            b'D' => {
                oflag |= O_TEMPORARY;
                32
            }
            b'N' => 64,
            b'x' if oflag & O_TRUNC != 0 => {
                oflag |= O_EXCL;
                128
            }
            _ => return None,
        };
        if seen & kind != 0 {
            return None;
        }
        seen |= kind;
    }
    Some((oflag, flags))
}

fn open_stream(path: Option<std::path::PathBuf>, mode: Option<Vec<u8>>, share: i32) -> *mut Stream {
    let (Some(path), Some(mode)) = (path, mode) else {
        invalid(EINVAL);
        return std::ptr::null_mut();
    };
    let Some((oflag, flags)) = parse_mode(&mode).filter(|_| matches!(share & !0x80, 0x10 | 0x20 | 0x30 | 0x40))
    else {
        invalid(EINVAL);
        return std::ptr::null_mut();
    };
    let file = open(&path, oflag, 0x180);
    if file < 0 {
        return std::ptr::null_mut();
    }
    let s = claim_stream();
    if s.is_null() {
        close(file);
        fail(EMFILE);
        return s;
    }
    with(s, |s| {
        s.file = file;
        s.flags = flags | OPEN;
    });
    s
}

fn narrow_mode(mode: *const c_char) -> Option<Vec<u8>> {
    // SAFETY: null or NUL-terminated, per the C contract.
    (!mode.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(mode) }.to_bytes().to_vec())
}

fn wide_mode(mode: *const u16) -> Option<Vec<u8>> {
    // SAFETY: null or NUL-terminated, per the C contract.
    (!mode.is_null()).then(|| unsafe { super::ucrt_string::units(mode) }.iter().map(|&c| c.min(0xFF) as u8).collect())
}

const SH_DENYNO: i32 = 0x40;

pub extern "win64" fn __acrt_iob_func(index: u32) -> *mut Stream {
    streams().get(index as usize).filter(|_| index < 3).map_or(std::ptr::null_mut(), |&s| s as *mut Stream)
}

pub extern "win64" fn fopen(path: *const c_char, mode: *const c_char) -> *mut Stream {
    open_stream(narrow_path(path), narrow_mode(mode), SH_DENYNO)
}

pub extern "win64" fn _fsopen(path: *const c_char, mode: *const c_char, share: i32) -> *mut Stream {
    open_stream(narrow_path(path), narrow_mode(mode), share)
}

pub extern "win64" fn _wfopen(path: *const u16, mode: *const u16) -> *mut Stream {
    open_stream(wide_path(path), wide_mode(mode), SH_DENYNO)
}

pub extern "win64" fn _wfsopen(path: *const u16, mode: *const u16, share: i32) -> *mut Stream {
    open_stream(wide_path(path), wide_mode(mode), share)
}

pub extern "win64" fn fclose(s: *mut Stream) -> i32 {
    let is_standard = streams().iter().take(3).any(|&t| t == s as usize);
    match with(s, |s| s.close()) {
        Some(result) => {
            if !is_standard {
                // SAFETY: a stream of ours; fopen may hand it out again.
                unsafe { &(*s).claimed }.store(false, Ordering::Release);
            }
            result
        }
        None => invalid(EINVAL).min(EOF),
    }
}

fn total(size: usize, count: usize, buffer: bool, s: *mut Stream) -> Option<usize> {
    if size == 0 || count == 0 {
        return Some(0);
    }
    let total = size.checked_mul(count).filter(|_| buffer && !s.is_null());
    if total.is_none() {
        invalid(EINVAL);
    }
    total
}

pub extern "win64" fn fread(buffer: *mut u8, size: usize, count: usize, s: *mut Stream) -> usize {
    match total(size, count, !buffer.is_null(), s) {
        Some(0) | None => 0,
        // SAFETY: the caller's buffer of size * count bytes.
        Some(n) => with(s, |s| s.read_bytes(unsafe { std::slice::from_raw_parts_mut(buffer, n) })).unwrap_or(0) / size,
    }
}

pub extern "win64" fn fwrite(buffer: *const u8, size: usize, count: usize, s: *mut Stream) -> usize {
    match total(size, count, !buffer.is_null(), s) {
        Some(0) | None => 0,
        // SAFETY: the caller's buffer of size * count bytes.
        Some(n) => with(s, |s| s.write_bytes(unsafe { std::slice::from_raw_parts(buffer, n) })).unwrap_or(0) / size,
    }
}

pub extern "win64" fn fgetc(s: *mut Stream) -> i32 {
    with(s, Stream::get).unwrap_or_else(|| invalid(EINVAL).min(EOF))
}

pub extern "win64" fn fputc(c: i32, s: *mut Stream) -> i32 {
    with(s, |s| s.put(c)).unwrap_or_else(|| invalid(EINVAL).min(EOF))
}

pub extern "win64" fn fputs(text: *const c_char, s: *mut Stream) -> i32 {
    if text.is_null() {
        return invalid(EINVAL).min(EOF);
    }
    // SAFETY: NUL-terminated, per the C contract.
    let bytes = unsafe { std::ffi::CStr::from_ptr(text) }.to_bytes();
    with(s, |s| if s.write_bytes(bytes) == bytes.len() { 0 } else { EOF }).unwrap_or_else(|| invalid(EINVAL).min(EOF))
}

pub extern "win64" fn ungetc(c: i32, s: *mut Stream) -> i32 {
    with(s, |s| s.unget(c)).unwrap_or_else(|| invalid(EINVAL).min(EOF))
}

pub extern "win64" fn fputwc(c: u16, s: *mut Stream) -> u16 {
    let Some(r) = with(s, |s| {
        if !s.is_text() {
            return if s.write_bytes(&c.to_le_bytes()) == 2 { c } else { WEOF };
        }
        if (0xD800..0xDC00).contains(&c) {
            s.put_high = c; // written with the second half
            return c;
        }
        let mut units = vec![];
        if s.put_high != 0 && (0xDC00..0xE000).contains(&c) {
            units.push(s.put_high);
        }
        s.put_high = 0;
        units.push(c);
        let bytes = String::from_utf16_lossy(&units).into_bytes();
        if s.write_bytes(&bytes) == bytes.len() { c } else { WEOF }
    }) else {
        invalid(EINVAL);
        return WEOF;
    };
    r
}

pub extern "win64" fn fgetwc(s: *mut Stream) -> u16 {
    let Some(r) = with(s, |s| {
        if !s.is_text() {
            let low = s.get();
            let high = if low == EOF { EOF } else { s.get() };
            return if high == EOF { WEOF } else { (low | (high << 8)) as u16 };
        }
        if s.get_low != 0 {
            return std::mem::take(&mut s.get_low);
        }
        let lead = s.get();
        if lead == EOF {
            return WEOF;
        }
        if lead < 0x80 {
            return lead as u16;
        }
        let more = match lead {
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF7 => 3,
            _ => 0,
        };
        let mut bytes = vec![lead as u8];
        for _ in 0..more {
            let next = s.get();
            if next == EOF {
                break;
            }
            if next & 0xC0 != 0x80 {
                s.unget(next);
                break;
            }
            bytes.push(next as u8);
        }
        let units: Vec<u16> = String::from_utf8_lossy(&bytes).encode_utf16().collect();
        if units.len() > 1 {
            s.get_low = units[1];
        }
        units[0]
    }) else {
        invalid(EINVAL);
        return WEOF;
    };
    r
}

pub extern "win64" fn ungetwc(c: u16, s: *mut Stream) -> u16 {
    if c == WEOF {
        return WEOF;
    }
    with(s, |s| {
        let bytes = if s.is_text() { String::from_utf16_lossy(&[c]).into_bytes() } else { c.to_le_bytes().to_vec() };
        for &b in bytes.iter().rev() {
            if s.unget(i32::from(b)) == EOF {
                return WEOF;
            }
        }
        c
    })
    .unwrap_or(WEOF)
}

pub extern "win64" fn fflush(s: *mut Stream) -> i32 {
    if s.is_null() {
        return flush_all();
    }
    with(s, |s| if s.flush() { 0 } else { EOF }).unwrap_or(EOF)
}

pub extern "win64" fn _fseeki64(s: *mut Stream, offset: i64, whence: i32) -> i32 {
    with(s, |s| s.seek(offset, whence)).unwrap_or_else(|| invalid(EINVAL).min(-1))
}

pub extern "win64" fn fseek(s: *mut Stream, offset: i32, whence: i32) -> i32 {
    _fseeki64(s, offset.into(), whence)
}

pub extern "win64" fn _ftelli64(s: *mut Stream) -> i64 {
    with(s, Stream::tell).unwrap_or_else(|| i64::from(invalid(EINVAL).min(-1)))
}

pub extern "win64" fn ftell(s: *mut Stream) -> i32 {
    let at = _ftelli64(s);
    if at > i64::from(i32::MAX) {
        fail(EINVAL);
        return -1;
    }
    at as i32
}

/// `fpos_t` is a 64-bit position.
pub extern "win64" fn fgetpos(s: *mut Stream, position: *mut i64) -> i32 {
    if s.is_null() || position.is_null() {
        return invalid(EINVAL);
    }
    let at = _ftelli64(s);
    // SAFETY: checked for null; the title's fpos_t.
    unsafe { *position = at };
    if at < 0 { -1 } else { 0 }
}

pub extern "win64" fn fsetpos(s: *mut Stream, position: *const i64) -> i32 {
    if s.is_null() || position.is_null() {
        return invalid(EINVAL);
    }
    // SAFETY: checked for null.
    _fseeki64(s, unsafe { *position }, 0)
}

pub extern "win64" fn rewind(s: *mut Stream) {
    if with(s, |s| {
        s.seek(0, 0);
        s.flags &= !(ERROR | AT_EOF);
    })
    .is_none()
    {
        invalid(EINVAL);
    }
}

pub extern "win64" fn ferror(s: *mut Stream) -> i32 {
    with(s, |s| s.flags & ERROR).unwrap_or_else(|| {
        invalid(EINVAL);
        0
    })
}

/// `_IOFBF` 0, `_IOLBF` 0x40 (fully buffered too, as in the UCRT),
/// `_IONBF` 4. The size is rounded down to an even number.
pub extern "win64" fn setvbuf(s: *mut Stream, buffer: *mut u8, mode: i32, size: usize) -> i32 {
    if s.is_null() || !matches!(mode, 0 | 0x40 | 4) || (mode != 4 && !(2..=i32::MAX as usize).contains(&size)) {
        return invalid(EINVAL).min(-1);
    }
    with(s, |s| {
        s.flush();
        if s.flags & OWN_BUFFER != 0 {
            // SAFETY: our malloc block.
            unsafe { libc::free(s.base.cast()) };
        }
        s.flags &= !(OWN_BUFFER | NO_BUFFER);
        s.cnt = 0;
        let size = size & !1;
        if mode == 4 {
            s.use_no_buffer();
            return 0;
        }
        let block = if buffer.is_null() {
            // SAFETY: plain allocation.
            let block = unsafe { libc::malloc(size) }.cast::<u8>();
            if block.is_null() {
                s.base = std::ptr::null_mut();
                s.ptr = s.base;
                s.bufsiz = 0;
                fail(ENOMEM);
                return -1;
            }
            s.flags |= OWN_BUFFER;
            block
        } else {
            buffer
        };
        s.base = block;
        s.ptr = block;
        s.bufsiz = size as i32;
        0
    })
    .unwrap_or(-1)
}

pub extern "win64" fn _get_stream_buffer_pointers(
    s: *mut Stream,
    base: *mut *mut *mut u8,
    ptr: *mut *mut *mut u8,
    count: *mut *mut i32,
) -> i32 {
    if s.is_null() {
        return invalid(EINVAL);
    }
    // SAFETY: a FILE* of ours; the out pointers are null or writable.
    unsafe {
        if !base.is_null() {
            *base = &raw mut (*s).base;
        }
        if !ptr.is_null() {
            *ptr = &raw mut (*s).ptr;
        }
        if !count.is_null() {
            *count = &raw mut (*s).cnt;
        }
    }
    0
}

pub extern "win64" fn _set_fmode(mode: i32) -> i32 {
    if !matches!(mode, O_TEXT | O_BINARY | 0x10000) {
        return invalid(EINVAL);
    }
    FMODE.store(mode, Ordering::Relaxed);
    0
}

pub extern "win64" fn __p__commode() -> *mut i32 {
    COMMODE.as_ptr()
}

/// (options, stream, format, locale, va_list): the characters written.
pub extern "win64" fn __stdio_common_vfprintf(
    options: u64,
    s: *mut Stream,
    format: *const u8,
    _locale: usize,
    args: *const u64,
) -> i32 {
    if s.is_null() {
        return invalid(EINVAL).min(-1);
    }
    let Some(text) = to_vec(options, format, args) else { return -1 };
    with(s, |s| if s.write_bytes(&text) == text.len() { text.len() as i32 } else { -1 }).unwrap_or(-1)
}

pub extern "win64" fn __stdio_common_vfwprintf(
    options: u64,
    s: *mut Stream,
    format: *const u16,
    _locale: usize,
    args: *const u64,
) -> i32 {
    if s.is_null() {
        return invalid(EINVAL).min(-1);
    }
    let Some(text) = to_vec(options, format, args) else { return -1 };
    with(s, |s| {
        let bytes = if s.is_text() {
            String::from_utf16_lossy(&text).into_bytes()
        } else {
            text.iter().flat_map(|u| u.to_le_bytes()).collect()
        };
        if s.write_bytes(&bytes) == bytes.len() { text.len() as i32 } else { -1 }
    })
    .unwrap_or(-1)
}

pub(crate) fn module() -> HleModule {
    use super::ucrt_format::*;
    use super::ucrt_scan::*;
    HleModule {
        name: "ucrt-stdio",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-stdio-*"],
        exports: vec![
            hle_export!(_open),
            hle_export!(_wopen),
            hle_export!(_close),
            hle_export!(_read),
            hle_export!(_write),
            hle_export!(_lseeki64),
            hle_export!(__acrt_iob_func),
            hle_export!(fopen),
            hle_export!(_fsopen),
            hle_export!(_wfopen),
            hle_export!(_wfsopen),
            hle_export!(fclose),
            hle_export!(fread),
            hle_export!(fwrite),
            hle_export!(fgetc),
            hle_export!(fputc),
            hle_export!(fputs),
            hle_export!(ungetc),
            hle_export!(fputwc),
            hle_export!(fgetwc),
            hle_export!(ungetwc),
            hle_export!(fflush),
            hle_export!(_fseeki64),
            hle_export!(fseek),
            hle_export!(_ftelli64),
            hle_export!(ftell),
            hle_export!(fgetpos),
            hle_export!(fsetpos),
            hle_export!(rewind),
            hle_export!(ferror),
            hle_export!(setvbuf),
            hle_export!(_get_stream_buffer_pointers),
            hle_export!(_set_fmode),
            hle_export!(__p__commode),
            hle_export!(__stdio_common_vfprintf),
            hle_export!(__stdio_common_vfwprintf),
            hle_export!(__stdio_common_vsprintf),
            hle_export!(__stdio_common_vswprintf),
            hle_export!(__stdio_common_vsprintf_s),
            hle_export!(__stdio_common_vswprintf_s),
            hle_export!(__stdio_common_vsnprintf_s),
            hle_export!(__stdio_common_vsnwprintf_s),
            hle_export!(__stdio_common_vsscanf),
            hle_export!(__stdio_common_vswscanf),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_runtime::{ENOENT, errno};

    fn path(dir: &tempfile::TempDir, name: &str) -> CString {
        CString::new(dir.path().join(name).to_str().unwrap()).unwrap()
    }

    #[test]
    fn layout_is_the_ucrts() {
        assert_eq!(std::mem::offset_of!(Stream, cnt), 16);
        assert_eq!(std::mem::offset_of!(Stream, file), 24);
        assert_eq!(std::mem::offset_of!(Stream, tmpfname), 40);
        assert_eq!(std::mem::offset_of!(Stream, lock), 48);
    }

    #[test]
    fn text_mode_translates_line_ends() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir, "t.txt");
        let f = fopen(p.as_ptr(), c"w".as_ptr());
        assert!(!f.is_null());
        assert_eq!(fputs(c"a\nb\n".as_ptr(), f), 0);
        assert_eq!(_ftelli64(f), 6);
        assert_eq!(fclose(f), 0);
        assert_eq!(std::fs::read(dir.path().join("t.txt")).unwrap(), b"a\r\nb\r\n");

        let f = fopen(p.as_ptr(), c"rt".as_ptr());
        assert_eq!(fgetc(f), b'a' as i32);
        assert_eq!(_ftelli64(f), 1);
        assert_eq!(fgetc(f), b'\n' as i32);
        assert_eq!(_ftelli64(f), 3); // the file's bytes, CR included
        let mut rest = [0u8; 8];
        assert_eq!(fread(rest.as_mut_ptr(), 1, 8, f), 2);
        assert_eq!(&rest[..2], b"b\n");
        assert_eq!(fgetc(f), EOF);
        assert_eq!(fseek(f, 0, 0), 0);
        assert_eq!(fgetc(f), b'a' as i32);
        assert_eq!(fclose(f), 0);

        let f = fopen(p.as_ptr(), c"rb".as_ptr());
        let mut raw = [0u8; 8];
        assert_eq!(fread(raw.as_mut_ptr(), 1, 8, f), 6);
        assert_eq!(&raw[..3], b"a\r\n");
        assert_eq!(fclose(f), 0);
        assert_eq!(fclose(f), EOF); // a closed stream stays a stream
    }

    #[test]
    fn binary_streams_seek_unget_and_wide() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir, "b.bin");
        let f = fopen(p.as_ptr(), c"w+b".as_ptr());
        let data: Vec<u8> = (0..10000u32).map(|i| i as u8).collect();
        assert_eq!(fwrite(data.as_ptr(), 1, data.len(), f), data.len());
        assert_eq!(_ftelli64(f), 10000);
        assert_eq!(_fseeki64(f, 5000, 0), 0);
        let mut back = vec![0u8; 5000];
        assert_eq!(fread(back.as_mut_ptr(), 1, 5000, f), 5000);
        assert_eq!(back, data[5000..]);
        assert_eq!(fgetc(f), EOF);
        assert_eq!(fseek(f, -2, 2), 0);
        assert_eq!(fgetc(f), data[9998] as i32);
        assert_eq!(ungetc(b'Z' as i32, f), b'Z' as i32);
        assert_eq!(fgetc(f), b'Z' as i32);
        let mut pos = 0;
        assert_eq!(fgetpos(f, &mut pos), 0);
        assert_eq!(pos, 9999);
        rewind(f);
        assert_eq!(fgetc(f), 0);
        assert_eq!(fseek(f, 0, 2), 0);
        assert_eq!(fputwc(0x4E2D, f), 0x4E2D);
        assert_eq!(fseek(f, -2, 2), 0);
        assert_eq!(fgetwc(f), 0x4E2D);
        assert_eq!(fclose(f), 0);

        let t = path(&dir, "w.txt");
        let f = fopen(t.as_ptr(), c"w".as_ptr());
        assert_eq!(fputwc(0xD83D, f), 0xD83D);
        assert_eq!(fputwc(0xDE00, f), 0xDE00);
        assert_eq!(fclose(f), 0);
        assert_eq!(std::fs::read_to_string(dir.path().join("w.txt")).unwrap(), "\u{1F600}");
        let f = fopen(t.as_ptr(), c"r".as_ptr());
        assert_eq!((fgetwc(f), fgetwc(f), fgetwc(f)), (0xD83D, 0xDE00, WEOF));
        assert_eq!(fclose(f), 0);
    }

    #[test]
    fn buffer_pointers_are_the_filebufs_view() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.bin"), b"xyz").unwrap();
        let p = path(&dir, "r.bin");
        let f = fopen(p.as_ptr(), c"rb".as_ptr());
        let (mut base, mut ptr, mut cnt) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(_get_stream_buffer_pointers(f, &mut base, &mut ptr, &mut cnt), 0);
        assert_eq!(fgetc(f), b'x' as i32);
        // SAFETY: the stream's own fields.
        unsafe {
            assert_eq!(*cnt, 2);
            assert_eq!(**ptr, b'y');
            *ptr = (*ptr).add(1); // what basic_filebuf does
            *cnt -= 1;
            assert_eq!(**base, b'x');
        }
        assert_eq!(fgetc(f), b'z' as i32);
        assert_eq!(fclose(f), 0);
    }

    #[test]
    fn modes_fds_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = path(&dir, "none");
        assert!(fopen(missing.as_ptr(), c"r".as_ptr()).is_null());
        assert_eq!(errno(), ENOENT);
        assert!(fopen(missing.as_ptr(), c"q".as_ptr()).is_null());
        assert_eq!(errno(), EINVAL);
        assert!(fopen(missing.as_ptr(), c"w,ccs=UTF-8".as_ptr()).is_null());
        assert!(fopen(missing.as_ptr(), c"rbb".as_ptr()).is_null());
        let p = path(&dir, "fd.bin");
        let fd = _open(p.as_ptr(), O_CREAT | O_RDWR | O_BINARY, 0x180);
        assert!(fd > 2);
        assert_eq!(_write(fd, b"hello".as_ptr(), 5), 5);
        assert_eq!(_lseeki64(fd, 1, 0), 1);
        let mut buf = [0u8; 8];
        assert_eq!(_read(fd, buf.as_mut_ptr(), 8), 4);
        assert_eq!(&buf[..4], b"ello");
        assert_eq!(_close(fd), 0);
        assert_eq!(_close(fd), -1);
        assert_eq!(errno(), EBADF);
        let wide: Vec<u16> = dir.path().join("fd.bin").to_str().unwrap().replace('/', "\\").encode_utf16().chain([0]).collect();
        let f = _wfopen(wide.as_ptr(), [b'r' as u16, 0].as_ptr());
        assert!(!f.is_null(), "a path spelled with backslashes");
        assert_eq!(fclose(f), 0);
    }

    #[test]
    fn formatted_output_reaches_the_stream() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir, "o.txt");
        let f = fopen(p.as_ptr(), c"wb".as_ptr());
        let args = [42u64, c"x".as_ptr() as u64];
        assert_eq!(__stdio_common_vfprintf(0, f, c"%d %s".as_ptr().cast(), 0, args.as_ptr()), 4);
        let wf: Vec<u16> = "|%d".encode_utf16().chain([0]).collect();
        assert_eq!(__stdio_common_vfwprintf(0, f, wf.as_ptr(), 0, args.as_ptr()), 3);
        assert_eq!(fclose(f), 0);
        assert_eq!(std::fs::read(dir.path().join("o.txt")).unwrap(), b"42 x|\x004\x002\x00");
        assert_eq!(fflush(std::ptr::null_mut()), 0);
        assert!(!__acrt_iob_func(2).is_null() && __acrt_iob_func(3).is_null());
    }
}
