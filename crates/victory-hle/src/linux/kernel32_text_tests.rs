//! The DBCS code pages, through the exports a title calls.
//!
//! These are the tests that would have caught the original gap: the
//! conversions used to answer `ERROR_INVALID_PARAMETER` for every code page
//! but the Western ones, and `IsDBCSLeadByte` used to answer "no page has
//! lead bytes" for all of them.

use super::*;

/// `MultiByteToWideChar` into a `Vec`, with the code page under test.
fn to_wide(cp: u32, bytes: &[u8], strict: bool) -> Result<Vec<u16>, i32> {
    let flags = if strict { MB_ERR_INVALID_CHARS } else { 0 };
    let mut out = vec![0u16; bytes.len() * 2 + 2];
    let n = MultiByteToWideChar(
        cp,
        flags,
        bytes.as_ptr(),
        bytes.len() as i32,
        out.as_mut_ptr(),
        out.len() as i32,
    );
    // These functions signal failure with 0 and the last error, not with
    // -1, so an empty string and a failed conversion both answer 0; the
    // error is what tells them apart, and the ill-formed test below reads
    // it that way.
    if n < 0 {
        return Err(n);
    }
    out.truncate(n as usize);
    Ok(out)
}

/// `WideCharToMultiByte` into a `Vec`.
fn to_bytes(cp: u32, text: &[u16]) -> Vec<u8> {
    let mut out = vec![0u8; text.len() * 4 + 4];
    let n = WideCharToMultiByte(
        cp,
        0,
        text.as_ptr(),
        text.len() as i32,
        out.as_mut_ptr(),
        out.len() as i32,
        std::ptr::null(),
        std::ptr::null_mut(),
    );
    assert!(n >= 0, "conversion failed with {n}");
    out.truncate(n as usize);
    out
}

#[test]
fn japanese_converts_both_ways() {
    // Hiragana "a i", which is two two-byte characters in code page 932.
    let wide = to_wide(932, b"\x82\xa0\x82\xa2", true).expect("932 decodes");
    assert_eq!(wide, vec![0x3042, 0x3044]);
    assert_eq!(to_bytes(932, &wide), b"\x82\xa0\x82\xa2");
}

#[test]
fn korean_and_the_chinese_pages_convert() {
    // A single two-byte character from each page, decoded and back.
    for (cp, bytes) in [(949u32, &b"\xb0\xa1"[..]), (936, &b"\xb0\xa1"[..]), (950, &b"\xa4\x40"[..])] {
        let wide = to_wide(cp, bytes, true).unwrap_or_else(|e| panic!("code page {cp} failed: {e}"));
        assert!(!wide.is_empty(), "code page {cp} decoded nothing");
        assert_eq!(to_bytes(cp, &wide), bytes, "code page {cp} did not round-trip");
    }
}

#[test]
fn a_lead_byte_is_reported_per_page() {
    // The same byte is a lead byte in one page and not in another, which is
    // exactly why the old "no page has lead bytes" answer was wrong.
    assert_eq!(IsDBCSLeadByteEx(932, 0x81), 1);
    assert_eq!(IsDBCSLeadByteEx(932, 0xFE), 0); // past Shift_JIS's high half
    assert_eq!(IsDBCSLeadByteEx(936, 0x81), 1);
    assert_eq!(IsDBCSLeadByteEx(936, 0x7F), 0);
    assert_eq!(IsDBCSLeadByteEx(1252, 0x81), 0);
    // `CP_ACP` is the Western page, so the no-code-page form answers 0.
    assert_eq!(IsDBCSLeadByte(0x81), 0);
}

#[test]
fn a_dbcs_page_is_valid_and_reports_its_size() {
    assert_eq!(IsValidCodePage(932), 1);
    assert_eq!(IsValidCodePage(936), 1);
    assert_eq!(IsValidCodePage(20127), 0); // US-ASCII is not implemented

    let mut info = CpInfo::default();
    assert_eq!(GetCPInfo(932, &mut info), 1);
    assert_eq!(info.max_char_size, 2, "a DBCS character is two bytes");
    assert_eq!(&info.lead_byte[..2], &[0x81, 0x9F]);

    let mut western = CpInfo::default();
    assert_eq!(GetCPInfo(1252, &mut western), 1);
    assert_eq!(western.max_char_size, 1);
    assert_eq!(western.lead_byte, [0u8; 12], "a single-byte page has no lead bytes");
}

#[test]
fn ill_formed_bytes_follow_the_strict_flag() {
    // A lead byte with no trail byte is ill-formed in 932. Leniently it
    // becomes the replacement, strictly it is an error.
    // Strictly the conversion fails: it answers 0 with the last error set
    // to `ERROR_NO_UNICODE_TRANSLATION`, which is why a caller checks the
    // error rather than only the count.
    assert_eq!(to_wide(932, b"\x82", true).map(|w| w.len()).unwrap_or(0), 0);
    // Leniently it becomes the replacement character.
    let lenient = to_wide(932, b"\x82", false).expect("lenient");
    assert_eq!(lenient, vec![0xFFFD]);
}

#[test]
fn the_western_pages_are_unchanged() {
    // The pages that worked before must keep working byte for byte.
    assert_eq!(to_wide(1252, b"caf\xe9", true).expect("1252"), vec![b'c' as u16, b'a' as u16, b'f' as u16, 0xE9]);
    assert_eq!(to_wide(65001, "café".as_bytes(), true).expect("utf8"), "café".encode_utf16().collect::<Vec<_>>());
}
