//! kernel32: clocks and calendar times. The performance counter runs at
//! 10 MHz (100 ns ticks) as on current Windows; the tick count includes
//! time asleep (`CLOCK_BOOTTIME`), as Windows' does.

#![allow(non_snake_case)]

use victory_loader::linux::teb::set_last_error;

use super::kernel32_wait::ERROR_INVALID_PARAMETER;
use crate::{HleModule, hle_export};

/// 100 ns intervals from 1601 to 1970.
const UNIX_EPOCH: u64 = 116_444_736_000_000_000;

fn clock(id: libc::clockid_t) -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `t` is a valid out pointer.
    unsafe { libc::clock_gettime(id, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

pub extern "win64" fn QueryPerformanceCounter(count: *mut i64) -> i32 {
    // SAFETY: the title's out pointer.
    unsafe { count.write_unaligned((clock(libc::CLOCK_MONOTONIC) / 100) as i64) };
    1
}

pub extern "win64" fn QueryPerformanceFrequency(frequency: *mut i64) -> i32 {
    // SAFETY: the title's out pointer.
    unsafe { frequency.write_unaligned(10_000_000) };
    1
}

pub extern "win64" fn GetTickCount64() -> u64 {
    clock(libc::CLOCK_BOOTTIME) / 1_000_000
}

pub extern "win64" fn GetTickCount() -> u32 {
    GetTickCount64() as u32
}

/// Now as a `FILETIME`: 100 ns intervals since 1601, UTC.
pub(crate) fn file_time_now() -> u64 {
    clock(libc::CLOCK_REALTIME) / 100 + UNIX_EPOCH
}

pub extern "win64" fn GetSystemTimeAsFileTime(time: *mut u64) {
    // SAFETY: the title's FILETIME (two DWORDs, 4-aligned).
    unsafe { time.write_unaligned(file_time_now()) };
}

pub extern "win64" fn GetSystemTimePreciseAsFileTime(time: *mut u64) {
    GetSystemTimeAsFileTime(time)
}

/// 15.625 ms, the Windows default; adjustment disabled.
pub extern "win64" fn GetSystemTimeAdjustment(adjustment: *mut u32, increment: *mut u32, disabled: *mut i32) -> i32 {
    // SAFETY: the title's out pointers.
    unsafe {
        adjustment.write_unaligned(156_250);
        increment.write_unaligned(156_250);
        disabled.write_unaligned(1);
    }
    1
}

/// `SYSTEMTIME`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct SystemTime {
    pub(crate) year: u16,
    pub(crate) month: u16,
    pub(crate) day_of_week: u16,
    pub(crate) day: u16,
    pub(crate) hour: u16,
    pub(crate) minute: u16,
    pub(crate) second: u16,
    pub(crate) milliseconds: u16,
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn system_time(file_time: u64) -> SystemTime {
    let ms = (file_time / 10_000) as i64 - (UNIX_EPOCH / 10_000) as i64;
    let (days, ms_of_day) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    SystemTime {
        year: y as u16,
        month: m as u16,
        day_of_week: (days + 4).rem_euclid(7) as u16, // 1970-01-01 was a Thursday
        day: d as u16,
        hour: (ms_of_day / 3_600_000) as u16,
        minute: (ms_of_day / 60_000 % 60) as u16,
        second: (ms_of_day / 1000 % 60) as u16,
        milliseconds: (ms_of_day % 1000) as u16,
    }
}

pub extern "win64" fn GetSystemTime(time: *mut SystemTime) {
    // SAFETY: the title's SYSTEMTIME.
    unsafe { time.write_unaligned(system_time(file_time_now())) };
}

pub extern "win64" fn GetLocalTime(time: *mut SystemTime) {
    let now = file_time_now();
    let seconds = ((now - UNIX_EPOCH) / 10_000_000) as libc::time_t;
    // SAFETY: localtime_r writes `tm`; a zeroed tm is valid.
    let offset = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&seconds, &mut tm);
        tm.tm_gmtoff
    };
    let local = (now as i64 + offset * 10_000_000) as u64;
    // SAFETY: the title's SYSTEMTIME.
    unsafe { time.write_unaligned(system_time(local)) };
}

pub extern "win64" fn FileTimeToSystemTime(file_time: *const u64, time: *mut SystemTime) -> i32 {
    // SAFETY: the title's FILETIME.
    let ft = unsafe { file_time.read_unaligned() };
    if ft >= 1 << 63 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: the title's SYSTEMTIME.
    unsafe { time.write_unaligned(system_time(ft)) };
    1
}

pub extern "win64" fn SystemTimeToFileTime(time: *const SystemTime, file_time: *mut u64) -> i32 {
    // SAFETY: the title's SYSTEMTIME.
    let t = unsafe { time.read_unaligned() };
    let leap = |y: u16| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let days_in_month = match t.month {
        2 if leap(t.year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1601..=30827).contains(&t.year)
        || !(1..=12).contains(&t.month)
        || !(1..=days_in_month).contains(&t.day)
        || t.hour > 23
        || t.minute > 59
        || t.second > 59
        || t.milliseconds > 999
    {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let days = days_from_civil(t.year.into(), t.month.into(), t.day.into());
    let ms = days * 86_400_000
        + i64::from(t.hour) * 3_600_000
        + i64::from(t.minute) * 60_000
        + i64::from(t.second) * 1000
        + i64::from(t.milliseconds);
    // SAFETY: the title's FILETIME.
    unsafe { file_time.write_unaligned((ms * 10_000 + UNIX_EPOCH as i64) as u64) };
    1
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-time",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-profile-*",
            "api-ms-win-core-sysinfo-*",
            "api-ms-win-core-timezone-*",
        ],
        exports: vec![
            hle_export!(QueryPerformanceCounter),
            hle_export!(QueryPerformanceFrequency),
            hle_export!(GetTickCount64),
            hle_export!(GetTickCount),
            hle_export!(GetSystemTimeAsFileTime),
            hle_export!(GetSystemTimePreciseAsFileTime),
            hle_export!(GetSystemTimeAdjustment),
            hle_export!(GetSystemTime),
            hle_export!(GetLocalTime),
            hle_export!(FileTimeToSystemTime),
            hle_export!(SystemTimeToFileTime),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_layouts_match_windows() {
        assert_eq!(size_of::<SystemTime>(), 16);
    }

    #[test]
    fn calendar_round_trips() {
        let t = SystemTime {
            year: 2026,
            month: 11,
            day_of_week: 2,
            day: 24,
            hour: 13,
            minute: 5,
            second: 9,
            milliseconds: 250,
        };
        let mut ft = 0;
        assert_eq!(SystemTimeToFileTime(&t, &mut ft), 1);
        let mut back = SystemTime::default();
        assert_eq!(FileTimeToSystemTime(&ft, &mut back), 1);
        assert_eq!(back, t);
        // 1601-01-01 is FILETIME 0, a Monday.
        assert_eq!(FileTimeToSystemTime(&0, &mut back), 1);
        assert_eq!((back.year, back.month, back.day, back.day_of_week), (1601, 1, 1, 1));
        let feb30 = SystemTime { month: 2, day: 30, ..t };
        assert_eq!(SystemTimeToFileTime(&feb30, &mut ft), 0);
        let leap = SystemTime { year: 2024, month: 2, day: 29, ..t };
        assert_eq!(SystemTimeToFileTime(&leap, &mut ft), 1);
    }

    #[test]
    fn clocks_move_forward_at_their_rates() {
        let (mut a, mut b, mut f) = (0, 0, 0);
        QueryPerformanceFrequency(&mut f);
        QueryPerformanceCounter(&mut a);
        std::thread::sleep(std::time::Duration::from_millis(20));
        QueryPerformanceCounter(&mut b);
        assert!((b - a) * 1000 / f >= 20);
        let mut ft = 0;
        GetSystemTimeAsFileTime(&mut ft);
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        assert!((ft / 10_000_000).abs_diff(unix.as_secs() + UNIX_EPOCH / 10_000_000) <= 1);
        let mut now = SystemTime::default();
        GetSystemTime(&mut now);
        assert!(now.year >= 2026);
        assert!(GetTickCount64() > 0);
    }
}
