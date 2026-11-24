//! The handle table: every kernel object title code holds (files, events,
//! threads, ...) behind one kind of `HANDLE`, so `CloseHandle`,
//! `DuplicateHandle` and the waits work on all of them. Also the wait
//! engine, thread objects, thread suspension and the futex helpers the
//! user-mode locks are built on.
//!
//! Adding a kind of object (the file and socket slices):
//!
//! ```text
//! struct File { .. }
//! impl Object for File {
//!     fn as_waitable(&self) -> Option<&dyn Waitable> { Some(self) } // if it is waitable
//! }
//! impl Waitable for File {
//!     fn try_acquire(&self) -> bool { self.done.load(Relaxed) }
//! }
//! let h = handles::insert(Arc::new(File { .. }));      // CreateFileW
//! let file = handles::get::<File>(h)?;                 // ReadFile
//! handles::update(&*file, || file.done.store(true, Relaxed)); // wakes its waiters
//! ```
//!
//! - Handle values are 4, 8, 12, ... and reused after `close`; 0 and -1
//!   are never handed out. -1 is the current process, -2 the current
//!   thread (`object` resolves both, `close` accepts them).
//! - The table holds one `Arc` per handle; `get` gives the caller another,
//!   so a `CloseHandle` elsewhere never frees an object a call is using.
//!   `Drop` is the "last handle closed" hook.
//! - Waits: one lock (the wait lock) covers the signalled state of every
//!   object. `try_acquire` and `signalled` run with it held, on the
//!   waiting thread; they must not block or take a lock that is ever held
//!   around a call into this module. State they read changes only inside
//!   [`update`], which wakes the threads waiting for that object; each
//!   re-checks its wait itself. A suspended thread takes nothing: it checks
//!   its suspend count under the wait lock before it tries.

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use victory_loader::linux::thread as guest_thread;

/// A title `HANDLE` value.
pub(crate) type Handle = usize;

/// `GetCurrentProcess()`, also `INVALID_HANDLE_VALUE`.
pub(crate) const CURRENT_PROCESS: Handle = usize::MAX;
/// `GetCurrentThread()`.
pub(crate) const CURRENT_THREAD: Handle = usize::MAX - 1;

pub(crate) const WAIT_OBJECT_0: u32 = 0;
pub(crate) const WAIT_ABANDONED_0: u32 = 0x80;
pub(crate) const WAIT_TIMEOUT: u32 = 258;
pub(crate) const WAIT_FAILED: u32 = u32::MAX;
pub(crate) const INFINITE: u32 = u32::MAX;
pub(crate) const STILL_ACTIVE: u32 = 259;

/// A kernel object. Waitable objects say so through `as_waitable`.
pub(crate) trait Object: Any + Send + Sync {
    /// `Some` for objects `WaitForSingleObject` accepts.
    fn as_waitable(&self) -> Option<&dyn Waitable> {
        None
    }

    /// `SetEvent` (true) or `ResetEvent` (false) of an event, waking its
    /// waiters; false for objects that are not events. The file layer
    /// signals an `OVERLAPPED`'s event through it.
    fn set_signal(&self, _signalled: bool) -> bool {
        false
    }
}

/// An object a thread can wait for. Both methods run under the wait lock
/// on the waiting thread (see the module docs).
pub(crate) trait Waitable: Send + Sync {
    /// Whether a wait would be satisfied now; takes the object (an
    /// auto-reset event's signal, a semaphore count) when it is.
    fn try_acquire(&self) -> bool;
    /// Whether a wait would be satisfied now, without taking anything (the
    /// all-of check of `WaitForMultipleObjects`). The default is right for
    /// objects a wait does not consume.
    fn signalled(&self) -> bool {
        self.try_acquire()
    }
    /// Right after a `try_acquire` that succeeded: whether it took an
    /// abandoned mutex (`WAIT_ABANDONED`).
    fn take_abandoned(&self) -> bool {
        false
    }
}

// ---- the table ----

#[derive(Default)]
struct Table {
    slots: Vec<Option<Arc<dyn Object>>>, // handle = (index + 1) * 4
    free: Vec<usize>,
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(Default::default);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A new handle to `object`. Handle values are multiples of 4 from 4.
pub(crate) fn insert<T: Object>(object: Arc<T>) -> Handle {
    insert_dyn(object)
}

pub(crate) fn insert_dyn(object: Arc<dyn Object>) -> Handle {
    let mut table = lock(&TABLE);
    let index = match table.free.pop() {
        Some(index) => {
            table.slots[index] = Some(object);
            index
        }
        None => {
            table.slots.push(Some(object));
            table.slots.len() - 1
        }
    };
    (index + 1) << 2
}

/// The object behind `handle`, if it is a `T`.
pub(crate) fn get<T: Object>(handle: Handle) -> Option<Arc<T>> {
    let object: Arc<dyn Any + Send + Sync> = object(handle)?;
    object.downcast().ok()
}

/// The object behind `handle` or a pseudo handle, whatever it is.
pub(crate) fn object(handle: Handle) -> Option<Arc<dyn Object>> {
    match handle {
        CURRENT_PROCESS => return Some(PROCESS.clone()),
        CURRENT_THREAD => return Some(current_thread()),
        _ => {}
    }
    // The two low bits are ignored, as on Windows.
    let index = (handle >> 2).checked_sub(1)?;
    lock(&TABLE).slots.get(index)?.clone()
}

/// Closes `handle`; false if it was not open. Pseudo handles: true.
pub(crate) fn close(handle: Handle) -> bool {
    if handle == CURRENT_PROCESS || handle == CURRENT_THREAD {
        return true;
    }
    let Some(index) = (handle >> 2).checked_sub(1) else {
        return false;
    };
    let last = {
        let mut table = lock(&TABLE);
        let last = table.slots.get_mut(index).and_then(Option::take);
        if last.is_some() {
            table.free.push(index);
        }
        last
    };
    // Dropped here, outside the table lock.
    last.is_some()
}

/// Named objects (`CreateEventW(.., name)` and friends): the live object
/// registered under `name`, else `make()`'s, registered. The bool says
/// whether it existed. Names are case sensitive and live as long as their
/// object.
pub(crate) fn open_named(
    name: &str,
    make: impl FnOnce() -> Arc<dyn Object>,
) -> (Arc<dyn Object>, bool) {
    static NAMES: LazyLock<Mutex<HashMap<String, Weak<dyn Object>>>> =
        LazyLock::new(Default::default);
    let mut names = lock(&NAMES);
    names.retain(|_, object| object.strong_count() > 0);
    if let Some(object) = names.get(name).and_then(Weak::upgrade) {
        return (object, true);
    }
    let object = make();
    names.insert(name.to_owned(), Arc::downgrade(&object));
    (object, false)
}

/// The current process, the object behind -1. Never signalled.
pub(crate) struct Process;
impl Object for Process {
    fn as_waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}
impl Waitable for Process {
    fn try_acquire(&self) -> bool {
        false
    }
}
static PROCESS: LazyLock<Arc<dyn Object>> = LazyLock::new(|| Arc::new(Process));

// ---- futexes ----

/// A point on `CLOCK_MONOTONIC`, or never.
#[derive(Clone, Copy)]
pub(crate) struct Deadline(Option<libc::timespec>);

impl Deadline {
    pub(crate) const NEVER: Deadline = Deadline(None);

    /// `milliseconds` from now; `INFINITE` is never.
    pub(crate) fn after_ms(milliseconds: u32) -> Deadline {
        if milliseconds == INFINITE {
            return Deadline::NEVER;
        }
        Deadline::after(Duration::from_millis(milliseconds.into()))
    }

    pub(crate) fn after(duration: Duration) -> Deadline {
        let now = monotonic();
        let ns = now.tv_nsec as u64 + u64::from(duration.subsec_nanos());
        let secs = (now.tv_sec as u64)
            .saturating_add(duration.as_secs())
            .saturating_add(ns / 1_000_000_000)
            .min(i64::MAX as u64 / 2);
        Deadline(Some(libc::timespec {
            tv_sec: secs as libc::time_t,
            tv_nsec: (ns % 1_000_000_000) as libc::c_long,
        }))
    }

    pub(crate) fn passed(&self) -> bool {
        self.0.is_some_and(|at| {
            let now = monotonic();
            (now.tv_sec, now.tv_nsec) >= (at.tv_sec, at.tv_nsec)
        })
    }
}

fn monotonic() -> libc::timespec {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now
}

/// Sleeps while `*word == expected`. False only when the deadline passed;
/// true for a wake, a changed value, a signal or nothing at all, so every
/// caller re-checks its condition in a loop. Async-signal-safe.
pub(crate) fn futex_wait(word: *const u32, expected: u32, deadline: Deadline) -> bool {
    let at = deadline.0.as_ref().map_or(std::ptr::null(), |t| t as *const _);
    // SAFETY: FUTEX_WAIT_BITSET only reads `word`, which the caller keeps
    // mapped; `at` is null or a valid absolute CLOCK_MONOTONIC time.
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word,
            libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG,
            expected,
            at,
            std::ptr::null::<u32>(),
            libc::FUTEX_BITSET_MATCH_ANY,
        )
    };
    !(r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ETIMEDOUT))
}

/// Wakes up to `count` threads asleep on `word`. Harmless on any address.
pub(crate) fn futex_wake(word: *const u32, count: i32) {
    // SAFETY: FUTEX_WAKE does not access `word`'s memory.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            count,
        )
    };
}

// ---- waits ----

/// The wait lock and the threads asleep on each object (by its address).
static WAITS: LazyLock<Mutex<HashMap<usize, Vec<usize>>>> = LazyLock::new(Default::default);

thread_local! {
    /// What wakers bump (under the wait lock) to end this thread's sleep.
    static WAKE: AtomicU32 = const { AtomicU32::new(0) };
}

fn key(object: &dyn Waitable) -> usize {
    object as *const dyn Waitable as *const () as usize
}

/// Runs `change` under the wait lock, then wakes every thread waiting for
/// `object`. The only way to change what `try_acquire` reads.
pub(crate) fn update<R>(object: &dyn Waitable, change: impl FnOnce() -> R) -> R {
    let waits = lock(&WAITS);
    let r = change();
    #[cfg(feature = "diagnostics")]
    if lock(&LONG_WAITS).contains(&key(object)) {
        tracing::warn!(object = key(object), thread = current_thread().id(), "a long-waited object changes");
    }
    let mut wake = [0usize; 16];
    let mut n = 0;
    for &word in waits.get(&key(object)).into_iter().flatten() {
        // SAFETY: a linked word belongs to a thread asleep in `wait`, which
        // unlinks it (under this lock) before it returns or exits.
        unsafe { &*(word as *const AtomicU32) }.fetch_add(1, Ordering::SeqCst);
        if n < wake.len() {
            wake[n] = word;
            n += 1;
        } else {
            futex_wake(word as *const u32, 1);
        }
    }
    drop(waits);
    // A woken thread may be gone already: a wake of its old word wakes nobody.
    for &word in &wake[..n] {
        futex_wake(word as *const u32, 1);
    }
    r
}

/// Objects a wait has waited for over 10 s (diagnostics).
#[cfg(feature = "diagnostics")]
static LONG_WAITS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Logs a long wait's objects and watches them (`begin`), or stops.
#[cfg(feature = "diagnostics")]
pub(crate) fn long_wait(list: &[Handle], objects: &[&dyn Waitable], begin: bool) {
    let mut watched = lock(&LONG_WAITS);
    for (&handle, &object) in list.iter().zip(objects) {
        if begin {
            let kind = if get::<Event>(handle).is_some() {
                "event"
            } else if get::<Semaphore>(handle).is_some() {
                "semaphore"
            } else if get::<KMutex>(handle).is_some() {
                "mutex"
            } else if get::<Thread>(handle).is_some() {
                "thread"
            } else {
                "other"
            };
            tracing::warn!(handle, kind, object = key(object), signalled = object.signalled(), thread = current_thread().id(), "waiting over 10 s");
            watched.push(key(object));
        } else if let Some(i) = watched.iter().position(|&k| k == key(object)) {
            watched.swap_remove(i);
        }
    }
}

/// `WaitForMultipleObjects` on objects the caller keeps alive:
/// `WAIT_OBJECT_0 + i`, `WAIT_ABANDONED_0 + i` or `WAIT_TIMEOUT`. With
/// `all`, the index is 0 (the abandoned form names the first abandoned
/// mutex); the callers rule out an object given twice.
pub(crate) fn wait(objects: &[&dyn Waitable], all: bool, timeout_ms: u32) -> u32 {
    let deadline = Deadline::after_ms(timeout_ms);
    let me = current_thread();
    let word = WAKE.with(|w| w as *const AtomicU32 as usize);
    let _blocking = Blocking::begin();
    let mut linked = false;
    loop {
        let seq;
        {
            let mut waits = lock(&WAITS);
            let result = if me.suspend.load(Ordering::SeqCst) == 0 {
                try_satisfy(objects, all)
            } else {
                None
            };
            let result = result.or_else(|| {
                (timeout_ms == 0 || deadline.passed()).then_some(WAIT_TIMEOUT)
            });
            if let Some(result) = result {
                if linked {
                    for object in objects {
                        if let Some(list) = waits.get_mut(&key(*object)) {
                            list.retain(|&w| w != word);
                            if list.is_empty() {
                                waits.remove(&key(*object));
                            }
                        }
                    }
                }
                return result;
            }
            if !linked {
                for object in objects {
                    waits.entry(key(*object)).or_default().push(word);
                }
                linked = true;
            }
            // SAFETY: our own thread-local.
            seq = unsafe { &*(word as *const AtomicU32) }.load(Ordering::SeqCst);
        }
        futex_wait(word as *const u32, seq, deadline);
        park_if_suspended(&me);
    }
}

fn try_satisfy(objects: &[&dyn Waitable], all: bool) -> Option<u32> {
    if !all {
        return objects.iter().enumerate().find_map(|(i, o)| {
            o.try_acquire().then(|| {
                let base = if o.take_abandoned() { WAIT_ABANDONED_0 } else { WAIT_OBJECT_0 };
                base + i as u32
            })
        });
    }
    if !objects.iter().all(|o| o.signalled()) {
        return None;
    }
    let mut result = WAIT_OBJECT_0;
    for (i, o) in objects.iter().enumerate().rev() {
        let taken = o.try_acquire();
        debug_assert!(taken, "signalled but not acquirable");
        if o.take_abandoned() {
            result = WAIT_ABANDONED_0 + i as u32;
        }
    }
    Some(result)
}

// ---- events, semaphores, mutexes ----

/// An event (`CreateEventW`).
pub(crate) struct Event {
    manual: bool,
    set: AtomicBool,
}

impl Event {
    pub(crate) fn new(manual_reset: bool, initially_set: bool) -> Arc<Event> {
        Arc::new(Event {
            manual: manual_reset,
            set: AtomicBool::new(initially_set),
        })
    }
    pub(crate) fn set(&self) {
        update(self, || self.set.store(true, Ordering::Relaxed));
    }
    pub(crate) fn reset(&self) {
        update(self, || self.set.store(false, Ordering::Relaxed));
    }
}
impl Object for Event {
    fn as_waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }

    fn set_signal(&self, signalled: bool) -> bool {
        if signalled { self.set() } else { self.reset() }
        true
    }
}
impl Waitable for Event {
    fn try_acquire(&self) -> bool {
        if self.manual {
            self.set.load(Ordering::Relaxed)
        } else {
            self.set.swap(false, Ordering::Relaxed)
        }
    }
    fn signalled(&self) -> bool {
        self.set.load(Ordering::Relaxed)
    }
}

/// A semaphore (`CreateSemaphoreW`).
pub(crate) struct Semaphore {
    count: AtomicI32,
    maximum: i32,
}

impl Semaphore {
    pub(crate) fn new(initial: i32, maximum: i32) -> Arc<Semaphore> {
        Arc::new(Semaphore {
            count: AtomicI32::new(initial),
            maximum,
        })
    }
    /// `ReleaseSemaphore`: the previous count, or `None` (nothing changed)
    /// when the count would pass the maximum.
    pub(crate) fn release(&self, count: i32) -> Option<i32> {
        update(self, || {
            let now = self.count.load(Ordering::Relaxed);
            (count > 0 && count <= self.maximum - now).then(|| {
                self.count.store(now + count, Ordering::Relaxed);
                now
            })
        })
    }
}
impl Object for Semaphore {
    fn as_waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}
impl Waitable for Semaphore {
    fn try_acquire(&self) -> bool {
        let now = self.count.load(Ordering::Relaxed);
        (now > 0).then(|| self.count.store(now - 1, Ordering::Relaxed)).is_some()
    }
    fn signalled(&self) -> bool {
        self.count.load(Ordering::Relaxed) > 0
    }
}

/// A mutex (`CreateMutexW`): recursive, owned by a thread. When its owner
/// exits holding it, it becomes free and the next wait that gets it
/// reports `WAIT_ABANDONED` once.
pub(crate) struct KMutex {
    owner: AtomicU32, // thread id, 0 when free
    recursion: AtomicU32,
    abandoned: AtomicBool,
    took_abandoned: AtomicBool,
}

/// Every mutex, for abandonment at thread exit.
// ponytail: a scan of all mutexes per thread exit; an owned list per thread if titles make thousands.
static MUTEXES: Mutex<Vec<Weak<KMutex>>> = Mutex::new(Vec::new());

impl KMutex {
    pub(crate) fn new(owned: bool) -> Arc<KMutex> {
        // An owner needs a thread object, which abandons the mutex at its exit.
        let me = if owned { current_thread().id() } else { 0 };
        let m = Arc::new(KMutex {
            owner: AtomicU32::new(me),
            recursion: AtomicU32::new(owned.into()),
            abandoned: AtomicBool::new(false),
            took_abandoned: AtomicBool::new(false),
        });
        let mut all = lock(&MUTEXES);
        all.retain(|m| m.strong_count() > 0);
        all.push(Arc::downgrade(&m));
        m
    }
    /// `ReleaseMutex`: false when the calling thread does not own it.
    pub(crate) fn release(&self) -> bool {
        let me = guest_thread::current_id();
        update(self, || {
            if self.owner.load(Ordering::Relaxed) != me {
                return false;
            }
            if self.recursion.fetch_sub(1, Ordering::Relaxed) == 1 {
                self.owner.store(0, Ordering::Relaxed);
            }
            true
        })
    }
}
impl Object for KMutex {
    fn as_waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}
impl Waitable for KMutex {
    fn try_acquire(&self) -> bool {
        let me = guest_thread::current_id();
        let owner = self.owner.load(Ordering::Relaxed);
        if owner == me {
            self.recursion.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if owner != 0 {
            return false;
        }
        self.owner.store(me, Ordering::Relaxed);
        self.recursion.store(1, Ordering::Relaxed);
        let abandoned = self.abandoned.swap(false, Ordering::Relaxed);
        self.took_abandoned.store(abandoned, Ordering::Relaxed);
        true
    }
    fn signalled(&self) -> bool {
        matches!(self.owner.load(Ordering::Relaxed), 0)
            || self.owner.load(Ordering::Relaxed) == guest_thread::current_id()
    }
    fn take_abandoned(&self) -> bool {
        self.took_abandoned.swap(false, Ordering::Relaxed)
    }
}

// ---- threads ----

/// A thread as title code sees it: signalled once it has exited, its exit
/// code kept while a handle is open. Threads made by `CreateThread` get one
/// at creation; any other thread the first time it needs one.
pub(crate) struct Thread {
    id: AtomicU32,
    exited: AtomicBool, // under the wait lock
    exit_code: AtomicU32,
    /// Futex: a parked thread sleeps while it is non-zero.
    suspend: AtomicU32,
    /// Futex: bumped each time the thread answers a suspend signal.
    ack: AtomicU32,
    answer: AtomicU32,
    /// Serialises `SuspendThread` and `ResumeThread` on this thread.
    control: Mutex<()>,
    /// What title code asked for, read back as set.
    pub(crate) priority: AtomicI32,
    pub(crate) affinity: AtomicU64, // 0: never set
    pub(crate) ideal_processor: AtomicU32,
}

impl Thread {
    pub(crate) fn new(id: u32) -> Arc<Thread> {
        Arc::new(Thread {
            id: AtomicU32::new(id),
            exited: AtomicBool::new(false),
            exit_code: AtomicU32::new(STILL_ACTIVE),
            suspend: AtomicU32::new(0),
            ack: AtomicU32::new(0),
            answer: AtomicU32::new(0),
            control: Mutex::new(()),
            priority: AtomicI32::new(0),
            affinity: AtomicU64::new(0),
            ideal_processor: AtomicU32::new(0),
        })
    }
    pub(crate) fn id(&self) -> u32 {
        self.id.load(Ordering::Acquire)
    }
    /// `GetExitCodeThread`: `STILL_ACTIVE` while it runs.
    pub(crate) fn exit_code(&self) -> u32 {
        self.exit_code.load(Ordering::Acquire)
    }
    pub(crate) fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}
impl Object for Thread {
    fn as_waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}
impl Waitable for Thread {
    fn try_acquire(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }
}

/// Every live thread that has an object, by id.
static THREADS: LazyLock<Mutex<HashMap<u32, Arc<Thread>>>> = LazyLock::new(Default::default);

thread_local! {
    /// The calling thread's object (a strong count lives in `THREADS`);
    /// read by the suspend handler, so a plain const cell.
    static SELF: Cell<*const Thread> = const { Cell::new(std::ptr::null()) };
    static BLOCKING: Cell<u32> = const { Cell::new(0) };
    static FINISH: FinishAtExit = const { FinishAtExit };
}

/// Ends the object of a thread that `CreateThread` did not make (the
/// entry thread, host threads) when the thread ends.
struct FinishAtExit;
impl Drop for FinishAtExit {
    fn drop(&mut self) {
        if let Some(me) = registered_self() {
            finish(&me, 0);
        }
    }
}

fn registered_self() -> Option<Arc<Thread>> {
    let me = SELF.with(Cell::get);
    // SAFETY: SELF is cleared before THREADS drops its reference (finish).
    (!me.is_null()).then(|| unsafe {
        Arc::increment_strong_count(me);
        Arc::from_raw(me)
    })
}

/// The calling thread's object, made on first use.
pub(crate) fn current_thread() -> Arc<Thread> {
    if let Some(me) = registered_self() {
        return me;
    }
    let id = guest_thread::current_id();
    let me = lock(&THREADS).entry(id).or_insert_with(|| Thread::new(id)).clone();
    adopt(&me);
    FINISH.with(|_| {});
    me
}

/// Makes `thread` the calling thread's object (`CreateThread`'s start).
pub(crate) fn adopt(thread: &Arc<Thread>) {
    thread.id.store(guest_thread::current_id(), Ordering::Release);
    lock(&THREADS).insert(thread.id(), thread.clone());
    SELF.with(|s| s.set(Arc::as_ptr(thread)));
}

/// Gives a thread `CreateThread` just started its id, so its handle and
/// `OpenThread` work before the thread itself has run.
pub(crate) fn register(thread: &Arc<Thread>, id: u32) {
    thread.id.store(id, Ordering::Release);
    let mut threads = lock(&THREADS);
    // Under the registry lock: `finish` marks it exited before it locks.
    if !thread.has_exited() {
        threads.insert(id, thread.clone());
    }
}

/// The object of a live thread by id (`OpenThread`).
pub(crate) fn thread_by_id(id: u32) -> Option<Arc<Thread>> {
    lock(&THREADS).get(&id).cloned()
}

/// Marks `thread` (the calling thread) exited with `code`: wakes its
/// waiters and abandons its mutexes. Idempotent.
pub(crate) fn finish(thread: &Arc<Thread>, code: u32) {
    block_suspend_signal();
    let id = thread.id();
    let mutexes: Vec<_> = lock(&MUTEXES).iter().filter_map(Weak::upgrade).collect();
    for m in mutexes.iter().filter(|m| m.owner.load(Ordering::Relaxed) == id) {
        update(&**m, || {
            if m.owner.load(Ordering::Relaxed) == id {
                m.owner.store(0, Ordering::Relaxed);
                m.recursion.store(0, Ordering::Relaxed);
                m.abandoned.store(true, Ordering::Relaxed);
            }
        });
    }
    update(&**thread, || {
        if !thread.exited.load(Ordering::Relaxed) {
            thread.exit_code.store(code, Ordering::Release);
            thread.exited.store(true, Ordering::Release);
        }
    });
    if SELF.with(Cell::get) == Arc::as_ptr(thread) {
        SELF.with(|s| s.set(std::ptr::null()));
    }
    let mut threads = lock(&THREADS);
    if threads.get(&id).is_some_and(|t| Arc::ptr_eq(t, thread)) {
        threads.remove(&id);
    }
}

// ---- suspension ----
//
// SuspendThread stops another thread with a signal. The handler parks the
// thread only where it holds no host lock: in title code (outside every
// host ELF object). Inside a `Blocking` region (every wait and lock sleep
// here) it only answers, and the thread parks when the region ends or before
// it next tries for an object. Anywhere else (host code) it answers "not
// yet" and the suspender signals again, at growing intervals, until the
// thread is parked or in a region.

const ANSWER_HOST: u32 = 0;
const ANSWER_PARKED: u32 = 1;
const ANSWER_WILL_PARK: u32 = 2;

pub(crate) const MAXIMUM_SUSPEND_COUNT: u32 = 127;

fn suspend_signal() -> libc::c_int {
    libc::SIGRTMIN() + 6
}

fn block_suspend_signal() {
    // SAFETY: plain signal-mask calls on a local set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, suspend_signal());
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// A stretch where the thread may sleep for title code, holding no lock
/// when it ends. A suspend that lands inside takes effect at its end.
pub(crate) struct Blocking(());

impl Blocking {
    pub(crate) fn begin() -> Blocking {
        BLOCKING.with(|b| b.set(b.get() + 1));
        Blocking(())
    }
}

impl Drop for Blocking {
    fn drop(&mut self) {
        let depth = BLOCKING.with(|b| {
            b.set(b.get() - 1);
            b.get()
        });
        if depth == 0
            && let Some(me) = registered_self()
        {
            park_if_suspended(&me);
        }
    }
}

/// Sleeps while `thread` (the calling thread) is suspended.
pub(crate) fn park_if_suspended(thread: &Thread) {
    loop {
        let count = thread.suspend.load(Ordering::SeqCst);
        if count == 0 {
            return;
        }
        futex_wait(thread.suspend.as_ptr(), count, Deadline::NEVER);
    }
}

/// Address ranges of host code (every loaded ELF object's executable
/// segments), for the handler; rebuilt by suspenders when objects change.
struct HostCode {
    adds: u64,
    ranges: Vec<(usize, usize)>,
}
static HOST_CODE: AtomicPtr<HostCode> = AtomicPtr::new(std::ptr::null_mut());

fn refresh_host_code() {
    unsafe extern "C" fn each(
        info: *mut libc::dl_phdr_info,
        _: libc::size_t,
        out: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: dl_iterate_phdr hands a valid info; `out` is our Vec.
        let (info, out) = unsafe { (&*info, &mut *out.cast::<HostCode>()) };
        out.adds = info.dlpi_adds;
        for i in 0..info.dlpi_phnum as usize {
            // SAFETY: dlpi_phdr has dlpi_phnum entries.
            let ph = unsafe { &*info.dlpi_phdr.add(i) };
            if ph.p_type == libc::PT_LOAD && ph.p_flags & libc::PF_X != 0 {
                let low = info.dlpi_addr as usize + ph.p_vaddr as usize;
                out.ranges.push((low, low + ph.p_memsz as usize));
            }
        }
        0
    }
    let current = HOST_CODE.load(Ordering::Acquire);
    let mut next = HostCode {
        adds: 0,
        ranges: Vec::new(),
    };
    // SAFETY: `each` matches the callback type and only writes `next`.
    unsafe { libc::dl_iterate_phdr(Some(each), (&raw mut next).cast()) };
    // SAFETY: published snapshots are never freed.
    if current.is_null() || unsafe { &*current }.adds != next.adds {
        // Leaked: the handler may still read an older one. One per change of
        // the loaded objects.
        HOST_CODE.store(Box::into_raw(Box::new(next)), Ordering::Release);
    }
}

fn is_host_code(rip: usize) -> bool {
    let code = HOST_CODE.load(Ordering::Acquire);
    // SAFETY: published snapshots are never freed or changed.
    code.is_null() || unsafe { &*code }.ranges.iter().any(|&(l, h)| (l..h).contains(&rip))
}

extern "C" fn on_suspend_signal(_: libc::c_int, _: *mut libc::siginfo_t, context: *mut libc::c_void) {
    let me = SELF.with(Cell::get);
    if me.is_null() {
        return;
    }
    // SAFETY: SELF stays valid until `finish`, which blocks this signal first.
    let me = unsafe { &*me };
    let answer = |a: u32| {
        me.answer.store(a, Ordering::SeqCst);
        me.ack.fetch_add(1, Ordering::SeqCst);
        futex_wake(me.ack.as_ptr(), i32::MAX);
    };
    if BLOCKING.with(Cell::get) > 0 {
        return answer(ANSWER_WILL_PARK);
    }
    // SAFETY: the kernel hands an SA_SIGINFO handler its ucontext.
    let rip = unsafe { (*context.cast::<libc::ucontext_t>()).uc_mcontext.gregs[libc::REG_RIP as usize] };
    if is_host_code(rip as usize) {
        return answer(ANSWER_HOST);
    }
    answer(ANSWER_PARKED);
    park_if_suspended(me);
}

fn install_suspend_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: installs a handler that is async-signal-safe (atomics and
        // futex calls only).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_suspend_signal as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(suspend_signal(), &action, std::ptr::null_mut());
        }
    });
}

/// `SuspendThread`: the previous suspend count, or `None` past
/// `MAXIMUM_SUSPEND_COUNT`. Returns once the thread is parked or cannot
/// take anything before it parks (or, after about a second, gives up waiting
/// for a thread stuck in host code; it still parks at its next wait).
pub(crate) fn suspend(thread: &Arc<Thread>) -> Option<u32> {
    let me = current_thread();
    if Arc::ptr_eq(thread, &me) {
        let previous = thread.suspend.fetch_add(1, Ordering::SeqCst);
        park_if_suspended(thread);
        return Some(previous);
    }
    let _control = lock(&thread.control);
    let previous = thread.suspend.load(Ordering::SeqCst);
    if previous >= MAXIMUM_SUSPEND_COUNT {
        return None;
    }
    thread.suspend.store(previous + 1, Ordering::SeqCst);
    if previous > 0 || thread.has_exited() {
        return Some(previous);
    }
    install_suspend_handler();
    refresh_host_code();
    let start = Instant::now();
    let mut pause = Duration::from_micros(200);
    while start.elapsed() < Duration::from_secs(1) && !thread.has_exited() {
        let seen = thread.ack.load(Ordering::SeqCst);
        // SAFETY: tgkill to one of our own threads with our own signal.
        unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), thread.id(), suspend_signal()) };
        let until = Deadline::after(Duration::from_millis(20));
        // Until it answers or 20 ms pass.
        while thread.ack.load(Ordering::SeqCst) == seen {
            if !futex_wait(thread.ack.as_ptr(), seen, until) {
                break;
            }
        }
        if thread.ack.load(Ordering::SeqCst) != seen
            && thread.answer.load(Ordering::SeqCst) != ANSWER_HOST
        {
            break;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(8));
    }
    Some(previous)
}

/// `ResumeThread`: the previous suspend count.
pub(crate) fn resume(thread: &Thread) -> u32 {
    let _control = lock(&thread.control);
    let previous = thread.suspend.load(Ordering::SeqCst);
    if previous > 0 {
        thread.suspend.store(previous - 1, Ordering::SeqCst);
        if previous == 1 {
            futex_wake(thread.suspend.as_ptr(), i32::MAX);
        }
    }
    previous
}

/// Sets the suspend count of a thread that has not started yet
/// (`CREATE_SUSPENDED`).
pub(crate) fn start_suspended(thread: &Thread) {
    thread.suspend.store(1, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Thing(u32);
    impl Object for Thing {}

    #[test]
    fn handles_find_their_object_until_closed() {
        let h = insert(Arc::new(Thing(7)));
        assert_eq!(h % 4, 0);
        assert_eq!(get::<Thing>(h).map(|t| t.0), Some(7));
        assert!(object(h).unwrap().as_waitable().is_none());
        assert!(close(h));
        assert!(get::<Thing>(h).is_none() && !close(h));
        assert!(object(0).is_none() && !close(0) && close(CURRENT_THREAD));
        assert!(get::<Process>(CURRENT_PROCESS).is_some());
        assert_eq!(get::<Thread>(CURRENT_THREAD).unwrap().id(), guest_thread::current_id());
    }

    #[test]
    fn named_objects_are_shared_while_alive() {
        let (a, existed) = open_named("handles-test", || Event::new(true, false));
        assert!(!existed);
        let (b, existed) = open_named("handles-test", || Event::new(true, false));
        assert!(existed && Arc::ptr_eq(&a, &b));
        drop((a, b));
        assert!(!open_named("handles-test", || Event::new(true, false)).1);
    }

    #[test]
    fn auto_reset_event_wakes_one_waiter_per_set() {
        let e = Event::new(false, false);
        let woken = Arc::new(AtomicU32::new(0));
        let threads: Vec<_> = (0..3)
            .map(|_| {
                let (e, woken) = (e.clone(), woken.clone());
                std::thread::spawn(move || {
                    assert_eq!(wait(&[&*e], false, 5000), WAIT_OBJECT_0);
                    woken.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(50));
        for n in 1..=3 {
            e.set();
            let start = Instant::now();
            while woken.load(Ordering::SeqCst) < n && start.elapsed() < Duration::from_secs(5) {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(20));
            assert_eq!(woken.load(Ordering::SeqCst), n);
        }
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(wait(&[&*e], false, 0), WAIT_TIMEOUT);
    }

    #[test]
    fn wait_times_out_and_a_late_set_stays() {
        let e = Event::new(false, false);
        let start = Instant::now();
        assert_eq!(wait(&[&*e], false, 30), WAIT_TIMEOUT);
        assert!(start.elapsed() >= Duration::from_millis(30));
        e.set();
        assert_eq!(wait(&[&*e], false, 0), WAIT_OBJECT_0);
    }

    #[test]
    fn wait_all_takes_all_or_nothing() {
        let a = Semaphore::new(1, 10);
        let b = Event::new(false, false);
        assert_eq!(wait(&[&*a, &*b], true, 0), WAIT_TIMEOUT);
        // Nothing was taken from the semaphore by the failed all-of wait.
        assert_eq!(a.release(0), None);
        let (a2, b2) = (a.clone(), b.clone());
        let waiter = std::thread::spawn(move || wait(&[&*a2, &*b2], true, 5000));
        std::thread::sleep(Duration::from_millis(30));
        b.set();
        assert_eq!(waiter.join().unwrap(), WAIT_OBJECT_0);
        assert_eq!(wait(&[&*a], false, 0), WAIT_TIMEOUT);
        assert_eq!(wait(&[&*a, &*b], false, 0), WAIT_TIMEOUT);
        a.release(1);
        assert_eq!(wait(&[&*b, &*a], false, 0), WAIT_OBJECT_0 + 1);
    }

    #[test]
    fn semaphore_counts_to_its_maximum() {
        let s = Semaphore::new(0, 2);
        assert_eq!(s.release(3), None);
        assert_eq!(s.release(2), Some(0));
        assert_eq!(s.release(1), None);
        assert_eq!(wait(&[&*s], false, 0), WAIT_OBJECT_0);
        assert_eq!(wait(&[&*s], false, 0), WAIT_OBJECT_0);
        assert_eq!(wait(&[&*s], false, 0), WAIT_TIMEOUT);
    }

    #[test]
    fn mutex_is_recursive_and_abandoned_by_an_exiting_owner() {
        let owned = std::thread::spawn(|| KMutex::new(true)).join().unwrap();
        assert_eq!(wait(&[&*owned], false, 0), WAIT_ABANDONED_0);
        let m = KMutex::new(false);
        assert_eq!(wait(&[&*m], false, 0), WAIT_OBJECT_0);
        assert_eq!(wait(&[&*m], false, 0), WAIT_OBJECT_0);
        assert!(m.release() && m.release() && !m.release());
        let m2 = m.clone();
        std::thread::spawn(move || {
            assert_eq!(wait(&[&*m2], false, 0), WAIT_OBJECT_0);
            // Another thread cannot release it.
            let m3 = m2.clone();
            assert!(!std::thread::spawn(move || m3.release()).join().unwrap());
            current_thread(); // gets an object, finished at thread exit
        })
        .join()
        .unwrap();
        assert_eq!(wait(&[&*m], false, 0), WAIT_ABANDONED_0);
        assert!(m.release());
        assert_eq!(wait(&[&*m], false, 0), WAIT_OBJECT_0);
        assert!(m.release());
    }

    #[test]
    fn a_waiter_for_a_held_mutex_gets_it_on_release() {
        let m = KMutex::new(true);
        let m2 = m.clone();
        let t = std::thread::spawn(move || {
            let r = wait(&[&*m2], false, 5000);
            assert!(m2.release());
            r
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(m.release());
        assert_eq!(t.join().unwrap(), WAIT_OBJECT_0);
    }

    #[test]
    fn thread_objects_signal_on_exit() {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            tx.send(current_thread()).unwrap();
            std::thread::sleep(Duration::from_millis(30));
        });
        let thread = rx.recv().unwrap();
        assert_eq!(thread.exit_code(), STILL_ACTIVE);
        assert!(thread_by_id(thread.id()).is_some());
        assert_eq!(wait(&[&*thread], false, 5000), WAIT_OBJECT_0);
        assert_eq!(thread.exit_code(), 0);
        t.join().unwrap();
        assert!(thread_by_id(thread.id()).is_none());
    }

    #[test]
    fn a_suspended_waiter_takes_nothing_until_resumed() {
        let e = Event::new(false, false);
        let (tx, rx) = std::sync::mpsc::channel();
        let e2 = e.clone();
        let t = std::thread::spawn(move || {
            tx.send(current_thread()).unwrap();
            wait(&[&*e2], false, 5000)
        });
        let thread = rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(suspend(&thread), Some(0));
        e.set();
        std::thread::sleep(Duration::from_millis(30));
        // Still set: the suspended waiter did not take it.
        assert_eq!(wait(&[&*e], false, 0), WAIT_OBJECT_0);
        e.set();
        assert_eq!(resume(&thread), 1);
        assert_eq!(t.join().unwrap(), WAIT_OBJECT_0);
        assert_eq!(wait(&[&*e], false, 0), WAIT_TIMEOUT);
    }
}
