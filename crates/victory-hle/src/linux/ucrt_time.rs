//! ucrtbase: time (`api-ms-win-crt-time`). The clock and the time zone are
//! the host's; the calendar text is the C locale's.
//!
//! The guest's `struct tm` is nine ints (glibc's has two more). The UCRT's
//! limits are kept: a time is valid from 0 (gmtime: -12 h) to 23:59:59 on
//! 31 December 3000 (gmtime: +14 h); outside them a conversion fails with
//! EINVAL without the invalid-parameter call.
//!
//! `strftime`, `_Strftime` and `_Wcsftime` are one formatter written here:
//! the UCRT validates each field a conversion reads (out of range: invalid
//! parameter), knows `#` (no leading zeros; long forms of `%c` and `%x`),
//! rejects an unknown conversion and reports a full buffer with 0 and
//! ERANGE.
//! ponytail: the names argument of `_Strftime`/`_Wcsftime` (what
//! `_Gettnames` gave) is not read: it can only hold the C locale's names.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_long};

use super::ucrt_heap::malloc;
use super::ucrt_runtime::{EINVAL, ERANGE, fail, invalid};
use super::ucrt_string::Unit;
use crate::{HleModule, hle_export};

/// The guest's `struct tm`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
}

/// `struct __timeb64`.
#[repr(C)]
pub struct Timeb64 {
    time: i64,
    millitm: u16,
    timezone: i16, // minutes west of UTC, without daylight time
    dstflag: i16,
}

const MAX_TIME: i64 = 0x7_9358_2AFF; // _MAX__TIME64_T
const MIN_LOCAL: i64 = -12 * 3600;
const MAX_LOCAL: i64 = 14 * 3600;

unsafe extern "C" {
    fn tzset();
    static timezone: c_long;
    static tzname: [*const c_char; 2];
}

fn host_zone() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: glibc's tzset, once.
    ONCE.call_once(|| unsafe { tzset() });
}

/// Seconds west of UTC without daylight time.
fn zone_west() -> i64 {
    host_zone();
    // SAFETY: set by tzset, read only.
    unsafe { timezone }
}

fn from_host(t: &libc::tm) -> Tm {
    Tm {
        sec: t.tm_sec,
        min: t.tm_min,
        hour: t.tm_hour,
        mday: t.tm_mday,
        mon: t.tm_mon,
        year: t.tm_year,
        wday: t.tm_wday,
        yday: t.tm_yday,
        isdst: t.tm_isdst,
    }
}

fn to_host(t: &Tm) -> libc::tm {
    // SAFETY: libc::tm is plain data; all-zero is valid.
    let mut host: libc::tm = unsafe { std::mem::zeroed() };
    host.tm_sec = t.sec;
    host.tm_min = t.min;
    host.tm_hour = t.hour;
    host.tm_mday = t.mday;
    host.tm_mon = t.mon;
    host.tm_year = t.year;
    host.tm_wday = t.wday;
    host.tm_yday = t.yday;
    host.tm_isdst = t.isdst;
    host
}

pub extern "win64" fn _time64(out: *mut i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    if !out.is_null() {
        // SAFETY: the title's __time64_t.
        unsafe { *out = now };
    }
    now
}

pub extern "win64" fn _ftime64(out: *mut Timeb64) {
    if out.is_null() {
        invalid(EINVAL);
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs() as i64;
    let mut local = Tm::default();
    _localtime64_s(&mut local, &seconds);
    // SAFETY: checked for null; the title's struct.
    unsafe {
        *out = Timeb64 {
            time: seconds,
            millitm: now.subsec_millis() as u16,
            timezone: (zone_west() / 60) as i16,
            dstflag: i16::from(local.isdst > 0),
        }
    };
}

pub extern "win64" fn _difftime64(later: i64, earlier: i64) -> f64 {
    if later < 0 || earlier < 0 {
        fail(EINVAL);
        return 0.0;
    }
    (later - earlier) as f64
}

/// Every field is -1 after a failure that had somewhere to write.
fn convert(out: *mut Tm, time: *const i64, local: bool) -> i32 {
    // SAFETY: the title's struct, or null.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return invalid(EINVAL);
    };
    *out = Tm {
        sec: -1,
        min: -1,
        hour: -1,
        mday: -1,
        mon: -1,
        year: -1,
        wday: -1,
        yday: -1,
        isdst: -1,
    };
    // SAFETY: the title's time, or null.
    let Some(&time) = (unsafe { time.as_ref() }) else {
        return invalid(EINVAL);
    };
    let (low, high) = if local { (0, MAX_TIME) } else { (MIN_LOCAL, MAX_TIME + MAX_LOCAL) };
    if !(low..=high).contains(&time) {
        return fail(EINVAL);
    }
    host_zone();
    // SAFETY: libc::tm is plain data.
    let mut host: libc::tm = unsafe { std::mem::zeroed() };
    let t = time as libc::time_t;
    // SAFETY: valid pointers to locals.
    unsafe {
        if local {
            libc::localtime_r(&t, &mut host);
        } else {
            libc::gmtime_r(&t, &mut host);
        }
    }
    *out = from_host(&host);
    0
}

pub extern "win64" fn _gmtime64_s(out: *mut Tm, time: *const i64) -> i32 {
    convert(out, time, false)
}

pub extern "win64" fn _localtime64_s(out: *mut Tm, time: *const i64) -> i32 {
    convert(out, time, true)
}

pub extern "win64" fn _gmtime64(time: *const i64) -> *mut Tm {
    thread_local! {
        static RESULT: std::cell::Cell<Tm> = const { std::cell::Cell::new(Tm {
            sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0,
        }) };
    }
    let result = RESULT.with(std::cell::Cell::as_ptr);
    if convert(result, time, false) == 0 { result } else { std::ptr::null_mut() }
}

/// The fields may be out of range and are normalised in place; a positive
/// `isdst` says daylight time, a negative one asks the zone. -1 and EINVAL outside the
/// years 1969..=3001 or the UCRT's range.
fn make_time(tm: *mut Tm, local: bool) -> i64 {
    // SAFETY: the title's struct, or null.
    let Some(tm) = (unsafe { tm.as_mut() }) else {
        invalid(EINVAL);
        return -1;
    };
    if !(69..=1101).contains(&tm.year) {
        fail(EINVAL);
        return -1;
    }
    host_zone();
    let mut host = to_host(tm);
    if !local {
        host.tm_isdst = 0;
    }
    // SAFETY: a valid tm of ours.
    let result = unsafe { if local { libc::mktime(&mut host) } else { libc::timegm(&mut host) } };
    let (low, high) = if local { (0, MAX_TIME) } else { (MIN_LOCAL, MAX_TIME + MAX_LOCAL) };
    if !(low..=high).contains(&result) {
        fail(EINVAL);
        return -1;
    }
    *tm = from_host(&host);
    result
}

pub extern "win64" fn _mktime64(tm: *mut Tm) -> i64 {
    make_time(tm, true)
}

pub extern "win64" fn _mkgmtime64(tm: *mut Tm) -> i64 {
    make_time(tm, false)
}

// ---- the C locale's names ---------------------------------------------------

/// In `__crt_lc_time_data` order: 7 short and 7 long days, 12 short and
/// 12 long months, AM/PM, then the short date, long date and time
/// pictures (GetDateFormat style).
const NAMES: [&str; 43] = [
    "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sunday", "Monday", "Tuesday", "Wednesday",
    "Thursday", "Friday", "Saturday", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep",
    "Oct", "Nov", "Dec", "January", "February", "March", "April", "May", "June", "July", "August",
    "September", "October", "November", "December", "AM", "PM", "MM/dd/yy", "dddd, MMMM dd, yyyy",
    "HH:mm:ss",
];
const DAY_ABBR: usize = 0;
const DAY: usize = 7;
const MONTH_ABBR: usize = 14;
const MONTH: usize = 26;
const AM_PM: usize = 38;

/// `__crt_lc_time_data`: what msvcp140 hands back to `_Strftime`. One
/// heap block with the strings behind the structure; freed with `free`.
#[repr(C)]
pub struct TimeData {
    narrow: [*mut u8; 43],
    ww_caltype: i32,
    refcount: i32,
    wide: [*mut u16; 43],
    locale_name: *mut u16,
}

pub extern "win64" fn _Gettnames() -> *mut TimeData {
    let characters: usize = NAMES.iter().map(|n| n.len() + 1).sum();
    let size = std::mem::size_of::<TimeData>() + characters * 3;
    let data = malloc(size).cast::<TimeData>();
    if data.is_null() {
        return data;
    }
    // SAFETY: a fresh block of `size` bytes: the structure, then `characters`
    // wide units, then `characters` bytes.
    unsafe {
        std::ptr::write_bytes(data.cast::<u8>(), 0, size);
        let mut wide = data.add(1).cast::<u16>();
        let mut narrow = wide.add(characters).cast::<u8>();
        (*data).ww_caltype = 1;
        for (i, name) in NAMES.iter().enumerate() {
            (*data).narrow[i] = narrow;
            (*data).wide[i] = wide;
            for &b in name.as_bytes().iter().chain(&[0]) {
                *narrow = b;
                *wide = b.into();
                narrow = narrow.add(1);
                wide = wide.add(1);
            }
        }
    }
    data
}

/// ":Sun:Sunday:Mon:Monday:..." (and months), the form msvcp140's
/// `time_get` parses; freed with `free`.
fn name_list<T: Unit>(abbreviated: usize, full: usize, count: usize) -> *mut T {
    let mut text: Vec<T> = vec![];
    for i in 0..count {
        for name in [NAMES[abbreviated + i], NAMES[full + i]] {
            text.push(T::from_u32(':'.into()));
            text.extend(name.bytes().map(|b| T::from_u32(b.into())));
        }
    }
    text.push(T::default());
    let list = malloc(text.len() * std::mem::size_of::<T>()).cast::<T>();
    if !list.is_null() {
        // SAFETY: a fresh block of text.len() units.
        unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), list, text.len()) };
    }
    list
}

pub extern "win64" fn _Getdays() -> *mut c_char {
    name_list::<u8>(DAY_ABBR, DAY, 7).cast()
}

pub extern "win64" fn _Getmonths() -> *mut c_char {
    name_list::<u8>(MONTH_ABBR, MONTH, 12).cast()
}

pub extern "win64" fn _W_Getdays() -> *mut u16 {
    name_list::<u16>(DAY_ABBR, DAY, 7)
}

pub extern "win64" fn _W_Getmonths() -> *mut u16 {
    name_list::<u16>(MONTH_ABBR, MONTH, 12)
}

// ---- strftime ---------------------------------------------------------------

struct Output<T> {
    text: Vec<T>,
    room: usize, // units left, the terminator's among them
    full: bool,
}

impl<T: Unit> Output<T> {
    fn put(&mut self, c: u8) {
        if self.room > 1 {
            self.text.push(T::from_u32(c.into()));
            self.room -= 1;
        } else {
            self.full = true;
        }
    }

    fn text(&mut self, s: &str) {
        s.bytes().for_each(|b| self.put(b));
    }

    /// `width` digits padded with `pad`; none with '#'.
    fn number(&mut self, value: i32, width: usize, bare: bool, pad: u8) {
        if value < 0 {
            self.put(b'-');
        }
        let digits = value.unsigned_abs().to_string();
        if !bare {
            for _ in digits.len()..width {
                self.put(pad);
            }
        }
        self.text(&digits);
    }
}

fn within(value: i32, low: i32, high: i32) -> bool {
    (low..=high).contains(&value)
}

/// The ISO 8601 week and the year it belongs to (%V, %G, %g).
fn iso_week(tm: &Tm) -> (i32, i32) {
    let weeks_in = |y: i32| {
        let p = |v: i32| (v + v / 4 - v / 100 + v / 400) % 7;
        52 + i32::from(p(y) == 4 || p(y - 1) == 3)
    };
    let mut year = tm.year + 1900;
    let mut week = (tm.yday - (tm.wday + 6) % 7 + 10) / 7;
    if week < 1 {
        year -= 1;
        week = weeks_in(year);
    } else if week > weeks_in(year) {
        week = 1;
        year += 1;
    }
    (week, year)
}

/// One conversion; false for an unknown one or a field out of range.
fn conversion<T: Unit>(out: &mut Output<T>, c: u8, tm: &Tm, bare: bool) -> bool {
    let year_ok = within(tm.year, -1900, 8099);
    let week_ok = within(tm.wday, 0, 6) && within(tm.yday, 0, 365) && year_ok;
    let n = |out: &mut Output<T>, ok: bool, value: i32, width: usize| {
        if ok {
            out.number(value, width, bare, b'0');
        }
        ok
    };
    let name = |out: &mut Output<T>, ok: bool, index: usize| {
        if ok {
            out.text(NAMES[index]);
        }
        ok
    };
    match c {
        b'a' => name(out, within(tm.wday, 0, 6), DAY_ABBR + tm.wday as usize),
        b'A' => name(out, within(tm.wday, 0, 6), DAY + tm.wday as usize),
        b'b' | b'h' => name(out, within(tm.mon, 0, 11), MONTH_ABBR + tm.mon as usize),
        b'B' => name(out, within(tm.mon, 0, 11), MONTH + tm.mon as usize),
        b'c' => expand(out, if bare { "%A, %B %d, %Y %H:%M:%S" } else { "%m/%d/%y %H:%M:%S" }, tm),
        b'C' => n(out, year_ok, (tm.year + 1900) / 100, 2),
        b'd' => n(out, within(tm.mday, 1, 31), tm.mday, 2),
        b'D' => expand(out, "%m/%d/%y", tm),
        b'e' => {
            let ok = within(tm.mday, 1, 31);
            if ok {
                out.number(tm.mday, 2, bare, b' ');
            }
            ok
        }
        b'F' => expand(out, "%Y-%m-%d", tm),
        b'g' | b'G' | b'V' => {
            if !week_ok {
                return false;
            }
            let (week, year) = iso_week(tm);
            match c {
                b'V' => out.number(week, 2, bare, b'0'),
                b'g' => out.number(year % 100, 2, bare, b'0'),
                _ => out.number(year, 4, bare, b'0'),
            }
            true
        }
        b'H' => n(out, within(tm.hour, 0, 23), tm.hour, 2),
        b'I' => n(out, within(tm.hour, 0, 23), if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 }, 2),
        b'j' => n(out, within(tm.yday, 0, 365), tm.yday + 1, 3),
        b'm' => n(out, within(tm.mon, 0, 11), tm.mon + 1, 2),
        b'M' => n(out, within(tm.min, 0, 59), tm.min, 2),
        b'n' => {
            out.put(b'\n');
            true
        }
        b'p' => name(out, within(tm.hour, 0, 23), AM_PM + usize::from(tm.hour >= 12)),
        b'r' => expand(out, "%I:%M:%S %p", tm),
        b'R' => expand(out, "%H:%M", tm),
        b'S' => n(out, within(tm.sec, 0, 60), tm.sec, 2),
        b't' => {
            out.put(b'\t');
            true
        }
        b'T' | b'X' => expand(out, "%H:%M:%S", tm),
        b'u' => n(out, within(tm.wday, 0, 6), if tm.wday == 0 { 7 } else { tm.wday }, 1),
        b'U' => n(out, week_ok, (tm.yday + 7 - tm.wday) / 7, 2),
        b'w' => n(out, within(tm.wday, 0, 6), tm.wday, 1),
        b'W' => n(out, week_ok, (tm.yday + 7 - (tm.wday + 6) % 7) / 7, 2),
        b'x' => expand(out, if bare { "%A, %B %d, %Y" } else { "%m/%d/%y" }, tm),
        b'y' => n(out, year_ok, (tm.year % 100 + 100) % 100, 2),
        b'Y' => n(out, year_ok, tm.year + 1900, 4),
        b'z' => {
            // +hhmm east of UTC; daylight time is taken as one hour
            let east = -zone_west() + if tm.isdst > 0 { 3600 } else { 0 };
            let minutes = east.abs() / 60;
            out.put(if east < 0 { b'-' } else { b'+' });
            out.number((minutes / 60) as i32, 2, false, b'0');
            out.number((minutes % 60) as i32, 2, false, b'0');
            true
        }
        b'Z' => {
            host_zone();
            // SAFETY: tzname holds two NUL-terminated names after tzset.
            let zone = unsafe { std::ffi::CStr::from_ptr(tzname[usize::from(tm.isdst > 0)]) };
            out.text(&zone.to_string_lossy());
            true
        }
        b'%' => {
            out.put(b'%');
            true
        }
        _ => false,
    }
}

/// A picture of this file's own (the composite conversions).
fn expand<T: Unit>(out: &mut Output<T>, format: &str, tm: &Tm) -> bool {
    let mut bytes = format.bytes();
    while let Some(b) = bytes.next() {
        if b != b'%' {
            out.put(b);
        } else if !conversion(out, bytes.next().unwrap_or(0), tm, false) {
            return false;
        }
    }
    true
}

fn format_time<T: Unit>(string: *mut T, size: usize, format: *const T, tm: *const Tm) -> usize {
    if string.is_null() || size == 0 {
        invalid(EINVAL);
        return 0;
    }
    // SAFETY: size > 0.
    unsafe { *string = T::default() };
    if format.is_null() {
        invalid(EINVAL);
        return 0;
    }
    // SAFETY: NUL-terminated, per the C contract.
    let format = unsafe { super::ucrt_string::units(format) };
    // SAFETY: the title's tm, or null.
    let tm = unsafe { tm.as_ref() };
    let mut out = Output { text: vec![], room: size, full: false };
    let mut i = 0;
    while i < format.len() {
        let c: u32 = format[i].into();
        i += 1;
        if c != u32::from(b'%') {
            if out.room > 1 {
                out.text.push(T::from_u32(c));
                out.room -= 1;
            } else {
                out.full = true;
            }
            continue;
        }
        let mut next = || {
            let c: u32 = format.get(i).copied().unwrap_or_default().into();
            i += 1;
            c
        };
        let mut c = next();
        let bare = c == u32::from(b'#');
        if bare {
            c = next();
        }
        if c == u32::from(b'E') || c == u32::from(b'O') {
            c = next();
        }
        let ok = match tm {
            Some(tm) if c < 128 => conversion(&mut out, c as u8, tm, bare),
            _ => false,
        };
        if !ok {
            invalid(EINVAL);
            return 0;
        }
    }
    if out.full {
        fail(ERANGE);
        return 0;
    }
    let n = out.text.len();
    // SAFETY: n < size.
    unsafe {
        std::ptr::copy_nonoverlapping(out.text.as_ptr(), string, n);
        *string.add(n) = T::default();
    }
    n
}

pub extern "win64" fn strftime(string: *mut c_char, size: usize, format: *const c_char, tm: *const Tm) -> usize {
    format_time(string.cast::<u8>(), size, format.cast::<u8>(), tm)
}

pub extern "win64" fn _Strftime(
    string: *mut c_char,
    size: usize,
    format: *const c_char,
    tm: *const Tm,
    _names: *mut TimeData,
) -> usize {
    strftime(string, size, format, tm)
}

pub extern "win64" fn _Wcsftime(
    string: *mut u16,
    size: usize,
    format: *const u16,
    tm: *const Tm,
    _names: *mut TimeData,
) -> usize {
    format_time(string, size, format, tm)
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-time",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-time-*"],
        exports: vec![
            hle_export!(_time64),
            hle_export!(_ftime64),
            hle_export!(_difftime64),
            hle_export!(_gmtime64_s),
            hle_export!(_localtime64_s),
            hle_export!(_gmtime64),
            hle_export!(_mktime64),
            hle_export!(_mkgmtime64),
            hle_export!(_Gettnames),
            crate::HleExport {
                name: "_W_Gettnames",
                address: _Gettnames as *const () as usize,
            },
            hle_export!(_Getdays),
            hle_export!(_Getmonths),
            hle_export!(_W_Getdays),
            hle_export!(_W_Getmonths),
            hle_export!(strftime),
            hle_export!(_Strftime),
            hle_export!(_Wcsftime),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_heap::free;
    use crate::linux::ucrt_runtime::errno;

    #[test]
    fn layouts_are_the_ucrts() {
        assert_eq!(std::mem::size_of::<Tm>(), 36);
        assert_eq!(std::mem::size_of::<Timeb64>(), 16);
        assert_eq!(std::mem::size_of::<TimeData>(), 704);
        assert_eq!(std::mem::offset_of!(TimeData, wide), 352);
    }

    #[test]
    fn utc_conversions_round_trip() {
        let mut tm = Tm::default();
        let t = 951_782_400; // 2000-02-29 00:00:00 UTC
        assert_eq!(_gmtime64_s(&mut tm, &t), 0);
        assert_eq!((tm.year, tm.mon, tm.mday, tm.wday, tm.yday), (100, 1, 29, 2, 59));
        assert_eq!(_mkgmtime64(&mut tm), t);
        tm.mday = 30; // normalised to 1 March
        assert_eq!(_mkgmtime64(&mut tm), t + 86400);
        assert_eq!((tm.mon, tm.mday), (2, 1));
        assert_eq!(_gmtime64_s(&mut tm, &(MAX_TIME + MAX_LOCAL + 1)), EINVAL);
        assert_eq!(tm.year, -1);
        let mut local = Tm::default();
        assert_eq!(_localtime64_s(&mut local, &t), 0);
        assert_eq!(_mktime64(&mut local), t);
        assert_eq!(_localtime64_s(&mut local, &-1), EINVAL);
    }

    #[test]
    fn strftime_formats_like_the_ucrt() {
        let tm = Tm { sec: 5, min: 4, hour: 15, mday: 9, mon: 0, year: 121, wday: 6, yday: 8, isdst: 0 };
        let mut buf = [0 as c_char; 64];
        let f = |buf: &mut [c_char], format: &std::ffi::CStr| {
            let n = strftime(buf.as_mut_ptr(), buf.len(), format.as_ptr(), &tm);
            // SAFETY: NUL-terminated by strftime.
            (n, unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap().to_owned())
        };
        assert_eq!(f(&mut buf, c"%Y-%m-%d %H:%M:%S").1, "2021-01-09 15:04:05");
        assert_eq!(f(&mut buf, c"%a %A %b %B %p %I %j %#d %e").1, "Sat Saturday Jan January PM 03 009 9  9");
        assert_eq!(f(&mut buf, c"%c|%#x").1, "01/09/21 15:04:05|Saturday, January 09, 2021");
        assert_eq!(f(&mut buf, c"%V %G %U %W %u %w").1, "01 2021 01 01 6 6");
        assert_eq!(f(&mut buf[..5], c"%Y-%m"), (0, String::new()));
        assert_eq!(errno(), ERANGE);
        assert_eq!(f(&mut buf, c"%Q"), (0, String::new()));
        assert_eq!(errno(), EINVAL);
        let mut wide = [0u16; 16];
        let format: Vec<u16> = "%H:%M".encode_utf16().chain([0]).collect();
        assert_eq!(_Wcsftime(wide.as_mut_ptr(), 16, format.as_ptr(), &tm, std::ptr::null_mut()), 5);
        assert_eq!(String::from_utf16_lossy(&wide[..5]), "15:04");
    }

    #[test]
    fn name_lists_and_time_data() {
        let days = _Getdays();
        // SAFETY: a NUL-terminated block of ours.
        let text = unsafe { std::ffi::CStr::from_ptr(days) }.to_str().unwrap().to_owned();
        assert!(text.starts_with(":Sun:Sunday:Mon:Monday") && text.ends_with(":Sat:Saturday"));
        free(days.cast());
        let data = _Gettnames();
        // SAFETY: a TimeData block of ours.
        unsafe {
            assert_eq!(std::ffi::CStr::from_ptr((*data).narrow[MONTH + 11].cast()), c"December");
            assert_eq!(*(*data).wide[AM_PM + 1], u16::from(b'P'));
        }
        free(data.cast());
    }
}
