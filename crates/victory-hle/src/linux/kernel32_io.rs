//! kernel32 I/O completion: `OVERLAPPED` results, cancellation and I/O
//! completion ports. Every transfer completes inside `ReadFile` or
//! `WriteFile` (`kernel32_file.rs`), so nothing is ever in flight here: a
//! result is ready once the call returned, and a port holds the packets
//! of finished transfers and `PostQueuedCompletionStatus`.

#![allow(non_snake_case)] // named like the exports

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use victory_loader::linux::teb::set_last_error;

use super::handles::{self, Handle, Object};
use super::kernel32_file::{
    self as file, ERROR_GEN_FAILURE, ERROR_HANDLE_EOF, ERROR_INVALID_HANDLE,
    ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE, STATUS_END_OF_FILE, STATUS_PENDING, fail,
};
use crate::HleModule;
use crate::hle_export;

const WAIT_TIMEOUT: u32 = 258;
const ERROR_OPERATION_ABORTED: u32 = 995;
const ERROR_IO_INCOMPLETE: u32 = 996;
const ERROR_NOT_FOUND: u32 = 1168;
const STATUS_CANCELLED: u32 = 0xC000_0120;
const INFINITE: u32 = u32::MAX;

/// `OVERLAPPED`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct Overlapped {
    /// The NTSTATUS of the operation.
    pub internal: usize,
    /// Bytes transferred.
    pub internal_high: usize,
    pub offset_low: u32,
    pub offset_high: u32,
    /// An event handle; its low bit set means "no completion packet".
    pub event: usize,
}

impl Overlapped {
    pub fn offset(&self) -> u64 {
        u64::from(self.offset_high) << 32 | u64::from(self.offset_low)
    }
}

/// `OVERLAPPED_ENTRY`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct OverlappedEntry {
    pub key: usize,
    pub overlapped: usize,
    pub internal: usize,
    pub bytes: u32,
}

#[derive(Debug, Clone, Copy)]
struct Packet {
    bytes: u32,
    key: usize,
    overlapped: usize,
    status: u32,
}

/// An I/O completion port: a queue of packets threads wait on.
#[derive(Default)]
pub(crate) struct Port {
    queue: Mutex<VecDeque<Packet>>,
    ready: Condvar,
}

impl Object for Port {}

impl Port {
    pub(crate) fn post(&self, bytes: u32, key: usize, overlapped: usize, status: u32) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.push_back(Packet {
            bytes,
            key,
            overlapped,
            status,
        });
        self.ready.notify_one();
    }

    /// Up to `max` packets, waiting at most `timeout_ms` for the first.
    fn take(&self, max: usize, timeout_ms: u32) -> Vec<Packet> {
        let deadline = (timeout_ms != INFINITE)
            .then(|| Instant::now() + Duration::from_millis(timeout_ms.into()));
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        while queue.is_empty() {
            match deadline {
                None => queue = self.ready.wait(queue).unwrap_or_else(|e| e.into_inner()),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Vec::new();
                    }
                    queue = self
                        .ready
                        .wait_timeout(queue, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
        let n = max.min(queue.len());
        queue.drain(..n).collect()
    }
}

fn port(handle: *mut c_void) -> Option<Arc<Port>> {
    let port = handles::get::<Port>(handle as Handle);
    if port.is_none() {
        set_last_error(ERROR_INVALID_HANDLE);
    }
    port
}

/// The Win32 code of an operation's NTSTATUS.
fn status_error(status: u32) -> u32 {
    match status {
        0 => 0,
        STATUS_END_OF_FILE => ERROR_HANDLE_EOF,
        STATUS_CANCELLED => ERROR_OPERATION_ABORTED,
        _ => ERROR_GEN_FAILURE,
    }
}

/// A new port (`file` `INVALID_HANDLE_VALUE`), or `file` associated with
/// `existing` (or a new port when that is null) under `key`.
pub extern "win64" fn CreateIoCompletionPort(
    file: *mut c_void,
    existing: *mut c_void,
    key: usize,
    _threads: u32,
) -> *mut c_void {
    let (handle, port) = if existing.is_null() {
        let port = Arc::new(Port::default());
        (handles::insert(port.clone()), port)
    } else {
        match self::port(existing) {
            Some(port) => (existing as Handle, port),
            None => return std::ptr::null_mut(),
        }
    };
    if file != INVALID_HANDLE_VALUE {
        let Some(file) = file::file(file) else {
            if existing.is_null() {
                handles::close(handle);
            }
            return std::ptr::null_mut();
        };
        *file.port.lock().unwrap_or_else(|e| e.into_inner()) = Some((port, key));
    } else if !existing.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    handle as *mut c_void
}

pub extern "win64" fn PostQueuedCompletionStatus(
    port: *mut c_void,
    bytes: u32,
    key: usize,
    overlapped: *mut Overlapped,
) -> i32 {
    let Some(port) = self::port(port) else {
        return 0;
    };
    port.post(bytes, key, overlapped as usize, 0);
    1
}

pub extern "win64" fn GetQueuedCompletionStatus(
    port: *mut c_void,
    bytes: *mut u32,
    key: *mut usize,
    overlapped: *mut *mut Overlapped,
    timeout: u32,
) -> i32 {
    if bytes.is_null() || key.is_null() || overlapped.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    let Some(port) = self::port(port) else {
        return 0;
    };
    let Some(packet) = port.take(1, timeout).pop() else {
        // SAFETY: the caller's out pointer.
        unsafe { overlapped.write(std::ptr::null_mut()) };
        return fail(WAIT_TIMEOUT);
    };
    // SAFETY: the caller's out pointers, checked non-null.
    unsafe {
        bytes.write(packet.bytes);
        key.write(packet.key);
        overlapped.write(packet.overlapped as *mut Overlapped);
    }
    match status_error(packet.status) {
        0 => 1,
        e => fail(e),
    }
}

pub extern "win64" fn GetQueuedCompletionStatusEx(
    port: *mut c_void,
    entries: *mut OverlappedEntry,
    count: u32,
    removed: *mut u32,
    timeout: u32,
    _alertable: i32,
) -> i32 {
    if entries.is_null() || count == 0 || removed.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    let Some(port) = self::port(port) else {
        return 0;
    };
    let packets = port.take(count as usize, timeout);
    // SAFETY: the caller's out pointer.
    unsafe { removed.write(packets.len() as u32) };
    if packets.is_empty() {
        return fail(WAIT_TIMEOUT);
    }
    for (i, p) in packets.iter().enumerate() {
        let entry = OverlappedEntry {
            key: p.key,
            overlapped: p.overlapped,
            internal: p.status as usize,
            bytes: p.bytes,
        };
        // SAFETY: the caller's array of `count` entries.
        unsafe { entries.add(i).write_unaligned(entry) };
    }
    1
}

fn result(overlapped: *const Overlapped, bytes: *mut u32) -> i32 {
    // SAFETY: null or the caller's OVERLAPPED.
    let Some(ov) = (unsafe { overlapped.as_ref() }) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let status = ov.internal as u32;
    if status == STATUS_PENDING {
        // Nothing is in flight here, so a pending block was never started.
        return fail(ERROR_IO_INCOMPLETE);
    }
    if !bytes.is_null() {
        // SAFETY: the caller's out pointer.
        unsafe { bytes.write(ov.internal_high as u32) };
    }
    match status_error(status) {
        0 => 1,
        e => fail(e),
    }
}

pub extern "win64" fn GetOverlappedResult(
    file: *mut c_void,
    overlapped: *const Overlapped,
    bytes: *mut u32,
    _wait: i32,
) -> i32 {
    if file::file(file).is_none() {
        return 0;
    }
    result(overlapped, bytes)
}

pub extern "win64" fn GetOverlappedResultEx(
    file: *mut c_void,
    overlapped: *const Overlapped,
    bytes: *mut u32,
    _timeout: u32,
    _alertable: i32,
) -> i32 {
    GetOverlappedResult(file, overlapped, bytes, 0)
}

/// Nothing to cancel, which is success.
pub extern "win64" fn CancelIo(file: *mut c_void) -> i32 {
    i32::from(file::file(file).is_some())
}

/// Nothing is in flight: `ERROR_NOT_FOUND`, as Windows answers then.
pub extern "win64" fn CancelIoEx(file: *mut c_void, _overlapped: *const Overlapped) -> i32 {
    if file::file(file).is_none() {
        return 0;
    }
    fail(ERROR_NOT_FOUND)
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-io",
        dlls: &["kernel32.dll", "kernelbase.dll", "api-ms-win-core-io-*"],
        exports: vec![
            hle_export!(CreateIoCompletionPort),
            hle_export!(PostQueuedCompletionStatus),
            hle_export!(GetQueuedCompletionStatus),
            hle_export!(GetQueuedCompletionStatusEx),
            hle_export!(GetOverlappedResult),
            hle_export!(GetOverlappedResultEx),
            hle_export!(CancelIo),
            hle_export!(CancelIoEx),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::kernel32_file::tests::w;
    use crate::linux::kernel32_file::{
        CREATE_ALWAYS, CreateFileW, FILE_FLAG_OVERLAPPED, GENERIC_READ, GENERIC_WRITE, ReadFile,
        WriteFile, guest_path,
    };
    use crate::linux::handles::Event;
    use crate::linux::kernel32_wait::WaitForSingleObject;
    use victory_loader::linux::teb::last_error;

    /// Whether the event behind `h` is set, without taking it.
    fn is_set(h: usize) -> bool {
        WaitForSingleObject(h, 0) == 0
    }

    /// With `VICTORY_GAME_DIR`: a file of the dump, named in upper case,
    /// reads through an overlapped handle what the host reads.
    #[test]
    fn overlapped_reads_of_the_game_dump() {
        use crate::linux::kernel32_path::{FindClose, FindDataW, FindFirstFileW, FindNextFileW};
        let Some(dir) = std::env::var_os("VICTORY_GAME_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        // SAFETY: an all-zero WIN32_FIND_DATAW is valid.
        let mut fd: FindDataW = unsafe { std::mem::zeroed() };
        let find = FindFirstFileW(w(&format!("{}\\*", guest_path(&dir))).as_ptr(), &mut fd);
        assert_ne!(find, INVALID_HANDLE_VALUE);
        let name = loop {
            let n = fd.name.iter().position(|&c| c == 0).unwrap();
            let name = String::from_utf16_lossy(&fd.name[..n]);
            if fd.attributes & 0x10 == 0
                && (u64::from(fd.size_high) << 32 | u64::from(fd.size_low)) > 0
            {
                break name;
            }
            assert_eq!(
                FindNextFileW(find, &mut fd),
                1,
                "no file in the dump's folder"
            );
        };
        FindClose(find);
        let size = u64::from(fd.size_high) << 32 | u64::from(fd.size_low);
        let upper = w(&format!("{}\\{}", guest_path(&dir), name.to_uppercase()));
        let flags = FILE_FLAG_OVERLAPPED;
        let h = CreateFileW(
            upper.as_ptr(),
            GENERIC_READ,
            1,
            std::ptr::null(),
            3,
            flags,
            std::ptr::null_mut(),
        );
        assert_ne!(h, INVALID_HANDLE_VALUE, "{name}");
        let at = size / 2;
        let mut ov = Overlapped {
            offset_low: at as u32,
            offset_high: (at >> 32) as u32,
            ..Default::default()
        };
        let mut buf = vec![0u8; 1 << 16];
        let mut n = 0;
        assert_eq!(
            ReadFile(
                h,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut n,
                &mut ov
            ),
            1
        );
        let mut host = vec![0u8; n as usize];
        use std::os::unix::fs::FileExt;
        std::fs::File::open(dir.join(&name))
            .unwrap()
            .read_exact_at(&mut host, at)
            .unwrap();
        assert_eq!(&buf[..n as usize], &host[..]);
        assert_eq!(u64::from(n), (size - at).min(1 << 16));
        handles::close(h as Handle);
    }

    #[test]
    fn layouts_match_windows() {
        assert_eq!(size_of::<Overlapped>(), 32);
        assert_eq!(std::mem::offset_of!(Overlapped, offset_low), 16);
        assert_eq!(std::mem::offset_of!(Overlapped, event), 24);
        assert_eq!(size_of::<OverlappedEntry>(), 32);
    }

    #[test]
    fn overlapped_transfers_signal_and_post() {
        let dir = tempfile::tempdir().unwrap();
        let name = w(&guest_path(&dir.path().join("o.bin")));
        let rw = GENERIC_READ | GENERIC_WRITE;
        let h = CreateFileW(
            name.as_ptr(),
            rw,
            0,
            std::ptr::null(),
            CREATE_ALWAYS,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        );
        assert_ne!(h, INVALID_HANDLE_VALUE);
        let port = CreateIoCompletionPort(h, std::ptr::null_mut(), 77, 0);
        assert!(!port.is_null());

        // An overlapped handle needs an OVERLAPPED.
        let mut n = 0;
        assert_eq!(
            WriteFile(h, b"x".as_ptr().cast(), 1, &mut n, std::ptr::null_mut()),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);

        let eh = handles::insert(Event::new(true, false));
        let mut ov = Overlapped {
            offset_low: 4,
            event: eh,
            ..Default::default()
        };
        assert_eq!(WriteFile(h, b"data".as_ptr().cast(), 4, &mut n, &mut ov), 1);
        assert_eq!((n, ov.internal, ov.internal_high), (4, 0, 4));
        assert!(is_set(eh));
        // Append at the end.
        let mut append = Overlapped {
            offset_low: u32::MAX,
            offset_high: u32::MAX,
            ..Default::default()
        };
        assert_eq!(
            WriteFile(
                h,
                b"!".as_ptr().cast(),
                1,
                std::ptr::null_mut(),
                &mut append
            ),
            1
        );

        let (mut bytes, mut key, mut got) = (0u32, 0usize, std::ptr::null_mut());
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 0),
            1
        );
        assert_eq!((bytes, key, got), (4, 77, &raw mut ov));
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 0),
            1
        );
        assert_eq!(got, &raw mut append);
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 10),
            0
        );
        assert_eq!((last_error(), got), (WAIT_TIMEOUT, std::ptr::null_mut()));

        // Read back from offset 0: four zero bytes, the data, the append.
        let mut buf = [9u8; 16];
        let mut ov = Overlapped {
            event: eh | 1, // no packet
            ..Default::default()
        };
        handles::object(eh).unwrap().set_signal(false);
        assert_eq!(ReadFile(h, buf.as_mut_ptr().cast(), 16, &mut n, &mut ov), 1);
        assert_eq!(&buf[..n as usize], b"\0\0\0\0data!");
        assert!(is_set(eh));
        assert_eq!(GetOverlappedResult(h, &ov, &mut bytes, 1), 1);
        assert_eq!(bytes, 9);
        let mut removed = 0;
        let mut entries = [OverlappedEntry::default(); 4];
        assert_eq!(
            GetQueuedCompletionStatusEx(port, entries.as_mut_ptr(), 4, &mut removed, 0, 0),
            0
        );
        assert_eq!((removed, last_error()), (0, WAIT_TIMEOUT));

        // At the end an overlapped read fails with ERROR_HANDLE_EOF.
        let mut ov = Overlapped {
            offset_low: 100,
            ..Default::default()
        };
        assert_eq!(ReadFile(h, buf.as_mut_ptr().cast(), 16, &mut n, &mut ov), 0);
        assert_eq!(last_error(), ERROR_HANDLE_EOF);
        assert_eq!(ov.internal as u32, STATUS_END_OF_FILE);
        assert_eq!(GetOverlappedResult(h, &ov, &mut bytes, 0), 0);
        assert_eq!((last_error(), bytes), (ERROR_HANDLE_EOF, 0));

        // Posted packets, several at once, from another thread.
        let port_value = port as usize;
        let poster = std::thread::spawn(move || {
            for k in 0..3 {
                PostQueuedCompletionStatus(
                    port_value as *mut c_void,
                    k,
                    k as usize,
                    std::ptr::null_mut(),
                );
            }
        });
        poster.join().unwrap();
        assert_eq!(
            GetQueuedCompletionStatusEx(port, entries.as_mut_ptr(), 4, &mut removed, INFINITE, 0),
            1
        );
        assert_eq!(removed, 3);
        assert_eq!(entries.map(|e| e.key)[..3], [0, 1, 2]);

        assert_eq!(CancelIo(h), 1);
        assert_eq!(CancelIoEx(h, std::ptr::null()), 0);
        assert_eq!(last_error(), ERROR_NOT_FOUND);
        let pending = Overlapped {
            internal: STATUS_PENDING as usize,
            ..Default::default()
        };
        assert_eq!(GetOverlappedResult(h, &pending, &mut bytes, 0), 0);
        assert_eq!(last_error(), ERROR_IO_INCOMPLETE);
        assert!(CreateIoCompletionPort(INVALID_HANDLE_VALUE, port, 0, 0).is_null());
        handles::close(h as Handle);
        handles::close(port as Handle);
        handles::close(eh);
    }
}
