use std::collections::BTreeMap;
use std::sync::Mutex;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
pub(crate) const KERNEL_PAGE: usize = 4096;

pub(crate) struct Journal {
    /// Numbers the askings; 0 is "never".
    pub(crate) asked: u64,
    /// Page number → the asking that last found it written.
    pub(crate) written: BTreeMap<usize, u64>,
    /// Room for the kernel's answer.
    pub(crate) found: Vec<usize>,
}

pub(crate) static JOURNAL: Mutex<Journal> = Mutex::new(Journal {
    asked: 0,
    written: BTreeMap::new(),
    found: Vec::new(),
});

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let mut j = JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    let pages = written_since(&mut j.found, first, last, guarded)?;
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        #[cfg(not(windows))]
        add(C::GuardFaults, victory_loader::linux::watch::take_faults());
    }
    j.asked += 1;
    let now = j.asked;
    for page in pages {
        j.written.insert(page, now);
    }
    Some((
        now,
        j.written
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect(),
    ))
}

/// Pages `[first, last)` written since the last asking (by anyone), as
/// page numbers; `found` is room for the kernel's answer.
#[cfg(not(windows))]
pub(crate) fn written_since(
    _: &mut Vec<usize>,
    first: usize,
    last: usize,
    guarded: bool,
) -> Option<Vec<usize>> {
    const _: () = assert!(PAGE == victory_loader::linux::watch::BLOCK);
    victory_loader::linux::vm::guard(first * PAGE, last * PAGE, guarded)
}

#[cfg(windows)]
pub(crate) fn written_since(
    found: &mut Vec<usize>,
    first: usize,
    last: usize,
    guarded: bool,
) -> Option<Vec<usize>> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    found.resize(count.max(found.len()), 0);
    let mut granularity = 0;
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return if guarded {
            guard(first * PAGE, last * PAGE)
        } else {
            None
        };
    }
    let mut pages: Vec<usize> = found[..count].iter().map(|&at| at / PAGE).collect();
    pages.dedup();
    Some(pages)
}
