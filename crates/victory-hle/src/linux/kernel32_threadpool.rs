//! kernel32: the thread pool (`TP_WORK`, `TP_TIMER`, `TP_WAIT`,
//! `QueueUserWorkItem` and the callback-instance calls).
//!
//! One pool. Its workers are guest threads, since everything they run is
//! title code; one is added whenever something is queued and none is idle,
//! up to `MAX_WORKERS`, so a callback that blocks holds nobody up. Timers
//! are kept by one host thread and each set wait by a host thread of its
//! own; those only queue callbacks and never run title code. Callback
//! environments (private pools, cleanup groups, priorities) are ignored:
//! everything uses the one pool.
// ponytail: one lock and a FIFO for the pool, a scan of the armed timers, a
// host thread per set wait; per-worker queues, a timer heap and one waiter
// thread if a profile shows the pool hot (the title's job system does not
// use it).

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use victory_loader::linux::thread as guest_thread;

use super::handles::{self, Event, Handle, INFINITE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use super::kernel32_time::file_time_now;
use crate::{HleModule, hle_export};

const MAX_WORKERS: u32 = 64;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Work,
    Timer,
    Wait,
    /// `QueueUserWorkItem`: `callback(context)`.
    Simple,
}

/// TP_WORK, TP_TIMER or TP_WAIT; title code holds `Arc::into_raw` of it.
/// The counters and the timer fields change under the pool lock.
struct Item {
    kind: Kind,
    callback: usize,
    context: usize,
    queued: AtomicU32,
    running: AtomicU32,
    closed: AtomicBool,
    /// Timer, while set: nanoseconds after `EPOCH` of its next callback.
    due: AtomicU64,
    period_ms: AtomicU32,
    /// Wait, while set: what ends its waiter thread.
    cancel: Mutex<Option<Arc<Event>>>,
}

/// TP_CALLBACK_INSTANCE: one running callback.
pub struct Instance {
    item: *const Item,
    /// False once `DisassociateCurrentThreadFromCallback` uncounted it.
    counted: AtomicBool,
}

#[derive(Default)]
struct Pool {
    queue: VecDeque<(Arc<Item>, u32)>,
    timers: Vec<Arc<Item>>,
    workers: u32,
    idle: u32,
    timer_thread: bool,
}

static POOL: LazyLock<Mutex<Pool>> = LazyLock::new(Default::default);
/// The workers: the queue is not empty.
static WORK: Condvar = Condvar::new();
/// `WaitFor...Callbacks`: some item has fewer callbacks outstanding.
static SETTLED: Condvar = Condvar::new();
static TIMERS: Condvar = Condvar::new();
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn pool() -> MutexGuard<'static, Pool> {
    POOL.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ns() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

fn new_item(kind: Kind, callback: usize, context: usize) -> usize {
    Arc::into_raw(Arc::new(Item {
        kind,
        callback,
        context,
        queued: AtomicU32::new(0),
        running: AtomicU32::new(0),
        closed: AtomicBool::new(false),
        due: AtomicU64::new(0),
        period_ms: AtomicU32::new(0),
        cancel: Mutex::new(None),
    })) as usize
}

/// The item behind a title pointer, with a reference of our own.
fn item(ptr: usize) -> Arc<Item> {
    // SAFETY: a pointer Create... returned and Close... has not released.
    unsafe {
        Arc::increment_strong_count(ptr as *const Item);
        Arc::from_raw(ptr as *const Item)
    }
}

/// Queues one callback of `item` (pool lock held), and starts a worker
/// if none is idle.
fn queue(pool: &mut Pool, item: Arc<Item>, result: u32) -> bool {
    item.queued.fetch_add(1, Relaxed);
    pool.queue.push_back((item, result));
    WORK.notify_one();
    let start = pool.idle == 0 && pool.workers < MAX_WORKERS;
    if start {
        pool.workers += 1;
    }
    start
}

fn start_worker() {
    if let Err(e) = guest_thread::spawn(worker, std::ptr::null_mut(), 0, Box::new(|_| {})) {
        tracing::warn!(%e, "thread pool: no new worker");
        pool().workers -= 1;
    }
}

fn submit(item: Arc<Item>, result: u32) {
    let start = queue(&mut pool(), item, result);
    if start {
        start_worker();
    }
}

type WorkCallback = unsafe extern "win64" fn(*mut Instance, usize, usize);
type WaitCallback = unsafe extern "win64" fn(*mut Instance, usize, usize, u32);
type SimpleCallback = unsafe extern "win64" fn(usize) -> u32;

unsafe extern "win64" fn worker(_: *mut c_void) -> u32 {
    loop {
        let (item, result) = {
            let mut p = pool();
            p.idle += 1;
            while p.queue.is_empty() {
                p = WORK.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            p.idle -= 1;
            let (item, result) = p.queue.pop_front().expect("not empty");
            item.queued.fetch_sub(1, Relaxed);
            item.running.fetch_add(1, Relaxed);
            (item, result)
        };
        let mut instance = Instance {
            item: Arc::as_ptr(&item),
            counted: AtomicBool::new(true),
        };
        let this = Arc::as_ptr(&item) as usize;
        // SAFETY: the title's callback, of the shape its kind has; no lock
        // is held.
        unsafe {
            match item.kind {
                Kind::Work | Kind::Timer => {
                    std::mem::transmute::<usize, WorkCallback>(item.callback)(&mut instance, item.context, this)
                }
                Kind::Wait => std::mem::transmute::<usize, WaitCallback>(item.callback)(
                    &mut instance,
                    item.context,
                    this,
                    result,
                ),
                Kind::Simple => {
                    std::mem::transmute::<usize, SimpleCallback>(item.callback)(item.context);
                }
            }
        }
        let _p = pool();
        if instance.counted.load(Relaxed) {
            item.running.fetch_sub(1, Relaxed);
        }
        SETTLED.notify_all();
    }
}

/// `WaitFor...Callbacks`: drops the queued callbacks with `cancel`, then
/// waits until none is queued or running.
fn wait_for_callbacks(ptr: usize, cancel: i32) {
    let item = item(ptr);
    let mut p = pool();
    if cancel != 0 {
        let before = p.queue.len();
        p.queue.retain(|(i, _)| !Arc::ptr_eq(i, &item));
        item.queued.fetch_sub((before - p.queue.len()) as u32, Relaxed);
    }
    while item.queued.load(Relaxed) > 0 || item.running.load(Relaxed) > 0 {
        p = SETTLED.wait(p).unwrap_or_else(|e| e.into_inner());
    }
}

/// `Close...`: no new callbacks; the queued and running ones finish, then
/// the item is freed with its last reference.
fn close(ptr: usize) {
    let item = item(ptr);
    {
        let mut p = pool();
        item.closed.store(true, Relaxed);
        item.due.store(0, Relaxed);
        p.timers.retain(|t| !Arc::ptr_eq(t, &item));
    }
    if let Some(cancel) = item.cancel.lock().unwrap_or_else(|e| e.into_inner()).take() {
        cancel.set();
    }
    // SAFETY: the title's reference, released once.
    drop(unsafe { Arc::from_raw(ptr as *const Item) });
}

// ---- work ----

pub extern "win64" fn CreateThreadpoolWork(callback: usize, context: usize, _env: usize) -> usize {
    new_item(Kind::Work, callback, context)
}

pub extern "win64" fn SubmitThreadpoolWork(work: usize) {
    submit(item(work), 0);
}

pub extern "win64" fn WaitForThreadpoolWorkCallbacks(work: usize, cancel: i32) {
    wait_for_callbacks(work, cancel);
}

pub extern "win64" fn CloseThreadpoolWork(work: usize) {
    close(work);
}

pub extern "win64" fn QueueUserWorkItem(function: usize, context: usize, _flags: u32) -> i32 {
    // SAFETY: `new_item`'s reference moves into the queue.
    submit(unsafe { Arc::from_raw(new_item(Kind::Simple, function, context) as *const Item) }, 0);
    1
}

// ---- timers ----

/// A FILETIME due time as nanoseconds from now: negative is relative (100
/// ns units), positive absolute UTC.
fn due_in(due: i64) -> u64 {
    if due < 0 {
        due.unsigned_abs().saturating_mul(100)
    } else {
        (due as u64).saturating_sub(file_time_now()).saturating_mul(100)
    }
}

pub extern "win64" fn CreateThreadpoolTimer(callback: usize, context: usize, _env: usize) -> usize {
    new_item(Kind::Timer, callback, context)
}

/// Arms (or with a null due time, disarms) the timer. A callback already
/// queued still runs.
pub extern "win64" fn SetThreadpoolTimer(timer: usize, due: *const i64, period_ms: u32, _window_ms: u32) {
    let item = item(timer);
    let mut p = pool();
    // Closed while its callback runs (one re-arming itself): stays closed.
    if item.closed.load(Relaxed) {
        return;
    }
    p.timers.retain(|t| !Arc::ptr_eq(t, &item));
    item.due.store(0, Relaxed);
    if due.is_null() {
        return;
    }
    // SAFETY: the title's FILETIME.
    let at = now_ns() + due_in(unsafe { due.read_unaligned() });
    item.due.store(at.max(1), Relaxed);
    item.period_ms.store(period_ms, Relaxed);
    p.timers.push(item);
    if !p.timer_thread {
        p.timer_thread = true;
        std::thread::Builder::new()
            .name("victory-tp-timers".into())
            .spawn(timer_thread)
            .expect("thread pool timer thread");
    }
    TIMERS.notify_one();
}

fn timer_thread() {
    let mut p = pool();
    loop {
        let now = now_ns();
        let mut start = 0;
        let mut i = 0;
        while i < p.timers.len() {
            let t = p.timers[i].clone();
            let due = t.due.load(Relaxed);
            if due > now {
                i += 1;
                continue;
            }
            match u64::from(t.period_ms.load(Relaxed)) * 1_000_000 {
                0 => {
                    t.due.store(0, Relaxed);
                    p.timers.swap_remove(i);
                }
                period => {
                    // Skips the periods missed, as Windows coalesces them.
                    t.due.store(due + period * ((now - due) / period + 1), Relaxed);
                    i += 1;
                }
            }
            start += u32::from(queue(&mut p, t, 0));
        }
        if start > 0 {
            drop(p);
            (0..start).for_each(|_| start_worker());
            p = pool();
            continue;
        }
        let next = p.timers.iter().map(|t| t.due.load(Relaxed)).min();
        p = match next {
            Some(at) => TIMERS
                .wait_timeout(p, Duration::from_nanos(at.saturating_sub(now)))
                .unwrap_or_else(|e| e.into_inner())
                .0,
            None => TIMERS.wait(p).unwrap_or_else(|e| e.into_inner()),
        };
    }
}

pub extern "win64" fn WaitForThreadpoolTimerCallbacks(timer: usize, cancel: i32) {
    wait_for_callbacks(timer, cancel);
}

pub extern "win64" fn CloseThreadpoolTimer(timer: usize) {
    close(timer);
}

// ---- waits ----

pub extern "win64" fn CreateThreadpoolWait(callback: usize, context: usize, _env: usize) -> usize {
    new_item(Kind::Wait, callback, context)
}

/// Waits once for `handle` (null: stops waiting) and queues the callback
/// with `WAIT_OBJECT_0` or, past `timeout` (null: never), `WAIT_TIMEOUT`.
pub extern "win64" fn SetThreadpoolWait(wait: usize, handle: Handle, timeout: *const i64) {
    let item = item(wait);
    let cancel = Event::new(true, false);
    let previous = std::mem::replace(
        &mut *item.cancel.lock().unwrap_or_else(|e| e.into_inner()),
        (handle != 0 && !item.closed.load(Relaxed)).then(|| cancel.clone()),
    );
    if let Some(previous) = previous {
        previous.set();
    }
    if handle == 0 || item.closed.load(Relaxed) {
        return;
    }
    let Some(object) = handles::object(handle).filter(|o| o.as_waitable().is_some()) else {
        tracing::warn!(handle, "SetThreadpoolWait on a handle that cannot be waited for");
        return;
    };
    let ms = if timeout.is_null() {
        INFINITE
    } else {
        // SAFETY: the title's FILETIME.
        (due_in(unsafe { timeout.read_unaligned() }) / 1_000_000).min(u64::from(INFINITE - 1)) as u32
    };
    std::thread::Builder::new()
        .name("victory-tp-wait".into())
        .spawn(move || {
            let waitable = object.as_waitable().expect("checked");
            let result = handles::wait(&[waitable, &*cancel], false, ms);
            let mut p = pool();
            let current = item.cancel.lock().unwrap_or_else(|e| e.into_inner()).take_if(|c| Arc::ptr_eq(c, &cancel));
            if current.is_some() && matches!(result, WAIT_OBJECT_0 | WAIT_TIMEOUT) && !item.closed.load(Relaxed) {
                let start = queue(&mut p, item, result);
                drop(p);
                if start {
                    start_worker();
                }
            }
        })
        .expect("thread pool wait thread");
}

pub extern "win64" fn WaitForThreadpoolWaitCallbacks(wait: usize, cancel: i32) {
    wait_for_callbacks(wait, cancel);
}

pub extern "win64" fn CloseThreadpoolWait(wait: usize) {
    close(wait);
}

// ---- callback instances ----

/// Workers are added on demand: another one is always available.
pub extern "win64" fn CallbackMayRunLong(_instance: *mut Instance) -> i32 {
    1
}

/// The callback stops counting as outstanding for `WaitFor...Callbacks`.
pub extern "win64" fn DisassociateCurrentThreadFromCallback(instance: *mut Instance) {
    // SAFETY: the instance the running callback was handed.
    let instance = unsafe { &*instance };
    let _p = pool();
    if instance.counted.swap(false, Relaxed) {
        // SAFETY: the worker holds a reference to the item while it runs.
        unsafe { &*instance.item }.running.fetch_sub(1, Relaxed);
        SETTLED.notify_all();
    }
}

/// Nothing to do: we never unload a module.
pub extern "win64" fn FreeLibraryWhenCallbackReturns(_instance: *mut Instance, _module: usize) {}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-threadpool",
        dlls: &["kernel32.dll", "kernelbase.dll", "api-ms-win-core-threadpool-*"],
        exports: vec![
            hle_export!(CreateThreadpoolWork),
            hle_export!(SubmitThreadpoolWork),
            hle_export!(WaitForThreadpoolWorkCallbacks),
            hle_export!(CloseThreadpoolWork),
            hle_export!(QueueUserWorkItem),
            hle_export!(CreateThreadpoolTimer),
            hle_export!(SetThreadpoolTimer),
            hle_export!(WaitForThreadpoolTimerCallbacks),
            hle_export!(CloseThreadpoolTimer),
            hle_export!(CreateThreadpoolWait),
            hle_export!(SetThreadpoolWait),
            hle_export!(WaitForThreadpoolWaitCallbacks),
            hle_export!(CloseThreadpoolWait),
            hle_export!(CallbackMayRunLong),
            hle_export!(DisassociateCurrentThreadFromCallback),
            hle_export!(FreeLibraryWhenCallbackReturns),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    unsafe extern "win64" fn bump(_: *mut Instance, counter: usize, _: usize) {
        std::thread::sleep(Duration::from_millis(2));
        // SAFETY: a leaked counter.
        unsafe { &*(counter as *const AtomicUsize) }.fetch_add(1, Relaxed);
    }

    fn counter() -> (&'static AtomicUsize, usize) {
        let c = Box::leak(Box::new(AtomicUsize::new(0)));
        (c, c as *const AtomicUsize as usize)
    }

    #[test]
    fn work_runs_every_submission_and_waits_for_them() {
        let (count, context) = counter();
        let work = CreateThreadpoolWork(bump as *const () as usize, context, 0);
        for _ in 0..20 {
            SubmitThreadpoolWork(work);
        }
        WaitForThreadpoolWorkCallbacks(work, 0);
        assert_eq!(count.load(Relaxed), 20);
        CloseThreadpoolWork(work);
    }

    #[test]
    fn timers_fire_once_or_periodically_and_stop() {
        let (count, context) = counter();
        let timer = CreateThreadpoolTimer(bump as *const () as usize, context, 0);
        let due = -20 * 10_000i64; // 20 ms from now
        SetThreadpoolTimer(timer, &due, 0, 0);
        std::thread::sleep(Duration::from_millis(80));
        WaitForThreadpoolTimerCallbacks(timer, 0);
        assert_eq!(count.load(Relaxed), 1);
        SetThreadpoolTimer(timer, &due, 10, 0);
        std::thread::sleep(Duration::from_millis(120));
        SetThreadpoolTimer(timer, std::ptr::null(), 0, 0);
        WaitForThreadpoolTimerCallbacks(timer, 1);
        let fired = count.load(Relaxed);
        assert!(fired >= 4, "{fired}");
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(count.load(Relaxed), fired);
        CloseThreadpoolTimer(timer);
    }

    unsafe extern "win64" fn record(_: *mut Instance, out: usize, _: usize, result: u32) {
        // SAFETY: a leaked counter.
        unsafe { &*(out as *const AtomicUsize) }.store(result as usize + 1, Relaxed);
    }

    #[test]
    fn waits_report_the_object_or_the_time() {
        let (seen, context) = counter();
        let wait = CreateThreadpoolWait(record as *const () as usize, context, 0);
        let event = Event::new(true, false);
        let h = handles::insert(event.clone());
        SetThreadpoolWait(wait, h, std::ptr::null());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(seen.load(Relaxed), 0);
        event.set();
        let start = Instant::now();
        while seen.load(Relaxed) == 0 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        WaitForThreadpoolWaitCallbacks(wait, 0);
        assert_eq!(seen.load(Relaxed), WAIT_OBJECT_0 as usize + 1);
        event.reset();
        let timeout = -10 * 10_000i64;
        SetThreadpoolWait(wait, h, &timeout);
        while seen.load(Relaxed) == 1 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(seen.load(Relaxed), WAIT_TIMEOUT as usize + 1);
        CloseThreadpoolWait(wait);
        handles::close(h);
    }

    unsafe extern "win64" fn simple(counter: usize) -> u32 {
        // SAFETY: a leaked counter.
        unsafe { &*(counter as *const AtomicUsize) }.fetch_add(1, Relaxed);
        0
    }

    #[test]
    fn queued_work_items_run() {
        let (count, context) = counter();
        assert_eq!(QueueUserWorkItem(simple as *const () as usize, context, 0), 1);
        let start = Instant::now();
        while count.load(Relaxed) == 0 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(count.load(Relaxed), 1);
    }
}
