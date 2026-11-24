//! ucrtbase: sorting, searching and random numbers
//! (`api-ms-win-crt-utility`). `qsort` and `bsearch` are the UCRT's own
//! algorithms, so equal elements land where they land there; `rand` is its
//! generator with per-thread state, so a seeded title gets its sequence.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::c_void;

use super::ucrt_runtime::{EINVAL, ENOMEM, fail, invalid};
use crate::{HleModule, hle_export};

type Compare = extern "win64" fn(*const c_void, *const c_void) -> i32;

/// Swaps two elements of `width` bytes (equal or disjoint).
///
/// # Safety
/// Both are elements of the array being sorted.
unsafe fn swap(a: usize, b: usize, width: usize) {
    if a != b {
        // SAFETY: two distinct elements of one array do not overlap.
        unsafe { std::ptr::swap_nonoverlapping(a as *mut u8, b as *mut u8, width) };
    }
}

/// The UCRT's `qsort`: median-of-three quicksort with an explicit stack and
/// a selection sort (largest to the end) at 8 elements or fewer.
pub extern "win64" fn qsort(base: *mut c_void, count: usize, width: usize, compare: Option<Compare>) {
    let Some(compare) = compare.filter(|_| (!base.is_null() || count == 0) && width > 0) else {
        invalid(EINVAL);
        return;
    };
    if count < 2 {
        return;
    }
    let cmp = |a: usize, b: usize| compare(a as *const c_void, b as *const c_void);
    // SAFETY (for every swap below): lo..=hi are elements of the array.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let (mut lo, mut hi) = (base as usize, base as usize + width * (count - 1));
    loop {
        let size = (hi - lo) / width + 1;
        if size <= 8 {
            let mut top = hi;
            while top > lo {
                let mut max = lo;
                let mut p = lo + width;
                while p <= top {
                    if cmp(p, max) > 0 {
                        max = p;
                    }
                    p += width;
                }
                // SAFETY: see above.
                unsafe { swap(max, top, width) };
                top -= width;
            }
        } else {
            let mut mid = lo + (size / 2) * width;
            // SAFETY: see above.
            unsafe {
                if cmp(lo, mid) > 0 {
                    swap(lo, mid, width);
                }
                if cmp(lo, hi) > 0 {
                    swap(lo, hi, width);
                }
                if cmp(mid, hi) > 0 {
                    swap(mid, hi, width);
                }
            }
            let (mut loguy, mut higuy) = (lo, hi);
            loop {
                if mid > loguy {
                    loop {
                        loguy += width;
                        if !(loguy < mid && cmp(loguy, mid) <= 0) {
                            break;
                        }
                    }
                }
                if mid <= loguy {
                    loop {
                        loguy += width;
                        if !(loguy <= hi && cmp(loguy, mid) <= 0) {
                            break;
                        }
                    }
                }
                loop {
                    higuy -= width;
                    if !(higuy > mid && cmp(higuy, mid) > 0) {
                        break;
                    }
                }
                if higuy < loguy {
                    break;
                }
                // SAFETY: see above.
                unsafe { swap(loguy, higuy, width) };
                if mid == higuy {
                    mid = loguy;
                }
            }
            higuy += width;
            if mid < higuy {
                loop {
                    higuy -= width;
                    if !(higuy > mid && cmp(higuy, mid) == 0) {
                        break;
                    }
                }
            }
            if mid >= higuy {
                loop {
                    higuy -= width;
                    if !(higuy > lo && cmp(higuy, mid) == 0) {
                        break;
                    }
                }
            }
            // The smaller part first; the larger waits on the stack.
            if higuy.wrapping_sub(lo) as isize >= hi.wrapping_sub(loguy) as isize {
                if lo < higuy {
                    stack.push((lo, higuy));
                }
                if loguy < hi {
                    lo = loguy;
                    continue;
                }
            } else {
                if loguy < hi {
                    stack.push((loguy, hi));
                }
                if lo < higuy {
                    hi = higuy;
                    continue;
                }
            }
        }
        match stack.pop() {
            Some((l, h)) => (lo, hi) = (l, h),
            None => return,
        }
    }
}

/// The UCRT's halving, so the element found among equal ones is its.
pub extern "win64" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    count: usize,
    width: usize,
    compare: Option<Compare>,
) -> *mut c_void {
    let Some(compare) = compare.filter(|_| (!base.is_null() || count == 0) && width > 0) else {
        invalid(EINVAL);
        return std::ptr::null_mut();
    };
    let mut count = count;
    let mut low = base as usize;
    let mut high = low + count.saturating_sub(1) * width;
    while count > 0 && low <= high {
        let half = count / 2;
        if half == 0 {
            return if compare(key, low as *const c_void) == 0 {
                low as *mut c_void
            } else {
                std::ptr::null_mut()
            };
        }
        let middle = low + (if count & 1 == 1 { half } else { half - 1 }) * width;
        let order = compare(key, middle as *const c_void);
        if order == 0 {
            return middle as *mut c_void;
        }
        if order < 0 {
            high = middle.wrapping_sub(width);
            count = if count & 1 == 1 { half } else { half - 1 };
        } else {
            low = middle + width;
            count = half;
        }
    }
    std::ptr::null_mut()
}

thread_local! {
    static RAND_STATE: Cell<u32> = const { Cell::new(1) };
}

pub extern "win64" fn srand(seed: u32) {
    RAND_STATE.set(seed);
}

pub extern "win64" fn rand() -> i32 {
    let state = RAND_STATE.get().wrapping_mul(214013).wrapping_add(2531011);
    RAND_STATE.set(state);
    ((state >> 16) & 0x7FFF) as i32
}

/// 32 bits from the system's generator.
pub extern "win64" fn rand_s(value: *mut u32) -> i32 {
    if value.is_null() {
        return invalid(EINVAL);
    }
    let mut bytes = [0u8; 4];
    // SAFETY: a 4-byte buffer of ours.
    let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), 4, 0) };
    // SAFETY: checked for null; the title's unsigned int.
    unsafe { *value = if got == 4 { u32::from_ne_bytes(bytes) } else { 0 } };
    if got == 4 { 0 } else { fail(ENOMEM) }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-utility",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-utility-*"],
        exports: vec![
            hle_export!(qsort),
            hle_export!(bsearch),
            hle_export!(srand),
            hle_export!(rand),
            hle_export!(rand_s),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "win64" fn by_value(a: *const c_void, b: *const c_void) -> i32 {
        // SAFETY: i32 elements.
        let (a, b) = unsafe { (*a.cast::<i32>(), *b.cast::<i32>()) };
        a.cmp(&b) as i32
    }

    #[test]
    fn qsort_sorts_and_bsearch_finds() {
        let mut seed = 12345u32;
        for n in [0usize, 1, 2, 7, 8, 9, 50, 1000] {
            let mut v: Vec<i32> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                    (seed >> 20) as i32 % 100
                })
                .collect();
            let mut expected = v.clone();
            expected.sort();
            qsort(v.as_mut_ptr().cast(), n, 4, Some(by_value));
            assert_eq!(v, expected, "n = {n}");
            for key in [0, 42, 99, 100] {
                let found = bsearch((&key as *const i32).cast(), v.as_ptr().cast(), n, 4, Some(by_value));
                assert_eq!(!found.is_null(), v.contains(&key));
                if !found.is_null() {
                    // SAFETY: an element of v.
                    assert_eq!(unsafe { *found.cast::<i32>() }, key);
                }
            }
        }
        qsort(std::ptr::null_mut(), 3, 4, Some(by_value)); // invalid: nothing happens
    }

    #[test]
    fn rand_is_the_ucrts_sequence() {
        srand(1);
        assert_eq!([rand(), rand(), rand()], [41, 18467, 6334]);
        let mut v = 0;
        assert_eq!(rand_s(&mut v), 0);
        assert_eq!(rand_s(std::ptr::null_mut()), EINVAL);
    }
}
