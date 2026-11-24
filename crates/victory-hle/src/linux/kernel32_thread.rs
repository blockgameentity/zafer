//! kernel32: threads (creation, exit codes, suspension, priority,
//! affinity, sleeping) and their TLS and FLS slots. Thread objects and the
//! suspend engine are in [`super::handles`]; guest threads themselves (TEB,
//! stack, TLS callbacks) in `victory_loader::linux::thread`.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use victory_loader::linux::teb::{self, set_last_error};
use victory_loader::linux::thread::{self as guest_thread, StartRoutine};

use super::handles::{self, Blocking, CURRENT_PROCESS, CURRENT_THREAD, Handle, INFINITE, Thread};
use super::kernel32_system::topology;
use super::kernel32_wait::{ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER};
use crate::{HleModule, hle_export};

const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_SIGNAL_REFUSED: u32 = 156;
const CREATE_SUSPENDED: u32 = 4;
const STACK_SIZE_PARAM_IS_A_RESERVATION: u32 = 0x1_0000;
const THREAD_PRIORITY_ERROR_RETURN: i32 = 0x7FFF_FFFF;
const PROC_THREAD_ATTRIBUTE_GROUP_AFFINITY: usize = 0x3_0003;
const PROC_THREAD_ATTRIBUTE_IDEAL_PROCESSOR: usize = 0x3_0005;

fn thread(handle: Handle) -> Option<Arc<Thread>> {
    let thread = handles::get::<Thread>(handle);
    if thread.is_none() {
        set_last_error(ERROR_INVALID_HANDLE);
    }
    thread
}

// ---- creation and exit ----

struct Start {
    start: StartRoutine,
    parameter: usize,
    thread: Arc<Thread>,
}

/// Runs on the new thread: adopts its object, waits while it is created
/// suspended, then runs title code.
unsafe extern "win64" fn thread_start(start: *mut c_void) -> u32 {
    // SAFETY: the box `create_thread` handed to spawn, ours alone now.
    let start = unsafe { Box::from_raw(start.cast::<Start>()) };
    handles::adopt(&start.thread);
    handles::park_if_suspended(&start.thread);
    // SAFETY: the title's start routine and its parameter.
    unsafe { (start.start)(start.parameter as *mut c_void) }
}

/// A guest thread running `start(parameter)`, with its object.
pub(crate) fn create_thread(
    start: StartRoutine,
    parameter: usize,
    stack: usize,
    suspended: bool,
) -> std::io::Result<Arc<Thread>> {
    let thread = Thread::new(0);
    if suspended {
        handles::start_suspended(&thread);
    }
    let boxed = Box::into_raw(Box::new(Start {
        start,
        parameter,
        thread: thread.clone(),
    }));
    let object = thread.clone();
    let on_exit = Box::new(move |code: u32| {
        fls_thread_exit();
        handles::finish(&object, code);
    });
    match guest_thread::spawn(thread_start, boxed.cast(), stack, on_exit) {
        Ok(spawned) => {
            handles::register(&thread, spawned.id);
            Ok(thread)
        }
        Err(e) => {
            // SAFETY: no thread started, so the box is still ours.
            drop(unsafe { Box::from_raw(boxed) });
            Err(e)
        }
    }
}

/// Windows commits `stack` up front of the default reservation unless it
/// says it is the reservation; Linux commits as the stack is touched.
fn stack_size(stack: usize, flags: u32) -> usize {
    if flags & STACK_SIZE_PARAM_IS_A_RESERVATION != 0 || stack > 1 << 20 {
        stack
    } else {
        0
    }
}

pub extern "win64" fn CreateThread(
    _sa: usize,
    stack: usize,
    start: StartRoutine,
    parameter: usize,
    flags: u32,
    id: *mut u32,
) -> Handle {
    CreateRemoteThreadEx(CURRENT_PROCESS, _sa, stack, start, parameter, flags, std::ptr::null(), id)
}

#[allow(clippy::too_many_arguments)]
pub extern "win64" fn CreateRemoteThreadEx(
    process: Handle,
    _sa: usize,
    stack: usize,
    start: StartRoutine,
    parameter: usize,
    flags: u32,
    attributes: *const AttributeList,
    id: *mut u32,
) -> Handle {
    if handles::get::<handles::Process>(process).is_none() {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let thread = match create_thread(start, parameter, stack_size(stack, flags), flags & CREATE_SUSPENDED != 0) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%e, "CreateThread failed");
            set_last_error(ERROR_NOT_ENOUGH_MEMORY);
            return 0;
        }
    };
    // SAFETY: null or a list InitializeProcThreadAttributeList made.
    for (attribute, value) in unsafe { AttributeList::entries(attributes) } {
        match attribute {
            PROC_THREAD_ATTRIBUTE_GROUP_AFFINITY => {
                // SAFETY: the title's GROUP_AFFINITY.
                let a = unsafe { value.cast::<GroupAffinity>().read_unaligned() };
                set_affinity(&thread, a.mask as u64);
            }
            PROC_THREAD_ATTRIBUTE_IDEAL_PROCESSOR => {
                // SAFETY: the title's PROCESSOR_NUMBER.
                let p = unsafe { value.cast::<ProcessorNumber>().read_unaligned() };
                thread.ideal_processor.store(p.number.into(), Ordering::Relaxed);
            }
            _ => {}
        }
    }
    if !id.is_null() {
        // SAFETY: the title's out pointer.
        unsafe { id.write_unaligned(thread.id()) };
    }
    handles::insert(thread)
}

pub extern "win64" fn GetExitCodeThread(handle: Handle, code: *mut u32) -> i32 {
    let Some(thread) = thread(handle) else {
        return 0;
    };
    // SAFETY: the title's out pointer.
    unsafe { code.write_unaligned(thread.exit_code()) };
    1
}

pub extern "win64" fn OpenThread(_access: u32, _inherit: i32, id: u32) -> Handle {
    match handles::thread_by_id(id) {
        Some(thread) => handles::insert(thread),
        None => {
            set_last_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub extern "win64" fn GetCurrentThread() -> Handle {
    CURRENT_THREAD
}

pub extern "win64" fn GetCurrentThreadId() -> u32 {
    guest_thread::current_id()
}

pub extern "win64" fn SuspendThread(handle: Handle) -> u32 {
    let Some(thread) = thread(handle) else {
        return u32::MAX;
    };
    handles::suspend(&thread).unwrap_or_else(|| {
        set_last_error(ERROR_SIGNAL_REFUSED);
        u32::MAX
    })
}

pub extern "win64" fn ResumeThread(handle: Handle) -> u32 {
    thread(handle).map_or(u32::MAX, |t| handles::resume(&t))
}

pub extern "win64" fn Sleep(ms: u32) {
    if ms == 0 {
        // SAFETY: no preconditions.
        unsafe { libc::sched_yield() };
        return;
    }
    let _blocking = Blocking::begin();
    if ms == INFINITE {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    std::thread::sleep(Duration::from_millis(ms.into()));
}

pub extern "win64" fn SwitchToThread() -> i32 {
    // SAFETY: no preconditions.
    unsafe { libc::sched_yield() };
    1
}

pub extern "win64" fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize) {
    // SAFETY: pthread_getattr_np fills `attr` for the calling thread, which
    // we destroy after reading its stack.
    let (base, size) = unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        let (mut base, mut size) = (std::ptr::null_mut(), 0);
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) == 0 {
            libc::pthread_attr_getstack(&attr, &mut base, &mut size);
            libc::pthread_attr_destroy(&mut attr);
        }
        (base as usize, size)
    };
    // SAFETY: the title's out pointers.
    unsafe {
        low.write_unaligned(base);
        high.write_unaligned(base + size);
    }
}

// ---- priority, affinity ----

/// Remembered and read back; the host scheduler is not told (raising a
/// lowered nice value again needs privileges a game does not have).
pub extern "win64" fn SetThreadPriority(handle: Handle, priority: i32) -> i32 {
    if !matches!(priority, -15 | -2..=2 | 15) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    thread(handle).map_or(0, |t| {
        t.priority.store(priority, Ordering::Relaxed);
        1
    })
}

pub extern "win64" fn GetThreadPriority(handle: Handle) -> i32 {
    thread(handle).map_or(THREAD_PRIORITY_ERROR_RETURN, |t| t.priority.load(Ordering::Relaxed))
}

/// `GROUP_AFFINITY`.
#[repr(C)]
pub struct GroupAffinity {
    mask: usize,
    group: u16,
    reserved: [u16; 3],
}

/// `PROCESSOR_NUMBER`.
#[repr(C)]
pub struct ProcessorNumber {
    group: u16,
    number: u8,
    reserved: u8,
}

fn affinity(thread: &Thread) -> u64 {
    match thread.affinity.load(Ordering::Relaxed) {
        0 => topology().all(),
        mask => mask,
    }
}

/// Remembers `mask` and pins the thread there. A title written for one
/// machine pins by number: a processor this machine lacks folds onto one
/// it has (number modulo the count), so "A on 12, B on 13" still lands on
/// two processors where there are two.
fn set_affinity(thread: &Thread, mask: u64) -> u64 {
    let previous = affinity(thread);
    thread.affinity.store(mask, Ordering::Relaxed);
    let host = &topology().host;
    // SAFETY: a zeroed cpu_set_t is empty; CPU_SET stays inside it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for bit in (0..64).filter(|b| mask & 1 << b != 0) {
        // SAFETY: host CPU numbers are below CPU_SETSIZE.
        unsafe { libc::CPU_SET(host[bit % host.len()], &mut set) };
    }
    // SAFETY: a valid set for one of our threads; failure leaves it unpinned.
    unsafe { libc::sched_setaffinity(thread.id() as libc::pid_t, size_of_val(&set), &set) };
    previous
}

pub extern "win64" fn SetThreadAffinityMask(handle: Handle, mask: usize) -> usize {
    if mask == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    thread(handle).map_or(0, |t| set_affinity(&t, mask as u64) as usize)
}

pub extern "win64" fn GetThreadGroupAffinity(handle: Handle, out: *mut GroupAffinity) -> i32 {
    thread(handle).map_or(0, |t| {
        // SAFETY: the title's GROUP_AFFINITY.
        unsafe {
            out.write_unaligned(GroupAffinity {
                mask: affinity(&t) as usize,
                group: 0,
                reserved: [0; 3],
            })
        };
        1
    })
}

pub extern "win64" fn SetThreadGroupAffinity(
    handle: Handle,
    affinity: *const GroupAffinity,
    previous: *mut GroupAffinity,
) -> i32 {
    // SAFETY: the title's GROUP_AFFINITY.
    let wanted = unsafe { affinity.read_unaligned() };
    if wanted.group != 0 || wanted.mask == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    thread(handle).map_or(0, |t| {
        let old = set_affinity(&t, wanted.mask as u64);
        if !previous.is_null() {
            // SAFETY: the title's out pointer.
            unsafe {
                previous.write_unaligned(GroupAffinity {
                    mask: old as usize,
                    group: 0,
                    reserved: [0; 3],
                })
            };
        }
        1
    })
}

/// Remembered; Linux has no such hint.
pub extern "win64" fn SetThreadIdealProcessorEx(
    handle: Handle,
    ideal: *const ProcessorNumber,
    previous: *mut ProcessorNumber,
) -> i32 {
    thread(handle).map_or(0, |t| {
        // SAFETY: the title's PROCESSOR_NUMBER.
        let wanted = unsafe { ideal.read_unaligned() };
        let old = t.ideal_processor.swap(wanted.number.into(), Ordering::Relaxed);
        if !previous.is_null() {
            // SAFETY: the title's out pointer.
            unsafe {
                previous.write_unaligned(ProcessorNumber {
                    group: 0,
                    number: old as u8,
                    reserved: 0,
                })
            };
        }
        1
    })
}

// ---- attribute lists (CreateRemoteThreadEx) ----

/// `PROC_THREAD_ATTRIBUTE_LIST`, opaque to title code: a header, then
/// (attribute, size, value) entries, at the sizes Windows asks for.
#[repr(C)]
pub struct AttributeList {
    capacity: u32,
    count: u32,
    reserved: [usize; 2],
    entries: [[usize; 3]; 0],
}

impl AttributeList {
    /// # Safety
    /// `list` is null or was set up by `InitializeProcThreadAttributeList`.
    unsafe fn entries(list: *const AttributeList) -> Vec<(usize, *const u8)> {
        if list.is_null() {
            return Vec::new();
        }
        // SAFETY: by the caller.
        unsafe {
            let entries = (&raw const (*list).entries).cast::<[usize; 3]>();
            (0..(*list).count as usize).map(|i| ((*entries.add(i))[0], (*entries.add(i))[2] as *const u8)).collect()
        }
    }
}

pub extern "win64" fn InitializeProcThreadAttributeList(
    list: *mut AttributeList,
    count: u32,
    _flags: u32,
    size: *mut usize,
) -> i32 {
    let needed = size_of::<AttributeList>() + count as usize * 24;
    // SAFETY: the title's in-out size.
    let room = unsafe { size.read_unaligned() };
    // SAFETY: as above.
    unsafe { size.write_unaligned(needed) };
    if list.is_null() || room < needed {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the title's buffer of `needed` bytes.
    unsafe {
        list.write(AttributeList {
            capacity: count,
            count: 0,
            reserved: [0; 2],
            entries: [],
        })
    };
    1
}

pub extern "win64" fn UpdateProcThreadAttribute(
    list: *mut AttributeList,
    _flags: u32,
    attribute: usize,
    value: *const u8,
    size: usize,
    _previous: *mut u8,
    _return_size: *mut usize,
) -> i32 {
    // SAFETY: a list InitializeProcThreadAttributeList set up, with room
    // for `capacity` entries.
    unsafe {
        if (*list).count >= (*list).capacity {
            set_last_error(ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        let entries = (&raw mut (*list).entries).cast::<[usize; 3]>();
        entries.add((*list).count as usize).write([attribute, size, value as usize]);
        (*list).count += 1;
    }
    1
}

pub extern "win64" fn DeleteProcThreadAttributeList(_list: *mut AttributeList) {}

// ---- TLS, FLS ----

const TLS_OUT_OF_INDEXES: u32 = u32::MAX;

/// TLS indexes handed out, as bits. Index 0 is never handed out (callers
/// take 0 as "none").
// ponytail: freed indexes are not reused, so no thread can see another
// index's stale value without zeroing every thread's slot; 63 per run.
static TLS_USED: AtomicU64 = AtomicU64::new(1);

pub extern "win64" fn TlsAlloc() -> u32 {
    let mut used = TLS_USED.load(Ordering::SeqCst);
    loop {
        if used == u64::MAX {
            set_last_error(ERROR_NOT_ENOUGH_MEMORY);
            return TLS_OUT_OF_INDEXES;
        }
        let index = used.trailing_ones();
        match TLS_USED.compare_exchange(used, used | 1 << index, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return index,
            Err(now) => used = now,
        }
    }
}

fn tls_slot(index: u32) -> Option<*mut usize> {
    if index == 0 || index >= 64 || TLS_USED.load(Ordering::Relaxed) & 1 << index == 0 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return None;
    }
    // SAFETY: index < 64, inside the calling thread's slots.
    Some(unsafe { teb::tls_slots().add(index as usize) })
}

pub extern "win64" fn TlsFree(index: u32) -> i32 {
    tls_slot(index).map_or(0, |slot| {
        // SAFETY: the calling thread's slot.
        unsafe { slot.write(0) };
        1
    })
}

pub extern "win64" fn TlsGetValue(index: u32) -> usize {
    tls_slot(index).map_or(0, |slot| {
        set_last_error(0);
        // SAFETY: the calling thread's slot.
        unsafe { slot.read() }
    })
}

pub extern "win64" fn TlsSetValue(index: u32, value: usize) -> i32 {
    tls_slot(index).map_or(0, |slot| {
        // SAFETY: the calling thread's slot.
        unsafe { slot.write(value) };
        1
    })
}

/// `PFLS_CALLBACK_FUNCTION`.
pub type FlsCallback = unsafe extern "win64" fn(data: usize);

const FLS_OUT_OF_INDEXES: u32 = u32::MAX;
const FLS_MAXIMUM_AVAILABLE: usize = 4080;

/// The allocated FLS indexes with their callbacks, and every thread's
/// values (so `FlsFree` can hand each to the callback and clear it).
struct Fls {
    indexes: Vec<Option<Option<FlsCallback>>>, // None: free
    threads: Vec<Weak<Mutex<Vec<usize>>>>,
}

static FLS: LazyLock<Mutex<Fls>> = LazyLock::new(|| {
    Mutex::new(Fls {
        indexes: vec![Some(None)], // index 0 is never handed out
        threads: Vec::new(),
    })
});

thread_local! {
    static FLS_VALUES: Arc<Mutex<Vec<usize>>> = {
        let values = Arc::new(Mutex::new(Vec::new()));
        let mut fls = lock(&FLS);
        fls.threads.retain(|t| t.strong_count() > 0);
        fls.threads.push(Arc::downgrade(&values));
        values
    };
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub extern "win64" fn FlsAlloc(callback: Option<FlsCallback>) -> u32 {
    let mut fls = lock(&FLS);
    let index = match fls.indexes.iter().position(Option::is_none) {
        Some(i) => i,
        None if fls.indexes.len() < FLS_MAXIMUM_AVAILABLE => {
            fls.indexes.push(None);
            fls.indexes.len() - 1
        }
        None => {
            set_last_error(ERROR_NOT_ENOUGH_MEMORY);
            return FLS_OUT_OF_INDEXES;
        }
    };
    fls.indexes[index] = Some(callback);
    index as u32
}

fn fls_valid(index: u32) -> bool {
    let valid = index != 0 && lock(&FLS).indexes.get(index as usize).is_some_and(Option::is_some);
    if !valid {
        set_last_error(ERROR_INVALID_PARAMETER);
    }
    valid
}

/// Hands every thread's value of `index` to its callback (on this thread,
/// as Windows does), then frees the index.
pub extern "win64" fn FlsFree(index: u32) -> i32 {
    if !fls_valid(index) {
        return 0;
    }
    let (callback, threads) = {
        let fls = lock(&FLS);
        (fls.indexes[index as usize].flatten(), fls.threads.iter().filter_map(Weak::upgrade).collect::<Vec<_>>())
    };
    for values in threads {
        let value = lock(&values).get_mut(index as usize).map_or(0, std::mem::take);
        if let (Some(callback), true) = (callback, value != 0) {
            // SAFETY: the title's callback with the title's value.
            unsafe { callback(value) };
        }
    }
    lock(&FLS).indexes[index as usize] = None;
    1
}

pub extern "win64" fn FlsGetValue(index: u32) -> usize {
    if !fls_valid(index) {
        return 0;
    }
    set_last_error(0);
    FLS_VALUES.with(|v| lock(v).get(index as usize).copied().unwrap_or(0))
}

pub extern "win64" fn FlsSetValue(index: u32, value: usize) -> i32 {
    if !fls_valid(index) {
        return 0;
    }
    FLS_VALUES.with(|v| {
        let mut values = lock(v);
        if values.len() <= index as usize {
            values.resize(index as usize + 1, 0);
        }
        values[index as usize] = value;
    });
    1
}

/// A thread that `CreateThread` made ends: its FLS values go to their
/// callbacks, as on Windows.
pub(crate) fn fls_thread_exit() {
    let values = FLS_VALUES.with(|v| std::mem::take(&mut *lock(v)));
    for (index, value) in values.into_iter().enumerate().filter(|&(_, v)| v != 0) {
        let callback = lock(&FLS).indexes.get(index).copied().flatten().flatten();
        if let Some(callback) = callback {
            // SAFETY: the title's callback with the title's value.
            unsafe { callback(value) };
        }
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-thread",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-processthreads-*",
            "api-ms-win-core-processtopology-*",
            "api-ms-win-core-synch-*",
            "api-ms-win-core-fibers-*",
        ],
        exports: vec![
            hle_export!(CreateThread),
            hle_export!(CreateRemoteThreadEx),
            hle_export!(GetExitCodeThread),
            hle_export!(OpenThread),
            hle_export!(GetCurrentThread),
            hle_export!(GetCurrentThreadId),
            hle_export!(SuspendThread),
            hle_export!(ResumeThread),
            hle_export!(Sleep),
            hle_export!(SwitchToThread),
            hle_export!(GetCurrentThreadStackLimits),
            hle_export!(SetThreadPriority),
            hle_export!(GetThreadPriority),
            hle_export!(SetThreadAffinityMask),
            hle_export!(GetThreadGroupAffinity),
            hle_export!(SetThreadGroupAffinity),
            hle_export!(SetThreadIdealProcessorEx),
            hle_export!(InitializeProcThreadAttributeList),
            hle_export!(UpdateProcThreadAttribute),
            hle_export!(DeleteProcThreadAttributeList),
            hle_export!(TlsAlloc),
            hle_export!(TlsFree),
            hle_export!(TlsGetValue),
            hle_export!(TlsSetValue),
            hle_export!(FlsAlloc),
            hle_export!(FlsFree),
            hle_export!(FlsGetValue),
            hle_export!(FlsSetValue),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::handles::{STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use crate::linux::kernel32_wait::{CloseHandle, WaitForSingleObject};
    use std::mem::offset_of;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn guest_layouts_match_windows() {
        assert_eq!(size_of::<GroupAffinity>(), 16);
        assert_eq!(offset_of!(GroupAffinity, group), 8);
        assert_eq!(size_of::<ProcessorNumber>(), 4);
        assert_eq!(offset_of!(ProcessorNumber, number), 2);
        assert_eq!(size_of::<AttributeList>(), 24);
    }

    static RAN: AtomicU32 = AtomicU32::new(0);

    unsafe extern "win64" fn count(parameter: *mut c_void) -> u32 {
        RAN.fetch_add(1, Ordering::SeqCst);
        parameter as u32
    }

    #[test]
    fn threads_run_report_their_exit_code_and_start_suspended() {
        let mut id = 0;
        let h = CreateThread(0, 0, count, 42, 0, &mut id);
        assert!(h != 0 && id != 0);
        assert_eq!(WaitForSingleObject(h, 5000), WAIT_OBJECT_0);
        let mut code = 0;
        assert_eq!(GetExitCodeThread(h, &mut code), 1);
        assert_eq!(code, 42);
        assert_eq!(CloseHandle(h), 1);

        let before = RAN.load(Ordering::SeqCst);
        let h = CreateThread(0, 0, count, 7, CREATE_SUSPENDED, &mut id);
        assert_eq!(WaitForSingleObject(h, 50), WAIT_TIMEOUT);
        assert_eq!(GetExitCodeThread(h, &mut code), 1);
        assert_eq!(code, STILL_ACTIVE);
        let opened = OpenThread(0, 0, id);
        assert!(opened != 0);
        assert_eq!(SetThreadPriority(opened, 2), 1);
        assert_eq!(GetThreadPriority(h), 2);
        assert_eq!(SetThreadAffinityMask(h, 1), topology().all() as usize);
        assert_eq!(SuspendThread(h), 1);
        assert_eq!(ResumeThread(h), 2);
        assert_eq!(ResumeThread(h), 1);
        assert_eq!(WaitForSingleObject(opened, 5000), WAIT_OBJECT_0);
        assert!(RAN.load(Ordering::SeqCst) > before);
        assert_eq!(GetExitCodeThread(h, &mut code), 1);
        assert_eq!(code, 7);
        assert!(CloseHandle(h) == 1 && CloseHandle(opened) == 1);
        assert_eq!(GetThreadPriority(GetCurrentThread()), 0);
    }

    #[test]
    fn affinity_is_read_back_and_pins_the_host_thread() {
        let me = GetCurrentThread();
        let mut a = GroupAffinity { mask: 0, group: 0, reserved: [0; 3] };
        let t = std::thread::spawn(move || {
            assert_eq!(SetThreadAffinityMask(me, 1), topology().all() as usize);
            let mut a = GroupAffinity { mask: 0, group: 0, reserved: [0; 3] };
            assert_eq!(GetThreadGroupAffinity(me, &mut a), 1);
            assert_eq!(a.mask, 1);
            // SAFETY: plain query of our own affinity.
            let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            unsafe { libc::sched_getaffinity(0, size_of_val(&set), &mut set) };
            // SAFETY: reads of our own set.
            assert!(unsafe { libc::CPU_ISSET(topology().host[0], &set) && libc::CPU_COUNT(&set) == 1 });
            assert_eq!(SetThreadAffinityMask(me, 0), 0);
        });
        t.join().unwrap();
        assert_eq!(GetThreadGroupAffinity(me, &mut a), 1);
        assert_eq!(a.mask as u64, topology().all());
    }

    #[test]
    fn tls_never_hands_out_zero_and_keeps_values_per_thread() {
        let i = TlsAlloc();
        assert!(i != 0 && i != TLS_OUT_OF_INDEXES);
        assert_eq!(TlsGetValue(i), 0);
        assert_eq!(TlsSetValue(i, 5), 1);
        assert_eq!(std::thread::spawn(move || TlsGetValue(i)).join().unwrap(), 0);
        assert_eq!(TlsGetValue(i), 5);
        assert_eq!(TlsFree(i), 1);
        assert_eq!(TlsGetValue(i), 0);
        assert_eq!(TlsSetValue(0, 1), 0);
    }

    static FREED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    unsafe extern "win64" fn freed(value: usize) {
        FREED.lock().unwrap().push(value);
    }
    unsafe extern "win64" fn set_fls(index: *mut c_void) -> u32 {
        FlsSetValue(index as u32, 0x77);
        0
    }

    #[test]
    fn fls_callbacks_run_at_thread_exit_and_free() {
        let i = FlsAlloc(Some(freed));
        assert!(i != 0 && i != FLS_OUT_OF_INDEXES);
        let h = CreateThread(0, 0, set_fls, i as usize, 0, std::ptr::null_mut());
        assert_eq!(WaitForSingleObject(h, 5000), WAIT_OBJECT_0);
        assert_eq!(*FREED.lock().unwrap(), [0x77]);
        assert_eq!(FlsSetValue(i, 0x99), 1);
        assert_eq!(FlsGetValue(i), 0x99);
        assert_eq!(FlsFree(i), 1);
        assert_eq!(*FREED.lock().unwrap(), [0x77, 0x99]);
        assert_eq!(FlsGetValue(i), 0);
        CloseHandle(h);
    }

    #[test]
    fn attribute_lists_size_and_hold_entries() {
        let mut size = 0;
        assert_eq!(InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size), 0);
        assert_eq!(size, 24 + 48);
        let mut buffer = vec![0usize; size / 8];
        let list = buffer.as_mut_ptr().cast::<AttributeList>();
        assert_eq!(InitializeProcThreadAttributeList(list, 2, 0, &mut size), 1);
        let ideal = ProcessorNumber { group: 0, number: 3, reserved: 0 };
        let p = (&raw const ideal).cast();
        assert_eq!(UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_IDEAL_PROCESSOR, p, 4, std::ptr::null_mut(), std::ptr::null_mut()), 1);
        // SAFETY: the list was just set up.
        let entries = unsafe { AttributeList::entries(list) };
        assert_eq!(entries, [(PROC_THREAD_ATTRIBUTE_IDEAL_PROCESSOR, p)]);
        let mut low = 0;
        let mut high = 0;
        GetCurrentThreadStackLimits(&mut low, &mut high);
        let here = (&raw const low) as usize;
        assert!(low < here && here < high);
    }
}
