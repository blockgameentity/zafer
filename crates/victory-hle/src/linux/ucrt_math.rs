//! ucrtbase: math (`api-ms-win-crt-math`) over the host's libm. Both follow
//! C99 Annex F for the special values; the last bit of a result can differ
//! from the UCRT's. What is not libm's: the guest's `long` is 32 bits
//! (`lroundf`), its `long double` is a double (`nexttowardf`, `_ldsign`),
//! and the classification helpers answer with the UCRT's own codes.
//! ponytail: libm reports domain and range errors in glibc's errno, not the
//! guest's; route them through if a title reads errno after math.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::{HleModule, hle_export};

mod libm {
    unsafe extern "C" {
        pub safe fn acos(x: f64) -> f64;
        pub safe fn asin(x: f64) -> f64;
        pub safe fn atan(x: f64) -> f64;
        pub safe fn atan2(y: f64, x: f64) -> f64;
        pub safe fn ceil(x: f64) -> f64;
        pub safe fn cos(x: f64) -> f64;
        pub safe fn cosh(x: f64) -> f64;
        pub safe fn exp(x: f64) -> f64;
        pub safe fn exp2(x: f64) -> f64;
        pub safe fn expm1(x: f64) -> f64;
        pub safe fn floor(x: f64) -> f64;
        pub safe fn fmod(x: f64, y: f64) -> f64;
        pub safe fn ldexp(x: f64, e: i32) -> f64;
        pub safe fn log(x: f64) -> f64;
        pub safe fn log1p(x: f64) -> f64;
        pub safe fn log2(x: f64) -> f64;
        pub safe fn pow(x: f64, y: f64) -> f64;
        pub safe fn sin(x: f64) -> f64;
        pub safe fn sinh(x: f64) -> f64;
        pub safe fn sqrt(x: f64) -> f64;
        pub safe fn tan(x: f64) -> f64;
        pub safe fn acosf(x: f32) -> f32;
        pub safe fn asinf(x: f32) -> f32;
        pub safe fn atanf(x: f32) -> f32;
        pub safe fn atan2f(y: f32, x: f32) -> f32;
        pub safe fn cbrtf(x: f32) -> f32;
        pub safe fn ceilf(x: f32) -> f32;
        pub safe fn cosf(x: f32) -> f32;
        pub safe fn erff(x: f32) -> f32;
        pub safe fn exp2f(x: f32) -> f32;
        pub safe fn expf(x: f32) -> f32;
        pub safe fn fmodf(x: f32, y: f32) -> f32;
        pub safe fn log10f(x: f32) -> f32;
        pub safe fn log2f(x: f32) -> f32;
        pub safe fn logf(x: f32) -> f32;
        pub safe fn nextafterf(x: f32, y: f32) -> f32;
        pub safe fn powf(x: f32, y: f32) -> f32;
        pub safe fn remainderf(x: f32, y: f32) -> f32;
        pub safe fn sinf(x: f32) -> f32;
        pub safe fn sinhf(x: f32) -> f32;
        pub safe fn sqrtf(x: f32) -> f32;
        pub safe fn tanf(x: f32) -> f32;
        pub fn frexp(x: f64, e: *mut i32) -> f64;
        pub fn modf(x: f64, whole: *mut f64) -> f64;
        pub fn modff(x: f32, whole: *mut f32) -> f32;
    }
}

/// `extern "win64"` wrappers that forward to libm one to one.
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {
        $(pub extern "win64" fn $name($($arg: $ty),*) -> $ret { libm::$name($($arg),*) })*
        fn forwarded() -> Vec<crate::HleExport> { vec![$(hle_export!($name)),*] }
    };
}

forward! {
    acos(x: f64) -> f64;
    asin(x: f64) -> f64;
    atan(x: f64) -> f64;
    atan2(y: f64, x: f64) -> f64;
    ceil(x: f64) -> f64;
    cos(x: f64) -> f64;
    cosh(x: f64) -> f64;
    exp(x: f64) -> f64;
    exp2(x: f64) -> f64;
    expm1(x: f64) -> f64;
    floor(x: f64) -> f64;
    fmod(x: f64, y: f64) -> f64;
    ldexp(x: f64, e: i32) -> f64;
    log(x: f64) -> f64;
    log1p(x: f64) -> f64;
    log2(x: f64) -> f64;
    pow(x: f64, y: f64) -> f64;
    sin(x: f64) -> f64;
    sinh(x: f64) -> f64;
    sqrt(x: f64) -> f64;
    tan(x: f64) -> f64;
    acosf(x: f32) -> f32;
    asinf(x: f32) -> f32;
    atanf(x: f32) -> f32;
    atan2f(y: f32, x: f32) -> f32;
    cbrtf(x: f32) -> f32;
    ceilf(x: f32) -> f32;
    cosf(x: f32) -> f32;
    erff(x: f32) -> f32;
    exp2f(x: f32) -> f32;
    expf(x: f32) -> f32;
    fmodf(x: f32, y: f32) -> f32;
    log10f(x: f32) -> f32;
    log2f(x: f32) -> f32;
    logf(x: f32) -> f32;
    nextafterf(x: f32, y: f32) -> f32;
    powf(x: f32, y: f32) -> f32;
    remainderf(x: f32, y: f32) -> f32;
    sinf(x: f32) -> f32;
    sinhf(x: f32) -> f32;
    sqrtf(x: f32) -> f32;
    tanf(x: f32) -> f32;
}

pub extern "win64" fn frexp(x: f64, exponent: *mut i32) -> f64 {
    // SAFETY: the C contract: exponent is writable.
    unsafe { libm::frexp(x, exponent) }
}

pub extern "win64" fn modf(x: f64, whole: *mut f64) -> f64 {
    // SAFETY: the C contract: whole is writable.
    unsafe { libm::modf(x, whole) }
}

pub extern "win64" fn modff(x: f32, whole: *mut f32) -> f32 {
    // SAFETY: the C contract: whole is writable.
    unsafe { libm::modff(x, whole) }
}

/// `long double` is a double on Windows.
pub extern "win64" fn nexttowardf(x: f32, toward: f64) -> f32 {
    if x.is_nan() || toward.is_nan() {
        return x + toward as f32;
    }
    if f64::from(x) == toward {
        return toward as f32;
    }
    libm::nextafterf(x, if toward > f64::from(x) { f32::INFINITY } else { f32::NEG_INFINITY })
}

/// A value that does not fit the guest's 32-bit `long` is what the
/// conversion instruction gives there.
pub extern "win64" fn lroundf(x: f32) -> i32 {
    let r = x.round();
    if (-2147483648.0..2147483648.0).contains(&r) { r as i32 } else { i32::MIN }
}

/// math.h's codes: `_NANCODE` 2, `_INFCODE` 1, zero 0, `_FINITE` -1,
/// `_DENORM` -2.
fn classify(category: std::num::FpCategory) -> i16 {
    use std::num::FpCategory::*;
    match category {
        Nan => 2,
        Infinite => 1,
        Zero => 0,
        Subnormal => -2,
        Normal => -1,
    }
}

pub extern "win64" fn _dtest(x: *const f64) -> i16 {
    // SAFETY: the C contract: a readable double.
    classify(unsafe { *x }.classify())
}

pub extern "win64" fn _fdtest(x: *const f32) -> i16 {
    // SAFETY: the C contract: a readable float.
    classify(unsafe { *x }.classify())
}

/// The sign bit as it sits in the top 16 bits: 0x8000 or 0.
pub extern "win64" fn _dsign(x: f64) -> i16 {
    if x.is_sign_negative() { i16::MIN } else { 0 }
}

pub extern "win64" fn _ldsign(x: f64) -> i16 {
    _dsign(x)
}

pub extern "win64" fn _fdsign(x: f32) -> i16 {
    if x.is_sign_negative() { i16::MIN } else { 0 }
}

pub extern "win64" fn _isnan(x: f64) -> i32 {
    i32::from(x.is_nan())
}

/// The `_matherr` hook: stored by a start-up, called by nothing here.
pub extern "win64" fn __setusermatherr(_hook: usize) {}

pub(crate) fn module() -> HleModule {
    let mut exports = forwarded();
    exports.extend([
        hle_export!(frexp),
        hle_export!(modf),
        hle_export!(modff),
        hle_export!(nexttowardf),
        hle_export!(lroundf),
        hle_export!(_dtest),
        hle_export!(_fdtest),
        hle_export!(_dsign),
        hle_export!(_ldsign),
        hle_export!(_fdsign),
        hle_export!(_isnan),
        hle_export!(__setusermatherr),
    ]);
    HleModule {
        name: "ucrt-math",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-math-*"],
        exports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_values_and_the_ucrts_codes() {
        assert_eq!(pow(0.0, 0.0), 1.0);
        assert!(fmod(1.0, 0.0).is_nan());
        assert_eq!(atan2(0.0, -0.0), std::f64::consts::PI);
        assert_eq!(sqrtf(4.0), 2.0);
        assert_eq!(ldexp(1.0, 10), 1024.0);
        let mut e = 0;
        assert_eq!(frexp(8.0, &mut e), 0.5);
        assert_eq!(e, 4);
        let mut whole = 0.0;
        assert_eq!(modf(-3.25, &mut whole), -0.25);
        assert_eq!(whole, -3.0);
        assert_eq!(lroundf(2.5), 3);
        assert_eq!(lroundf(-2.5), -3);
        assert_eq!(lroundf(3e9), i32::MIN);
        assert_eq!(nexttowardf(1.0, 2.0), 1.0 + f32::EPSILON);
        assert_eq!(nexttowardf(1.0, 1.0), 1.0);
        assert_eq!(_dtest(&f64::NAN), 2);
        assert_eq!(_dtest(&f64::INFINITY), 1);
        assert_eq!(_dtest(&0.0), 0);
        assert_eq!(_dtest(&1.0), -1);
        assert_eq!(_fdtest(&1e-40), -2);
        assert_eq!(_dsign(-0.0), i16::MIN);
        assert_eq!(_fdsign(1.0), 0);
        assert_eq!(_isnan(f64::NAN), 1);
        assert!((erff(1.0) - 0.842_700_8).abs() < 1e-6);
    }
}
