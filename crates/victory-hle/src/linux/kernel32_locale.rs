//! kernel32 locales: the user's locale, names and LCIDs, and locale data.
//!
//! The user (and system) locale is en-US unless `LC_ALL` or `LANG` names
//! one of [`LOCALES`] (`fr_FR.UTF-8` is `fr-FR`); a value that does not
//! map cleanly (`C`, `POSIX`, an unknown pair) is en-US. It is what
//! `GetUserDefaultLocaleName`, the default LCID and LANGID and
//! `GetUserDefaultGeoName` report, which is how a title picks its
//! language.
//!
//! Locale data is en-US for every locale except what names it (its name,
//! LCID, language and region): separators, formats, day and month names.
// ponytail: en-US data for every locale; per-locale tables if a title formats numbers or dates with them.

#![allow(non_snake_case)] // named like the exports

use std::sync::OnceLock;

use victory_loader::linux::teb::set_last_error;

use super::kernel32_file::{ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, fail, wide};
use super::kernel32_text::ERROR_INVALID_FLAGS;
use crate::HleModule;
use crate::hle_export;

/// A locale we know: name, LCID, English language and region names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Locale {
    pub name: &'static str,
    pub lcid: u32,
    language: &'static str,
    region: &'static str,
}

const fn l(name: &'static str, lcid: u32, language: &'static str, region: &'static str) -> Locale {
    Locale {
        name,
        lcid,
        language,
        region,
    }
}

/// The locales a title is likely to ship texts for; the first of a
/// language is what its bare name resolves to.
pub(crate) const LOCALES: &[Locale] = &[
    l("en-US", 0x0409, "English", "United States"),
    l("en-GB", 0x0809, "English", "United Kingdom"),
    l("en-CA", 0x1009, "English", "Canada"),
    l("en-AU", 0x0C09, "English", "Australia"),
    l("fr-FR", 0x040C, "French", "France"),
    l("fr-CA", 0x0C0C, "French", "Canada"),
    l("de-DE", 0x0407, "German", "Germany"),
    l("es-ES", 0x0C0A, "Spanish", "Spain"),
    l("es-MX", 0x080A, "Spanish", "Mexico"),
    l("it-IT", 0x0410, "Italian", "Italy"),
    l("pt-BR", 0x0416, "Portuguese", "Brazil"),
    l("pt-PT", 0x0816, "Portuguese", "Portugal"),
    l("ru-RU", 0x0419, "Russian", "Russia"),
    l("pl-PL", 0x0415, "Polish", "Poland"),
    l("ja-JP", 0x0411, "Japanese", "Japan"),
    l("ko-KR", 0x0412, "Korean", "Korea"),
    l("zh-CN", 0x0804, "Chinese", "China"),
    l("zh-TW", 0x0404, "Chinese", "Taiwan"),
    l("zh-HK", 0x0C04, "Chinese", "Hong Kong SAR"),
    l("nl-NL", 0x0413, "Dutch", "Netherlands"),
    l("sv-SE", 0x041D, "Swedish", "Sweden"),
    l("da-DK", 0x0406, "Danish", "Denmark"),
    l("fi-FI", 0x040B, "Finnish", "Finland"),
    l("nb-NO", 0x0414, "Norwegian Bokmål", "Norway"),
    l("cs-CZ", 0x0405, "Czech", "Czechia"),
    l("hu-HU", 0x040E, "Hungarian", "Hungary"),
    l("el-GR", 0x0408, "Greek", "Greece"),
    l("tr-TR", 0x041F, "Turkish", "Türkiye"),
    l("ar-SA", 0x0401, "Arabic", "Saudi Arabia"),
    l("he-IL", 0x040D, "Hebrew", "Israel"),
    l("th-TH", 0x041E, "Thai", "Thailand"),
    l("uk-UA", 0x0422, "Ukrainian", "Ukraine"),
    l("id-ID", 0x0421, "Indonesian", "Indonesia"),
    l("vi-VN", 0x042A, "Vietnamese", "Vietnam"),
];

const INVARIANT: Locale = l("", 0x007F, "Invariant Language", "Invariant Country");
const EN_US: Locale = LOCALES[0];

fn by_name(name: &str) -> Option<Locale> {
    if name.is_empty() {
        return Some(INVARIANT);
    }
    LOCALES
        .iter()
        .copied()
        .find(|l| l.name.eq_ignore_ascii_case(name))
}

fn by_lcid(lcid: u32) -> Option<Locale> {
    match lcid {
        0 | LOCALE_USER_DEFAULT | LOCALE_SYSTEM_DEFAULT => Some(user()),
        0x007F => Some(INVARIANT),
        _ => LOCALES.iter().copied().find(|l| l.lcid == lcid),
    }
}

const LOCALE_USER_DEFAULT: u32 = 0x0400;
const LOCALE_SYSTEM_DEFAULT: u32 = 0x0800;

/// A POSIX locale value as one of ours: `fr_FR.UTF-8@euro` is `fr-FR`.
fn from_posix(value: &str) -> Option<Locale> {
    let tag = value.split(['.', '@']).next()?;
    let (language, region) = tag.split_once('_')?;
    by_name(&format!("{language}-{region}")).filter(|l| !l.name.is_empty())
}

/// The user's locale, read once.
pub(crate) fn user() -> Locale {
    static USER: OnceLock<Locale> = OnceLock::new();
    *USER.get_or_init(|| {
        ["LC_ALL", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .and_then(|v| from_posix(&v))
            .unwrap_or(EN_US)
    })
}

/// The locale a name argument stands for: null and
/// `!x-sys-default-locale` are the user's, "" is the invariant locale.
fn named(name: *const u16) -> Option<Locale> {
    match wide(name) {
        None => Some(user()),
        Some(n) => {
            let n = String::from_utf16_lossy(n);
            if n.eq_ignore_ascii_case("!x-sys-default-locale") {
                Some(user())
            } else {
                by_name(&n)
            }
        }
    }
}

/// The buffer rule here: the count includes the NUL, capacity 0 asks for
/// it, a short buffer is `ERROR_INSUFFICIENT_BUFFER`.
fn deliver(text: &str, out: *mut u16, capacity: i32) -> i32 {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    if capacity == 0 {
        return units.len() as i32;
    }
    if out.is_null() || (capacity as usize) < units.len() {
        return fail(ERROR_INSUFFICIENT_BUFFER);
    }
    // SAFETY: the caller's buffer of `capacity` units, enough.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), out, units.len()) };
    units.len() as i32
}

pub extern "win64" fn GetUserDefaultLocaleName(out: *mut u16, capacity: i32) -> i32 {
    deliver(user().name, out, capacity)
}

pub extern "win64" fn GetSystemDefaultLocaleName(out: *mut u16, capacity: i32) -> i32 {
    deliver(user().name, out, capacity)
}

pub extern "win64" fn GetUserDefaultLCID() -> u32 {
    user().lcid
}

pub extern "win64" fn GetSystemDefaultLCID() -> u32 {
    user().lcid
}

pub extern "win64" fn GetThreadLocale() -> u32 {
    user().lcid
}

pub extern "win64" fn GetUserDefaultLangID() -> u16 {
    user().lcid as u16
}

pub extern "win64" fn GetSystemDefaultLangID() -> u16 {
    user().lcid as u16
}

pub extern "win64" fn GetUserDefaultUILanguage() -> u16 {
    user().lcid as u16
}

pub extern "win64" fn GetSystemDefaultUILanguage() -> u16 {
    user().lcid as u16
}

/// The region subtag of the user locale.
pub extern "win64" fn GetUserDefaultGeoName(out: *mut u16, capacity: i32) -> i32 {
    deliver(user().name.split('-').nth(1).unwrap_or("US"), out, capacity)
}

pub extern "win64" fn LocaleNameToLCID(name: *const u16, _flags: u32) -> u32 {
    match named(name) {
        Some(l) => l.lcid,
        None => fail(ERROR_INVALID_PARAMETER),
    }
}

pub extern "win64" fn LCIDToLocaleName(
    lcid: u32,
    out: *mut u16,
    capacity: i32,
    _flags: u32,
) -> i32 {
    match by_lcid(lcid) {
        Some(l) => deliver(l.name, out, capacity),
        None => fail(ERROR_INVALID_PARAMETER),
    }
}

pub extern "win64" fn IsValidLocaleName(name: *const u16) -> i32 {
    i32::from(wide(name).is_some() && named(name).is_some())
}

pub extern "win64" fn IsValidLocale(lcid: u32, _flags: u32) -> i32 {
    i32::from(by_lcid(lcid).is_some())
}

/// A known name as itself; a bare language (`fr`) as its first locale.
pub extern "win64" fn ResolveLocaleName(name: *const u16, out: *mut u16, capacity: i32) -> i32 {
    let found = match named(name) {
        Some(l) => Some(l),
        None => wide(name).and_then(|n| {
            let n = String::from_utf16_lossy(n);
            let language = n.split('-').next().unwrap_or_default();
            LOCALES.iter().copied().find(|l| {
                l.name
                    .split('-')
                    .next()
                    .is_some_and(|x| x.eq_ignore_ascii_case(language))
            })
        }),
    };
    match found {
        Some(l) => deliver(l.name, out, capacity),
        None => 0,
    }
}

// ---- locale data ----------------------------------------------------------------------------

const LOCALE_NOUSEROVERRIDE: u32 = 0x8000_0000;
const LOCALE_USE_CP_ACP: u32 = 0x4000_0000;
const LOCALE_RETURN_NUMBER: u32 = 0x2000_0000;

const LOCALE_ILANGUAGE: u32 = 0x1;
const LOCALE_SLOCALIZEDDISPLAYNAME: u32 = 0x2;
const LOCALE_SNATIVELANGNAME: u32 = 0x4;
const LOCALE_ICOUNTRY: u32 = 0x5;
const LOCALE_SLOCALIZEDCOUNTRYNAME: u32 = 0x6;
const LOCALE_SNATIVECOUNTRYNAME: u32 = 0x8;
const LOCALE_IDEFAULTLANGUAGE: u32 = 0x9;
const LOCALE_IDEFAULTCOUNTRY: u32 = 0xA;
const LOCALE_IDEFAULTCODEPAGE: u32 = 0xB;
const LOCALE_SDAYNAME1: u32 = 0x2A;
const LOCALE_SABBREVDAYNAME1: u32 = 0x31;
const LOCALE_SMONTHNAME1: u32 = 0x38;
const LOCALE_SABBREVMONTHNAME1: u32 = 0x44;
const LOCALE_SISO639LANGNAME: u32 = 0x59;
const LOCALE_SISO3166CTRYNAME: u32 = 0x5A;
const LOCALE_SNAME: u32 = 0x5C;
const LOCALE_SPARENT: u32 = 0x6D;
const LOCALE_SENGLISHDISPLAYNAME: u32 = 0x72;
const LOCALE_SNATIVEDISPLAYNAME: u32 = 0x73;
const LOCALE_SENGLISHLANGUAGENAME: u32 = 0x1001;
const LOCALE_SENGLISHCOUNTRYNAME: u32 = 0x1002;
const LOCALE_IDEFAULTANSICODEPAGE: u32 = 0x1004;

const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The value of an LCTYPE as text, as Windows stores even numbers.
fn info(l: Locale, kind: u32) -> Option<String> {
    let (language, region) = l.name.split_once('-').unwrap_or(("", ""));
    let display = || {
        if l.name.is_empty() {
            l.language.to_string()
        } else {
            format!("{} ({})", l.language, l.region)
        }
    };
    let fixed = |s: &str| Some(s.to_string());
    match kind {
        LOCALE_SNAME => fixed(l.name),
        LOCALE_ILANGUAGE | LOCALE_IDEFAULTLANGUAGE => Some(format!("{:04x}", l.lcid & 0xFFFF)),
        LOCALE_SLOCALIZEDDISPLAYNAME | LOCALE_SENGLISHDISPLAYNAME | LOCALE_SNATIVEDISPLAYNAME => {
            Some(display())
        }
        LOCALE_SNATIVELANGNAME | LOCALE_SENGLISHLANGUAGENAME => fixed(l.language),
        LOCALE_SLOCALIZEDCOUNTRYNAME | LOCALE_SNATIVECOUNTRYNAME | LOCALE_SENGLISHCOUNTRYNAME => {
            fixed(l.region)
        }
        LOCALE_SISO639LANGNAME => fixed(if l.name.is_empty() { "iv" } else { language }),
        LOCALE_SISO3166CTRYNAME => fixed(if l.name.is_empty() { "IV" } else { region }),
        LOCALE_SPARENT => fixed(language),
        LOCALE_ICOUNTRY | LOCALE_IDEFAULTCOUNTRY => fixed("1"),
        LOCALE_IDEFAULTCODEPAGE => fixed("437"),
        LOCALE_IDEFAULTANSICODEPAGE => fixed("1252"),
        0x1011 => fixed("10000"),            // LOCALE_IDEFAULTMACCODEPAGE
        0x0C => fixed(","),                  // LOCALE_SLIST
        0x0D => fixed("1"),                  // LOCALE_IMEASURE: U.S.
        0x0E => fixed("."),                  // LOCALE_SDECIMAL
        0x0F => fixed(","),                  // LOCALE_STHOUSAND
        0x10 => fixed("3;0"),                // LOCALE_SGROUPING
        0x11 => fixed("2"),                  // LOCALE_IDIGITS
        0x12 => fixed("1"),                  // LOCALE_ILZERO
        0x13 => fixed("0123456789"),         // LOCALE_SNATIVEDIGITS
        0x14 => fixed("$"),                  // LOCALE_SCURRENCY
        0x15 => fixed("USD"),                // LOCALE_SINTLSYMBOL
        0x16 => fixed("."),                  // LOCALE_SMONDECIMALSEP
        0x17 => fixed(","),                  // LOCALE_SMONTHOUSANDSEP
        0x18 => fixed("3;0"),                // LOCALE_SMONGROUPING
        0x19 => fixed("2"),                  // LOCALE_ICURRDIGITS
        0x1B => fixed("0"),                  // LOCALE_ICURRENCY
        0x1C => fixed("0"),                  // LOCALE_INEGCURR
        0x1D => fixed("/"),                  // LOCALE_SDATE
        0x1E => fixed(":"),                  // LOCALE_STIME
        0x1F => fixed("M/d/yyyy"),           // LOCALE_SSHORTDATE
        0x20 => fixed("dddd, MMMM d, yyyy"), // LOCALE_SLONGDATE
        0x21 | 0x22 => fixed("0"),           // LOCALE_IDATE, LOCALE_ILDATE: month-day-year
        0x23 => fixed("0"),                  // LOCALE_ITIME: 12 hours
        0x25 => fixed("0"),                  // LOCALE_ITLZERO
        0x28 => fixed("AM"),                 // LOCALE_S1159
        0x29 => fixed("PM"),                 // LOCALE_S2359
        0x50 => fixed(""),                   // LOCALE_SPOSITIVESIGN
        0x51 => fixed("-"),                  // LOCALE_SNEGATIVESIGN
        0x5B => fixed("244"),                // LOCALE_IGEOID
        0x70 => fixed("0"),                  // LOCALE_IREADINGLAYOUT: left to right
        0x79 => fixed("h:mm tt"),            // LOCALE_SSHORTTIME
        0x100A => fixed("1"),                // LOCALE_IPAPERSIZE: letter
        0x100C => fixed("6"),                // LOCALE_IFIRSTDAYOFWEEK: Sunday
        0x100D => fixed("0"),                // LOCALE_IFIRSTWEEKOFYEAR
        0x1003 => fixed("h:mm:ss tt"),       // LOCALE_STIMEFORMAT
        0x1010 => fixed("1"),                // LOCALE_INEGNUMBER
        k if (LOCALE_SDAYNAME1..LOCALE_SDAYNAME1 + 7).contains(&k) => {
            fixed(DAYS[(k - LOCALE_SDAYNAME1) as usize])
        }
        k if (LOCALE_SABBREVDAYNAME1..LOCALE_SABBREVDAYNAME1 + 7).contains(&k) => {
            fixed(&DAYS[(k - LOCALE_SABBREVDAYNAME1) as usize][..3])
        }
        k if (LOCALE_SMONTHNAME1..LOCALE_SMONTHNAME1 + 12).contains(&k) => {
            fixed(MONTHS[(k - LOCALE_SMONTHNAME1) as usize])
        }
        k if (LOCALE_SABBREVMONTHNAME1..LOCALE_SABBREVMONTHNAME1 + 12).contains(&k) => {
            fixed(&MONTHS[(k - LOCALE_SABBREVMONTHNAME1) as usize][..3])
        }
        _ => None,
    }
}

fn locale_info(l: Option<Locale>, kind: u32, out: *mut u16, capacity: i32) -> i32 {
    let Some(l) = l else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let number = kind & LOCALE_RETURN_NUMBER != 0;
    let kind = kind & !(LOCALE_NOUSEROVERRIDE | LOCALE_USE_CP_ACP | LOCALE_RETURN_NUMBER);
    let Some(text) = info(l, kind) else {
        tracing::debug!(
            kind = format_args!("{kind:#x}"),
            "GetLocaleInfo: unknown LCTYPE"
        );
        return fail(ERROR_INVALID_FLAGS);
    };
    if !number {
        return deliver(&text, out, capacity);
    }
    // A DWORD in two units; the language ids are hexadecimal.
    let hex = matches!(kind, LOCALE_ILANGUAGE | LOCALE_IDEFAULTLANGUAGE);
    let Ok(value) = u32::from_str_radix(&text, if hex { 16 } else { 10 }) else {
        return fail(ERROR_INVALID_FLAGS);
    };
    if capacity == 0 {
        return 2;
    }
    if out.is_null() || capacity < 2 {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the caller's buffer of at least two units.
    unsafe { out.cast::<u32>().write_unaligned(value) };
    2
}

pub extern "win64" fn GetLocaleInfoEx(
    name: *const u16,
    kind: u32,
    out: *mut u16,
    capacity: i32,
) -> i32 {
    locale_info(named(name), kind, out, capacity)
}

pub extern "win64" fn GetLocaleInfoW(lcid: u32, kind: u32, out: *mut u16, capacity: i32) -> i32 {
    locale_info(by_lcid(lcid), kind, out, capacity)
}

// ---- dates and times ------------------------------------------------------------------------

use super::kernel32_time::{GetLocalTime, SystemTime, days_from_civil};

/// `t` in a Windows picture format (`dddd, MMMM d, yyyy`, `h:mm:ss tt`):
/// the date letters if `date`, else the time letters; `'...'` is literal
/// (`''` a quote), anything else is copied.
fn picture(format: &str, t: &SystemTime, date: bool) -> String {
    let mut out = String::new();
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let run = chars[i..].iter().take_while(|&&x| x == c).count();
        if c == '\'' {
            // `''` is a quote, in a quoted stretch or not.
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            i += 1;
            while let Some(&x) = chars.get(i) {
                i += 1;
                match x {
                    '\'' if chars.get(i) == Some(&'\'') => {
                        out.push('\'');
                        i += 1;
                    }
                    '\'' => break,
                    x => out.push(x),
                }
            }
            continue;
        }
        let n = |v: u16, run: usize| if run >= 2 { format!("{v:02}") } else { v.to_string() };
        let weekday = || {
            let days = days_from_civil(t.year.into(), t.month.into(), t.day.into());
            DAYS[(days + 3).rem_euclid(7) as usize]
        };
        let month = MONTHS[usize::from(t.month - 1)];
        let hour12 = if t.hour % 12 == 0 { 12 } else { t.hour % 12 };
        let marker = if t.hour < 12 { "AM" } else { "PM" };
        let text = match (date, c) {
            (true, 'd') if run >= 4 => weekday().to_string(),
            (true, 'd') if run == 3 => weekday()[..3].to_string(),
            (true, 'd') => n(t.day, run),
            (true, 'M') if run >= 4 => month.to_string(),
            (true, 'M') if run == 3 => month[..3].to_string(),
            (true, 'M') => n(t.month, run),
            (true, 'y') if run >= 3 => format!("{:04}", t.year),
            (true, 'y') => n(t.year % 100, run),
            (true, 'g') => "A.D.".to_string(),
            (false, 'h') => n(hour12, run),
            (false, 'H') => n(t.hour, run),
            (false, 'm') => n(t.minute, run),
            (false, 's') => n(t.second, run),
            (false, 't') if run >= 2 => marker.to_string(),
            (false, 't') => marker[..1].to_string(),
            _ => chars[i..i + run].iter().collect(),
        };
        out.push_str(&text);
        i += run;
    }
    out
}

/// The title's time, or now (local); `None` if a field is out of range.
fn when(time: *const SystemTime) -> Option<SystemTime> {
    let mut t = SystemTime::default();
    if time.is_null() {
        GetLocalTime(&mut t);
    } else {
        // SAFETY: the title's SYSTEMTIME.
        t = unsafe { time.read_unaligned() };
    }
    let ok = (1..=12).contains(&t.month)
        && (1..=31).contains(&t.day)
        && t.hour < 24
        && t.minute < 60
        && t.second < 60;
    ok.then_some(t)
}

fn format_time(
    locale: *const u16,
    time: *const SystemTime,
    format: *const u16,
    default: impl FnOnce() -> String,
    date: bool,
    out: *mut u16,
    capacity: i32,
) -> i32 {
    let (Some(_), Some(t)) = (named(locale), when(time)) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let format = wide(format).map_or_else(default, String::from_utf16_lossy);
    deliver(&picture(&format, &t, date), out, capacity)
}

pub extern "win64" fn GetDateFormatEx(
    locale: *const u16,
    flags: u32,
    date: *const SystemTime,
    format: *const u16,
    out: *mut u16,
    capacity: i32,
    _calendar: *const u16,
) -> i32 {
    const DATE_LONGDATE: u32 = 0x2;
    const DATE_YEARMONTH: u32 = 0x8;
    const DATE_MONTHDAY: u32 = 0x80;
    let default = || {
        if flags & DATE_LONGDATE != 0 {
            "dddd, MMMM d, yyyy"
        } else if flags & DATE_YEARMONTH != 0 {
            "MMMM yyyy"
        } else if flags & DATE_MONTHDAY != 0 {
            "MMMM d"
        } else {
            "M/d/yyyy"
        }
        .to_string()
    };
    format_time(locale, date, format, default, true, out, capacity)
}

/// The flags shape the default format only.
pub extern "win64" fn GetTimeFormatEx(
    locale: *const u16,
    flags: u32,
    time: *const SystemTime,
    format: *const u16,
    out: *mut u16,
    capacity: i32,
) -> i32 {
    const TIME_NOMINUTESORSECONDS: u32 = 0x1;
    const TIME_NOSECONDS: u32 = 0x2;
    const TIME_NOTIMEMARKER: u32 = 0x4;
    const TIME_FORCE24HOURFORMAT: u32 = 0x8;
    let default = || {
        let h24 = flags & TIME_FORCE24HOURFORMAT != 0;
        let mut f = String::from(if h24 { "H" } else { "h" });
        if flags & TIME_NOMINUTESORSECONDS == 0 {
            f.push_str(":mm");
            if flags & TIME_NOSECONDS == 0 {
                f.push_str(":ss");
            }
        }
        if !h24 && flags & TIME_NOTIMEMARKER == 0 {
            f.push_str(" tt");
        }
        f
    };
    format_time(locale, time, format, default, false, out, capacity)
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-locale",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-localization-*",
            "api-ms-win-core-datetime-*",
        ],
        exports: vec![
            hle_export!(GetDateFormatEx),
            hle_export!(GetTimeFormatEx),
            hle_export!(GetUserDefaultLocaleName),
            hle_export!(GetSystemDefaultLocaleName),
            hle_export!(GetUserDefaultLCID),
            hle_export!(GetSystemDefaultLCID),
            hle_export!(GetThreadLocale),
            hle_export!(GetUserDefaultLangID),
            hle_export!(GetSystemDefaultLangID),
            hle_export!(GetUserDefaultUILanguage),
            hle_export!(GetSystemDefaultUILanguage),
            hle_export!(GetUserDefaultGeoName),
            hle_export!(LocaleNameToLCID),
            hle_export!(LCIDToLocaleName),
            hle_export!(IsValidLocaleName),
            hle_export!(IsValidLocale),
            hle_export!(ResolveLocaleName),
            hle_export!(GetLocaleInfoEx),
            hle_export!(GetLocaleInfoW),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::kernel32_file::tests::w;
    use victory_loader::linux::teb::last_error;

    fn text(f: impl Fn(*mut u16, i32) -> i32) -> String {
        let mut buf = [0u16; 128];
        let n = f(buf.as_mut_ptr(), 128);
        assert!(n > 0);
        String::from_utf16_lossy(&buf[..n as usize - 1])
    }

    #[test]
    fn dates_and_times_in_pictures_and_defaults() {
        let t = SystemTime {
            year: 2026,
            month: 11,
            day_of_week: 0, // ignored: computed
            day: 4,
            hour: 15,
            minute: 7,
            second: 9,
            milliseconds: 0,
        };
        let null = std::ptr::null();
        let date = |flags, format: *const u16| text(|out, n| GetDateFormatEx(null, flags, &t, format, out, n, null));
        let time = |flags, format: *const u16| text(|out, n| GetTimeFormatEx(null, flags, &t, format, out, n));
        assert_eq!(date(0, null), "11/4/2026");
        assert_eq!(date(2, null), "Wednesday, November 4, 2026");
        let f = w("ddd dd MMM yy 'o''clock' ''");
        assert_eq!(date(0, f.as_ptr()), "Wed 04 Nov 26 o'clock '");
        assert_eq!(time(0, null), "3:07:09 PM");
        assert_eq!(time(8 | 2, null), "15:07");
        let f = w("HH'h'mm t");
        assert_eq!(time(0, f.as_ptr()), "15h07 P");
        let bad = SystemTime { month: 13, ..t };
        assert_eq!(GetDateFormatEx(null, 0, &bad, null, std::ptr::null_mut(), 0, null), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        // Now, when no time is given.
        assert!(GetTimeFormatEx(null, 0, std::ptr::null(), null, std::ptr::null_mut(), 0) > 0);
    }

    #[test]
    fn posix_names_map_cleanly_or_not_at_all() {
        assert_eq!(from_posix("fr_FR.UTF-8").map(|l| l.name), Some("fr-FR"));
        assert_eq!(from_posix("de_DE@euro").map(|l| l.lcid), Some(0x0407));
        assert_eq!(from_posix("C.UTF-8"), None);
        assert_eq!(from_posix("xx_YY"), None);
        let user = user();
        assert_eq!(text(|b, n| GetUserDefaultLocaleName(b, n)), user.name);
        assert_eq!(GetUserDefaultLangID(), user.lcid as u16);
        assert_eq!(text(|b, n| GetUserDefaultGeoName(b, n)), user.name[3..]);
    }

    #[test]
    fn names_and_lcids() {
        assert_eq!(LocaleNameToLCID(w("ja-jp").as_ptr(), 0), 0x0411);
        assert_eq!(LocaleNameToLCID(w("").as_ptr(), 0), 0x7F);
        assert_eq!(LocaleNameToLCID(std::ptr::null(), 0), user().lcid);
        assert_eq!(LocaleNameToLCID(w("xx-XX").as_ptr(), 0), 0);
        assert_eq!(last_error(), ERROR_INVALID_PARAMETER);
        assert_eq!(text(|b, n| LCIDToLocaleName(0x0C0A, b, n, 0)), "es-ES");
        assert_eq!(IsValidLocaleName(w("pt-BR").as_ptr()), 1);
        assert_eq!(IsValidLocaleName(w("pt-XX").as_ptr()), 0);
        assert_eq!(
            text(|b, n| ResolveLocaleName(w("zh").as_ptr(), b, n)),
            "zh-CN"
        );
        assert_eq!(
            text(|b, n| ResolveLocaleName(w("fr-CH").as_ptr(), b, n)),
            "fr-FR"
        );
        assert_eq!(
            ResolveLocaleName(w("xx").as_ptr(), std::ptr::null_mut(), 0),
            0
        );
        assert_eq!(
            GetUserDefaultLocaleName(std::ptr::null_mut(), 0) as usize,
            user().name.len() + 1
        );
        let mut small = [0u16; 2];
        assert_eq!(GetUserDefaultLocaleName(small.as_mut_ptr(), 2), 0);
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
    }

    #[test]
    fn locale_data() {
        let en = w("en-GB");
        assert_eq!(
            text(|b, n| GetLocaleInfoEx(en.as_ptr(), LOCALE_SNAME, b, n)),
            "en-GB"
        );
        assert_eq!(
            text(|b, n| GetLocaleInfoEx(en.as_ptr(), LOCALE_SENGLISHDISPLAYNAME, b, n)),
            "English (United Kingdom)"
        );
        assert_eq!(
            text(|b, n| GetLocaleInfoEx(en.as_ptr(), LOCALE_SISO3166CTRYNAME, b, n)),
            "GB"
        );
        assert_eq!(
            text(|b, n| GetLocaleInfoW(0x0407, LOCALE_ILANGUAGE, b, n)),
            "0407"
        );
        assert_eq!(text(|b, n| GetLocaleInfoW(0x0409, 0x22, b, n)), "0");
        assert_eq!(
            text(|b, n| GetLocaleInfoW(0x0409, LOCALE_SMONTHNAME1 + 11, b, n)),
            "December"
        );
        assert_eq!(
            text(|b, n| GetLocaleInfoW(0x0409, LOCALE_SABBREVDAYNAME1, b, n)),
            "Mon"
        );
        let mut out = [0u16; 2];
        assert_eq!(
            GetLocaleInfoW(
                0x0411,
                LOCALE_ILANGUAGE | LOCALE_RETURN_NUMBER,
                out.as_mut_ptr(),
                2
            ),
            2
        );
        assert_eq!(u32::from(out[0]) | u32::from(out[1]) << 16, 0x0411);
        assert_eq!(
            GetLocaleInfoW(0x0409, 0x1004 | LOCALE_RETURN_NUMBER, out.as_mut_ptr(), 2),
            2
        );
        assert_eq!(out[0], 1252);
        assert_eq!(GetLocaleInfoW(0x0409, 0x7777, out.as_mut_ptr(), 2), 0);
        assert_eq!(last_error(), ERROR_INVALID_FLAGS);
        assert_eq!(
            GetLocaleInfoEx(w("").as_ptr(), LOCALE_SNAME, out.as_mut_ptr(), 0),
            1
        );
    }
}
