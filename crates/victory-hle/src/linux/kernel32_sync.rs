//! kernel32: events, mutexes and semaphores (kernel objects, see
//! [`super::handles`]), and the user-mode locks: critical sections, SRW
//! locks, condition variables, `InitOnceExecuteOnce`, `WaitOnAddress`.
//!
//! Each user-mode lock lives entirely in the title's own object, at the
//! size Windows gives it, and an all-zero object is a valid unlocked one
//! (title code declares SRW locks and condition variables as zeroed
//! statics, with no destroy call). So nothing here keeps host state per
//! lock: every lock is a futex on a 32-bit half of the title's object.
//! Every sleep is a [`Blocking`] region that ends before the thread tries
//! for the lock again: a thread suspended while it waits parks without it.

#![allow(non_snake_case)]

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::SeqCst};

use victory_loader::linux::teb::set_last_error;
use victory_loader::linux::thread::current_id;

use super::handles::{
    self, Blocking, Deadline, Event, Handle, KMutex, Object, Semaphore, futex_wait, futex_wake,
};
use super::kernel32_process::{ansi_string, wide_string};
use super::kernel32_wait::{ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER};
use crate::{HleModule, hle_export};

const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_NOT_OWNER: u32 = 288;
const ERROR_TOO_MANY_POSTS: u32 = 298;
const ERROR_TIMEOUT: u32 = 1460;
const CREATE_EVENT_MANUAL_RESET: u32 = 1;
const CREATE_EVENT_INITIAL_SET: u32 = 2;

// ---- events, mutexes, semaphores ----

/// A handle to a new object, or to the live one named `name` if it is a
/// `T` (`ERROR_ALREADY_EXISTS`). Last error 0 for a new one, as Windows.
fn create<T: Object>(name: Option<String>, make: impl FnOnce() -> Arc<T>) -> Handle {
    let Some(name) = name else {
        set_last_error(0);
        return handles::insert(make());
    };
    let (object, existed) = handles::open_named(&name, || -> Arc<dyn Object> { make() });
    if existed && !(&*object as &dyn Any).is::<T>() {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    set_last_error(if existed { ERROR_ALREADY_EXISTS } else { 0 });
    handles::insert_dyn(object)
}

pub extern "win64" fn CreateEventW(_sa: usize, manual: i32, initial: i32, name: *const u16) -> Handle {
    // SAFETY: a null or NUL-terminated title string.
    let name = unsafe { wide_string(name) };
    create(name, || Event::new(manual != 0, initial != 0))
}

pub extern "win64" fn CreateEventA(_sa: usize, manual: i32, initial: i32, name: *const u8) -> Handle {
    // SAFETY: a null or NUL-terminated title string.
    let name = unsafe { ansi_string(name) };
    create(name, || Event::new(manual != 0, initial != 0))
}

pub extern "win64" fn CreateEventExW(_sa: usize, name: *const u16, flags: u32, _access: u32) -> Handle {
    // SAFETY: a null or NUL-terminated title string.
    let name = unsafe { wide_string(name) };
    create(name, || {
        Event::new(flags & CREATE_EVENT_MANUAL_RESET != 0, flags & CREATE_EVENT_INITIAL_SET != 0)
    })
}

fn with<T: Object>(handle: Handle, f: impl FnOnce(&T) -> bool) -> i32 {
    match handles::get::<T>(handle) {
        Some(object) => f(&object).into(),
        None => {
            set_last_error(ERROR_INVALID_HANDLE);
            0
        }
    }
}

pub extern "win64" fn SetEvent(event: Handle) -> i32 {
    with(event, |e: &Event| {
        e.set();
        true
    })
}

pub extern "win64" fn ResetEvent(event: Handle) -> i32 {
    with(event, |e: &Event| {
        e.reset();
        true
    })
}

pub extern "win64" fn CreateMutexW(_sa: usize, owned: i32, name: *const u16) -> Handle {
    // SAFETY: a null or NUL-terminated title string.
    let name = unsafe { wide_string(name) };
    create(name, || KMutex::new(owned != 0))
}

pub extern "win64" fn CreateMutexA(_sa: usize, owned: i32, name: *const u8) -> Handle {
    // SAFETY: a null or NUL-terminated title string.
    let name = unsafe { ansi_string(name) };
    create(name, || KMutex::new(owned != 0))
}

pub extern "win64" fn ReleaseMutex(mutex: Handle) -> i32 {
    with(mutex, |m: &KMutex| {
        m.release() || {
            set_last_error(ERROR_NOT_OWNER);
            false
        }
    })
}

pub extern "win64" fn CreateSemaphoreW(_sa: usize, initial: i32, maximum: i32, name: *const u16) -> Handle {
    CreateSemaphoreExW(_sa, initial, maximum, name, 0, 0)
}

pub extern "win64" fn CreateSemaphoreExW(
    _sa: usize,
    initial: i32,
    maximum: i32,
    name: *const u16,
    _flags: u32,
    _access: u32,
) -> Handle {
    if maximum <= 0 || initial < 0 || initial > maximum {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: a null or NUL-terminated title string.
    let name = unsafe { wide_string(name) };
    create(name, || Semaphore::new(initial, maximum))
}

pub extern "win64" fn ReleaseSemaphore(semaphore: Handle, count: i32, previous: *mut i32) -> i32 {
    with(semaphore, |s: &Semaphore| match s.release(count) {
        Some(p) => {
            if !previous.is_null() {
                // SAFETY: the title's out pointer.
                unsafe { previous.write_unaligned(p) };
            }
            true
        }
        None => {
            set_last_error(ERROR_TOO_MANY_POSTS);
            false
        }
    })
}

// ---- user-mode locks ----

/// A lock held for a few instructions is cheaper to wait out than to sleep on.
fn spin_until(free: impl Fn() -> bool) -> bool {
    (0..100).any(|_| {
        std::hint::spin_loop();
        free()
    })
}

fn word<'a>(at: *mut u32) -> &'a AtomicU32 {
    // SAFETY: title lock objects are aligned and live while a call uses them.
    unsafe { AtomicU32::from_ptr(at) }
}

/// `RTL_CRITICAL_SECTION`. Title code reads `OwningThread` and
/// `RecursionCount` ("do I hold it?"), so those two are kept as Windows
/// keeps them. `LockSemaphore`, a handle Windows makes on first contention,
/// holds the lock itself: 0 free, 1 held, 2 held with a sleeper maybe.
#[repr(C)]
pub struct CriticalSection {
    debug_info: usize,  // -1 once initialised, as Windows without a debug block
    lock_count: i32,    // -1 free, -2 held
    recursion_count: i32,
    owning_thread: usize, // thread id, 0 when free
    lock_semaphore: usize,
    spin_count: usize, // kept, not used
}

impl CriticalSection {
    fn lock(cs: *mut CriticalSection) -> &'static AtomicU32 {
        // SAFETY: field of the title's critical section.
        word(unsafe { (&raw mut (*cs).lock_semaphore).cast() })
    }
    fn owner(cs: *mut CriticalSection) -> &'static AtomicUsize {
        // SAFETY: field of the title's critical section.
        unsafe { AtomicUsize::from_ptr(&raw mut (*cs).owning_thread) }
    }
    /// Only ever called by the thread that holds (or just got) the lock.
    fn set_owner(cs: *mut CriticalSection, owner: u32) {
        Self::owner(cs).store(owner as usize, SeqCst);
        // SAFETY: the owner's own fields of the title's critical section.
        unsafe {
            (*cs).recursion_count = (owner != 0).into();
            (*cs).lock_count = if owner != 0 { -2 } else { -1 };
        }
    }
}

pub extern "win64" fn InitializeCriticalSectionEx(cs: *mut CriticalSection, spin: u32, _flags: u32) -> i32 {
    // SAFETY: the title's critical section.
    unsafe {
        cs.write(CriticalSection {
            debug_info: usize::MAX,
            lock_count: -1,
            recursion_count: 0,
            owning_thread: 0,
            lock_semaphore: 0,
            spin_count: (spin & 0x00FF_FFFF) as usize,
        })
    };
    1
}

pub extern "win64" fn InitializeCriticalSectionAndSpinCount(cs: *mut CriticalSection, spin: u32) -> i32 {
    InitializeCriticalSectionEx(cs, spin, 0)
}

pub extern "win64" fn InitializeCriticalSection(cs: *mut CriticalSection) {
    InitializeCriticalSectionEx(cs, 0, 0);
}

/// Nothing to free: a deleted section is a zeroed one, which still works.
pub extern "win64" fn DeleteCriticalSection(cs: *mut CriticalSection) {
    InitializeCriticalSectionEx(cs, 0, 0);
    // SAFETY: the title's critical section.
    unsafe { (*cs).debug_info = 0 };
}

pub extern "win64" fn TryEnterCriticalSection(cs: *mut CriticalSection) -> i32 {
    let me = current_id();
    // Only the owner can read its own id here.
    if CriticalSection::owner(cs).load(SeqCst) == me as usize {
        // SAFETY: the owner's own field.
        unsafe { (*cs).recursion_count += 1 };
        return 1;
    }
    if CriticalSection::lock(cs).compare_exchange(0, 1, SeqCst, SeqCst).is_err() {
        return 0;
    }
    CriticalSection::set_owner(cs, me);
    1
}

pub extern "win64" fn EnterCriticalSection(cs: *mut CriticalSection) {
    if TryEnterCriticalSection(cs) != 0 {
        return;
    }
    let lock = CriticalSection::lock(cs);
    let got = spin_until(|| lock.load(SeqCst) == 0)
        && lock.compare_exchange(0, 1, SeqCst, SeqCst).is_ok();
    if !got {
        while lock.swap(2, SeqCst) != 0 {
            let _blocking = Blocking::begin();
            futex_wait(lock.as_ptr(), 2, Deadline::NEVER);
        }
    }
    CriticalSection::set_owner(cs, current_id());
}

pub extern "win64" fn LeaveCriticalSection(cs: *mut CriticalSection) {
    // SAFETY: the owner's own field.
    let left = unsafe {
        (*cs).recursion_count -= 1;
        (*cs).recursion_count
    };
    if left > 0 {
        return;
    }
    CriticalSection::set_owner(cs, 0);
    let lock = CriticalSection::lock(cs);
    if lock.swap(0, SeqCst) == 2 {
        futex_wake(lock.as_ptr(), 1);
    }
}

// SRWLOCK: the low half of the pointer. Bit 0 held exclusively, bit 1 a
// thread may be asleep, the rest the number of shared holders. A release
// that frees the lock with bit 1 set wakes everybody and they compete
// again. New shared holders queue behind a sleeper, so a writer is not
// starved. Not recursive, as on Windows.
// ponytail: wake-all on release; a queue in the word (as Windows keeps one) if a profile shows herds.
const SRW_EXCLUSIVE: u32 = 1;
const SRW_SLEEPERS: u32 = 2;
const SRW_SHARED: u32 = 4;

pub type SrwLock = usize;

fn srw(lock: *mut SrwLock) -> &'static AtomicU32 {
    word(lock.cast())
}

fn srw_free(state: u32, exclusive: bool) -> bool {
    if exclusive {
        state & !SRW_SLEEPERS == 0
    } else {
        state & (SRW_EXCLUSIVE | SRW_SLEEPERS) == 0
    }
}

fn srw_try(lock: *mut SrwLock, exclusive: bool) -> bool {
    let word = srw(lock);
    let mut state = word.load(SeqCst);
    while srw_free(state, exclusive) {
        let next = if exclusive { state | SRW_EXCLUSIVE } else { state + SRW_SHARED };
        match word.compare_exchange(state, next, SeqCst, SeqCst) {
            Ok(_) => return true,
            Err(now) => state = now,
        }
    }
    false
}

fn srw_acquire(lock: *mut SrwLock, exclusive: bool) {
    if srw_try(lock, exclusive) {
        return;
    }
    let word = srw(lock);
    if spin_until(|| srw_free(word.load(SeqCst), exclusive)) && srw_try(lock, exclusive) {
        return;
    }
    while !srw_try(lock, exclusive) {
        let state = word.load(SeqCst);
        if srw_free(state, exclusive) {
            continue;
        }
        if state & SRW_SLEEPERS != 0
            || word.compare_exchange(state, state | SRW_SLEEPERS, SeqCst, SeqCst).is_ok()
        {
            let _blocking = Blocking::begin();
            futex_wait(word.as_ptr(), state | SRW_SLEEPERS, Deadline::NEVER);
        }
    }
}

pub extern "win64" fn InitializeSRWLock(lock: *mut SrwLock) {
    // SAFETY: the title's lock.
    unsafe { lock.write(0) };
}

pub extern "win64" fn AcquireSRWLockExclusive(lock: *mut SrwLock) {
    srw_acquire(lock, true);
}

pub extern "win64" fn AcquireSRWLockShared(lock: *mut SrwLock) {
    srw_acquire(lock, false);
}

pub extern "win64" fn TryAcquireSRWLockExclusive(lock: *mut SrwLock) -> u8 {
    srw_try(lock, true).into()
}

pub extern "win64" fn TryAcquireSRWLockShared(lock: *mut SrwLock) -> u8 {
    srw_try(lock, false).into()
}

pub extern "win64" fn ReleaseSRWLockExclusive(lock: *mut SrwLock) {
    let word = srw(lock);
    if word.swap(0, SeqCst) & SRW_SLEEPERS != 0 {
        futex_wake(word.as_ptr(), i32::MAX);
    }
}

pub extern "win64" fn ReleaseSRWLockShared(lock: *mut SrwLock) {
    let word = srw(lock);
    let mut state = word.load(SeqCst);
    loop {
        let mut next = state - SRW_SHARED;
        if next & !SRW_SLEEPERS == 0 {
            next = 0; // the last holder: the sleepers are woken below
        }
        match word.compare_exchange(state, next, SeqCst, SeqCst) {
            Ok(_) => {
                if next == 0 && state & SRW_SLEEPERS != 0 {
                    futex_wake(word.as_ptr(), i32::MAX);
                }
                return;
            }
            Err(now) => state = now,
        }
    }
}

/// CONDITION_VARIABLE: the pointer's low half is a wake sequence (the
/// futex), its high half the number of sleepers a wake may still have to
/// reach, so a wake with nobody asleep is no system call. Only wakes take
/// sleepers off: one that timed out cannot know whether a wake counted it
/// off already, and taking itself off could take the count of a thread
/// that went to sleep in between, whose wake would then be dropped. So the
/// count runs high (one wasted wake call each), never low.
#[repr(C)]
pub struct ConditionVariable {
    sequence: u32,
    sleepers: u32,
}

const CONDITION_VARIABLE_LOCKMODE_SHARED: u32 = 1;

pub extern "win64" fn InitializeConditionVariable(cv: *mut ConditionVariable) {
    // SAFETY: the title's condition variable.
    unsafe { cv.write(ConditionVariable { sequence: 0, sleepers: 0 }) };
}

fn cv_words(cv: *mut ConditionVariable) -> (&'static AtomicU32, &'static AtomicU32) {
    // SAFETY: fields of the title's condition variable.
    unsafe { (word(&raw mut (*cv).sequence), word(&raw mut (*cv).sleepers)) }
}

fn cv_wake(cv: *mut ConditionVariable, all: bool) {
    let (sequence, sleepers) = cv_words(cv);
    let taken = sleepers.fetch_update(SeqCst, SeqCst, |n| match n {
        0 => None,
        _ if all => Some(0),
        n => Some(n - 1),
    });
    if taken.is_ok() {
        sequence.fetch_add(1, SeqCst);
        futex_wake(sequence.as_ptr(), if all { i32::MAX } else { 1 });
    }
}

pub extern "win64" fn WakeConditionVariable(cv: *mut ConditionVariable) {
    cv_wake(cv, false);
}

pub extern "win64" fn WakeAllConditionVariable(cv: *mut ConditionVariable) {
    cv_wake(cv, true);
}

/// Called with the caller's lock held, which `unlock` and `relock` are;
/// it is held again on return, after a wake (1) or the time (0,
/// `ERROR_TIMEOUT`).
fn cv_sleep(cv: *mut ConditionVariable, ms: u32, unlock: impl FnOnce(), relock: impl FnOnce()) -> i32 {
    let (sequence, sleepers) = cv_words(cv);
    // Read before counting: a wake that takes this thread's count then
    // bumps the sequence after this read, so the sleep below ends.
    let seen = sequence.load(SeqCst);
    let _ = sleepers.fetch_update(SeqCst, SeqCst, |n| n.checked_add(1)); // stays at the top
    unlock();
    let woken = {
        let _blocking = Blocking::begin();
        futex_wait(sequence.as_ptr(), seen, Deadline::after_ms(ms))
    };
    relock();
    if !woken {
        set_last_error(ERROR_TIMEOUT);
    }
    woken.into()
}

pub extern "win64" fn SleepConditionVariableCS(
    cv: *mut ConditionVariable,
    cs: *mut CriticalSection,
    ms: u32,
) -> i32 {
    cv_sleep(cv, ms, || LeaveCriticalSection(cs), || EnterCriticalSection(cs))
}

pub extern "win64" fn SleepConditionVariableSRW(
    cv: *mut ConditionVariable,
    lock: *mut SrwLock,
    ms: u32,
    flags: u32,
) -> i32 {
    let shared = flags & CONDITION_VARIABLE_LOCKMODE_SHARED != 0;
    cv_sleep(
        cv,
        ms,
        || {
            if shared {
                ReleaseSRWLockShared(lock)
            } else {
                ReleaseSRWLockExclusive(lock)
            }
        },
        || srw_acquire(lock, !shared),
    )
}

/// `PINIT_ONCE_FN`.
pub type InitOnceFn = unsafe extern "win64" fn(once: *mut usize, parameter: usize, context: *mut usize) -> i32;

/// INIT_ONCE, the whole pointer: 0 not run, 1 running, 3 running with a
/// sleeper, and when done the callback's context with 2 in its low bits (as
/// Windows stores it). A callback that fails leaves 0: the next caller runs
/// it again.
pub extern "win64" fn InitOnceExecuteOnce(
    once: *mut usize,
    callback: InitOnceFn,
    parameter: usize,
    context: *mut usize,
) -> i32 {
    // SAFETY: the title's INIT_ONCE, pointer-aligned.
    let state = unsafe { AtomicU64::from_ptr(once.cast()) };
    loop {
        let now = state.load(SeqCst);
        if now & 3 == 2 {
            if !context.is_null() {
                // SAFETY: the title's out pointer.
                unsafe { context.write_unaligned((now & !3) as usize) };
            }
            return 1;
        }
        if now == 0 {
            if state.compare_exchange(0, 1, SeqCst, SeqCst).is_err() {
                continue;
            }
            let mut result = 0usize;
            // SAFETY: the title's callback; no lock is held, others sleep.
            let ok = unsafe { callback(once, parameter, &mut result) } != 0;
            let done = if ok { (result as u64 & !3) | 2 } else { 0 };
            if state.swap(done, SeqCst) == 3 {
                futex_wake(once.cast(), i32::MAX);
            }
            if ok && !context.is_null() {
                // SAFETY: the title's out pointer.
                unsafe { context.write_unaligned(result) };
            }
            return ok.into();
        }
        if now == 1 && state.compare_exchange(1, 3, SeqCst, SeqCst).is_err() {
            continue;
        }
        let _blocking = Blocking::begin();
        // The low half holds 3 while it runs (little-endian).
        futex_wait(once.cast(), 3, Deadline::NEVER);
    }
}

// WaitOnAddress: waiters sleep on the sequence of a bucket the address
// hashes to; a wake bumps it and wakes every sleeper of the bucket (a wake
// may be spurious, as the API allows), so one address never steals
// another's wake. The waiter count spares the system call when nobody
// sleeps (STL atomics notify on every store).
struct Bucket {
    sequence: AtomicU32,
    waiters: AtomicU32,
}

static BUCKETS: [Bucket; 256] = [const {
    Bucket {
        sequence: AtomicU32::new(0),
        waiters: AtomicU32::new(0),
    }
}; 256];

fn bucket(address: usize) -> &'static Bucket {
    &BUCKETS[(address >> 3).wrapping_mul(0x9E37_79B9) % BUCKETS.len()]
}

pub extern "win64" fn WaitOnAddress(address: *const u8, compare: *const u8, size: usize, ms: u32) -> i32 {
    if !matches!(size, 1 | 2 | 4 | 8) {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let b = bucket(address as usize);
    b.waiters.fetch_add(1, SeqCst);
    let seen = b.sequence.load(SeqCst);
    // SAFETY: the title's `size`-byte values; read volatile, the address is shared.
    let same = (0..size).all(|i| unsafe {
        address.add(i).read_volatile() == compare.add(i).read_volatile()
    });
    let woken = !same || {
        let _blocking = Blocking::begin();
        futex_wait(b.sequence.as_ptr(), seen, Deadline::after_ms(ms))
    };
    b.waiters.fetch_sub(1, SeqCst);
    if !woken {
        set_last_error(ERROR_TIMEOUT);
    }
    woken.into()
}

fn wake_address(address: *const u8) {
    let b = bucket(address as usize);
    b.sequence.fetch_add(1, SeqCst);
    if b.waiters.load(SeqCst) != 0 {
        futex_wake(b.sequence.as_ptr(), i32::MAX);
    }
}

pub extern "win64" fn WakeByAddressSingle(address: *const u8) {
    wake_address(address);
}

pub extern "win64" fn WakeByAddressAll(address: *const u8) {
    wake_address(address);
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-sync",
        dlls: &["kernel32.dll", "kernelbase.dll", "api-ms-win-core-synch-*"],
        exports: vec![
            hle_export!(CreateEventW),
            hle_export!(CreateEventA),
            hle_export!(CreateEventExW),
            hle_export!(SetEvent),
            hle_export!(ResetEvent),
            hle_export!(CreateMutexW),
            hle_export!(CreateMutexA),
            hle_export!(ReleaseMutex),
            hle_export!(CreateSemaphoreW),
            hle_export!(CreateSemaphoreExW),
            hle_export!(ReleaseSemaphore),
            hle_export!(InitializeCriticalSection),
            hle_export!(InitializeCriticalSectionAndSpinCount),
            hle_export!(InitializeCriticalSectionEx),
            hle_export!(DeleteCriticalSection),
            hle_export!(EnterCriticalSection),
            hle_export!(TryEnterCriticalSection),
            hle_export!(LeaveCriticalSection),
            hle_export!(InitializeSRWLock),
            hle_export!(AcquireSRWLockExclusive),
            hle_export!(AcquireSRWLockShared),
            hle_export!(TryAcquireSRWLockExclusive),
            hle_export!(TryAcquireSRWLockShared),
            hle_export!(ReleaseSRWLockExclusive),
            hle_export!(ReleaseSRWLockShared),
            hle_export!(InitializeConditionVariable),
            hle_export!(WakeConditionVariable),
            hle_export!(WakeAllConditionVariable),
            hle_export!(SleepConditionVariableCS),
            hle_export!(SleepConditionVariableSRW),
            hle_export!(InitOnceExecuteOnce),
            hle_export!(WaitOnAddress),
            hle_export!(WakeByAddressSingle),
            hle_export!(WakeByAddressAll),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::handles::{WAIT_ABANDONED_0, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use crate::linux::kernel32_wait::{CloseHandle, WaitForSingleObject};
    use std::mem::{offset_of, size_of};
    use std::time::{Duration, Instant};
    use victory_loader::linux::teb::last_error;

    #[test]
    fn guest_layouts_match_windows() {
        assert_eq!(size_of::<CriticalSection>(), 40);
        assert_eq!(offset_of!(CriticalSection, lock_count), 8);
        assert_eq!(offset_of!(CriticalSection, recursion_count), 12);
        assert_eq!(offset_of!(CriticalSection, owning_thread), 16);
        assert_eq!(offset_of!(CriticalSection, lock_semaphore), 24);
        assert_eq!(offset_of!(CriticalSection, spin_count), 32);
        assert_eq!(size_of::<SrwLock>(), 8);
        assert_eq!(size_of::<ConditionVariable>(), 8);
    }

    #[test]
    fn named_events_open_the_same_object() {
        let name: Vec<u16> = "sync-test-event\0".encode_utf16().collect();
        let a = CreateEventW(0, 1, 0, name.as_ptr());
        assert_eq!(last_error(), 0);
        let b = CreateEventExW(0, name.as_ptr(), 0, 0);
        assert_eq!(last_error(), ERROR_ALREADY_EXISTS);
        assert_eq!(CreateMutexW(0, 0, name.as_ptr()), 0);
        assert_eq!(SetEvent(a), 1);
        assert_eq!(WaitForSingleObject(b, 0), WAIT_OBJECT_0);
        assert_eq!(ResetEvent(b), 1);
        assert_eq!(WaitForSingleObject(a, 0), WAIT_TIMEOUT);
        assert_eq!(SetEvent(0), 0);
        assert!(CloseHandle(a) == 1 && CloseHandle(b) == 1);
    }

    #[test]
    fn mutex_and_semaphore_handles() {
        let m = CreateMutexA(0, 1, std::ptr::null());
        assert_eq!(WaitForSingleObject(m, 0), WAIT_OBJECT_0);
        assert!(ReleaseMutex(m) == 1 && ReleaseMutex(m) == 1 && ReleaseMutex(m) == 0);
        assert_eq!(last_error(), ERROR_NOT_OWNER);
        std::thread::spawn(move || {
            assert_eq!(WaitForSingleObject(m, 0), WAIT_OBJECT_0);
            handles::current_thread();
        })
        .join()
        .unwrap();
        assert_eq!(WaitForSingleObject(m, 0), WAIT_ABANDONED_0);
        assert_eq!(ReleaseMutex(m), 1);

        assert_eq!(CreateSemaphoreW(0, 3, 2, std::ptr::null()), 0);
        let s = CreateSemaphoreW(0, 1, 2, std::ptr::null());
        let mut previous = -1;
        assert_eq!(ReleaseSemaphore(s, 2, &mut previous), 0);
        assert_eq!(last_error(), ERROR_TOO_MANY_POSTS);
        assert_eq!(ReleaseSemaphore(s, 1, &mut previous), 1);
        assert_eq!(previous, 1);
        assert!(CloseHandle(m) == 1 && CloseHandle(s) == 1);
    }

    #[test]
    fn critical_section_excludes_and_recurses() {
        let mut cs = std::mem::MaybeUninit::<CriticalSection>::zeroed();
        let cs = cs.as_mut_ptr() as usize; // a zeroed one works uninitialised too
        InitializeCriticalSection(cs as *mut _);
        EnterCriticalSection(cs as *mut _);
        assert_eq!(TryEnterCriticalSection(cs as *mut _), 1);
        // SAFETY: our own critical section.
        let c = unsafe { &*(cs as *const CriticalSection) };
        assert_eq!((c.recursion_count, c.owning_thread), (2, current_id() as usize));
        assert_eq!(std::thread::spawn(move || TryEnterCriticalSection(cs as *mut _)).join().unwrap(), 0);
        LeaveCriticalSection(cs as *mut _);
        LeaveCriticalSection(cs as *mut _);
        let counter = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let counter = counter.clone();
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        EnterCriticalSection(cs as *mut _);
                        let v = counter.load(SeqCst);
                        counter.store(v + 1, SeqCst);
                        LeaveCriticalSection(cs as *mut _);
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(counter.load(SeqCst), 16000);
        DeleteCriticalSection(cs as *mut _);
    }

    #[test]
    fn srw_lock_shares_and_excludes() {
        let lock = Box::leak(Box::new(0usize)) as *mut usize as usize;
        let l = lock as *mut SrwLock;
        AcquireSRWLockShared(l);
        assert_eq!(TryAcquireSRWLockShared(l), 1);
        assert_eq!(TryAcquireSRWLockExclusive(l), 0);
        ReleaseSRWLockShared(l);
        ReleaseSRWLockShared(l);
        assert_eq!(TryAcquireSRWLockExclusive(l), 1);
        assert_eq!(TryAcquireSRWLockShared(l), 0);
        ReleaseSRWLockExclusive(l);
        let counter = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let counter = counter.clone();
                std::thread::spawn(move || {
                    let l = lock as *mut SrwLock;
                    for _ in 0..2000 {
                        if i % 2 == 0 {
                            AcquireSRWLockExclusive(l);
                            let v = counter.load(SeqCst);
                            counter.store(v + 1, SeqCst);
                            ReleaseSRWLockExclusive(l);
                        } else {
                            AcquireSRWLockShared(l);
                            let _ = counter.load(SeqCst);
                            ReleaseSRWLockShared(l);
                        }
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(counter.load(SeqCst), 8000);
        // SAFETY: our own lock word.
        assert_eq!(unsafe { *(lock as *const usize) }, 0);
    }

    /// Threads passing a turn round with a CS and WakeAll after the unlock:
    /// a lost wake hangs it.
    #[test]
    fn condition_variable_passes_turns_without_losing_wakes() {
        #[repr(C)]
        struct Shared {
            cs: CriticalSection,
            cv: ConditionVariable,
            turn: usize,
        }
        // SAFETY: zeroed CS, CV and integer are valid.
        let shared: &'static mut Shared = Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        let s = shared as *mut Shared as usize;
        const THREADS: usize = 8;
        const ROUNDS: usize = 200;
        let threads: Vec<_> = (0..THREADS)
            .map(|me| {
                std::thread::spawn(move || {
                    let s = s as *mut Shared;
                    // SAFETY: fields of the leaked shared block.
                    let (cs, cv) = unsafe { (&raw mut (*s).cs, &raw mut (*s).cv) };
                    for round in 0..ROUNDS {
                        EnterCriticalSection(cs);
                        // SAFETY: `turn` is read and written under the CS.
                        while unsafe { (&raw const (*s).turn).read_volatile() } != round * THREADS + me {
                            SleepConditionVariableCS(cv, cs, 5000);
                        }
                        // SAFETY: as above.
                        unsafe { (*s).turn += 1 };
                        LeaveCriticalSection(cs);
                        WakeAllConditionVariable(cv);
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(shared.turn, THREADS * ROUNDS);
    }

    #[test]
    fn condition_variable_times_out_and_a_timed_out_sleeper_keeps_wakes_working() {
        let lock = Box::leak(Box::new(0usize)) as *mut usize as usize;
        let cv = Box::leak(Box::new(ConditionVariable { sequence: 0, sleepers: 0 })) as *mut _ as usize;
        let (l, c) = (lock as *mut SrwLock, cv as *mut ConditionVariable);
        AcquireSRWLockExclusive(l);
        let start = Instant::now();
        assert_eq!(SleepConditionVariableSRW(c, l, 20, 0), 0);
        assert_eq!(last_error(), ERROR_TIMEOUT);
        assert!(start.elapsed() >= Duration::from_millis(20));
        ReleaseSRWLockExclusive(l);
        // The timed-out sleeper is still counted: a new one must still be woken.
        let flag = Arc::new(AtomicU32::new(0));
        let f2 = flag.clone();
        let t = std::thread::spawn(move || {
            let (l, c) = (lock as *mut SrwLock, cv as *mut ConditionVariable);
            AcquireSRWLockShared(l);
            while f2.load(SeqCst) == 0 {
                assert_eq!(SleepConditionVariableSRW(c, l, 5000, CONDITION_VARIABLE_LOCKMODE_SHARED), 1);
            }
            ReleaseSRWLockShared(l);
        });
        std::thread::sleep(Duration::from_millis(30));
        AcquireSRWLockExclusive(l);
        flag.store(1, SeqCst);
        ReleaseSRWLockExclusive(l);
        WakeConditionVariable(c);
        WakeConditionVariable(c);
        t.join().unwrap();
    }

    #[test]
    fn init_once_runs_once_and_retries_after_failure() {
        static RUNS: AtomicU32 = AtomicU32::new(0);
        unsafe extern "win64" fn init(_: *mut usize, fail: usize, context: *mut usize) -> i32 {
            RUNS.fetch_add(1, SeqCst);
            std::thread::sleep(Duration::from_millis(10));
            // SAFETY: InitOnceExecuteOnce's out pointer.
            unsafe { *context = 0x1000 };
            (fail == 0).into()
        }
        let once = Box::leak(Box::new(0usize)) as *mut usize as usize;
        let mut context = 0;
        assert_eq!(InitOnceExecuteOnce(once as *mut usize, init, 1, &mut context), 0);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(move || {
                    let mut context = 0;
                    assert_eq!(InitOnceExecuteOnce(once as *mut usize, init, 0, &mut context), 1);
                    context
                })
            })
            .collect();
        for t in threads {
            assert_eq!(t.join().unwrap(), 0x1000);
        }
        assert_eq!(RUNS.load(SeqCst), 2);
    }

    #[test]
    fn wait_on_address_returns_on_change_wake_or_time() {
        let value = Box::leak(Box::new(AtomicU32::new(5)));
        let v = value as *const AtomicU32 as usize;
        let five = 5u32;
        let six = 6u32;
        assert_eq!(WaitOnAddress(v as *const u8, (&raw const six).cast(), 4, 1000), 1);
        assert_eq!(WaitOnAddress(v as *const u8, (&raw const five).cast(), 4, 10), 0);
        assert_eq!(last_error(), ERROR_TIMEOUT);
        let t = std::thread::spawn(move || {
            let five = 5u32;
            // SAFETY: the leaked value.
            let value = unsafe { &*(v as *const AtomicU32) };
            while value.load(SeqCst) == 5 {
                WaitOnAddress(v as *const u8, (&raw const five).cast(), 4, 5000);
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        value.store(7, SeqCst);
        WakeByAddressSingle(v as *const u8);
        t.join().unwrap();
        assert_eq!(WaitOnAddress(v as *const u8, (&raw const five).cast(), 3, 0), 0);
    }
}
