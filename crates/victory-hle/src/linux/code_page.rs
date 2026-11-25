//! The code pages `kernel32`'s text functions convert between.
//!
//! Windows names a code page by number, and a title picks one with
//! `GetACP`/`GetOEMCP` or passes it to `MultiByteToWideChar` directly. We
//! implement the ones a title can actually select, and refuse nothing that
//! a DBCS title would ask for: Japanese (932), Korean (949) and the two
//! Chinese pages (936, 950) are all lead-byte pages, and a conversion that
//! refused them would fail every non-Latin title at its first string.
//!
//! The conversions are `encoding_rs`'s, which is the WHATWG reference
//! implementation: its tables are the ones browsers use, so what a title
//! gets here is what it gets on a real desktop. What stays ours is the
//! Windows-shaped part — which code page numbers map to which encoding, and
//! `IsDBCSLeadByte`'s answer for each.

use encoding_rs::{Encoding, GBK, SHIFT_JIS};

/// The code page numbers a title can select, and what each is.
///
/// `1252`/`437` are single-byte; the rest are DBCS. `CP_ACP` and
/// `CP_THREAD_ACP` are the machine's ANSI page (Western on every console
/// we target) and `CP_OEMCP` the OEM one, as `GetACP`/`GetOEMCP` report.
pub const CP_ACP: u32 = 0;
pub const CP_OEMCP: u32 = 1;
pub const CP_THREAD_ACP: u32 = 3;
pub const CP_UTF8: u32 = 65001;

/// The encoding for a Windows code page number, or `None` when we do not
/// implement it. `None` is what `IsValidCodePage` reports and what the
/// conversion functions turn into `ERROR_INVALID_PARAMETER`.
pub fn encoding(cp: u32) -> Option<&'static Encoding> {
    Some(match cp {
        CP_ACP | CP_THREAD_ACP | 1252 => encoding_rs::WINDOWS_1252,
        CP_OEMCP | 437 => encoding_rs::IBM866,
        CP_UTF8 => encoding_rs::UTF_8,
        // Japanese. Windows calls it 932; WHATWG's `shift_jis` is the same
        // byte mapping (including the 0x80/0xA0/0xFD-0xFF extensions).
        932 => SHIFT_JIS,
        // Korean: WHATWG's `euc-kr` is Windows code page 949.
        949 => encoding_rs::EUC_KR,
        // Simplified Chinese: WHATWG's `gbk` is Windows code page 936.
        936 => GBK,
        // Traditional Chinese: WHATWG's `big5` is Windows code page 950.
        950 => encoding_rs::BIG5,
        _ => return None,
    })
}

/// Whether `cp` is a lead-byte (DBCS) code page, which is what
/// `IsDBCSLeadByteEx` answers for a byte in its lead range.
pub fn is_dbcs(cp: u32) -> bool {
    matches!(cp, 932 | 936 | 949 | 950)
}

/// `IsDBCSLeadByteEx`: is `byte` the first byte of a two-byte character in
/// code page `cp`?
///
/// The ranges are the ones the Windows pages define, and they differ per
/// page, which is why this cannot be a single range check.
pub fn is_lead_byte(cp: u32, byte: u8) -> bool {
    match cp {
        // Shift_JIS.
        932 => matches!(byte, 0x81..=0x9F | 0xE0..=0xFC),
        // GBK.
        936 => matches!(byte, 0x81..=0xFE),
        // EUC-KR.
        949 => matches!(byte, 0x81..=0xFE),
        // Big5.
        950 => matches!(byte, 0x81..=0xFE),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_page_resolves() {
        for cp in [CP_ACP, CP_THREAD_ACP, 1252, CP_OEMCP, 437, CP_UTF8] {
            assert!(encoding(cp).is_some(), "code page {cp}");
        }
        for cp in [932, 936, 949, 950] {
            assert!(encoding(cp).is_some(), "code page {cp}");
            assert!(is_dbcs(cp));
        }
    }

    #[test]
    fn an_unimplemented_page_is_refused() {
        // 20127 (US-ASCII) and 1200 (UTF-16LE) are not pages we convert.
        assert!(encoding(20127).is_none());
        assert!(encoding(1200).is_none());
        assert!(!is_dbcs(20127));
    }

    #[test]
    fn a_japanese_string_round_trips() {
        let page = encoding(932).expect("932");
        let (wide, _, unmappable) = page.decode(b"\x82\xa0\x82\xa2");
        assert!(!unmappable, "the bytes are valid Shift_JIS");
        assert_eq!(wide, "\u{3042}\u{3044}"); // hiragana a, i
        let (back, _, unmappable) = page.encode(&wide);
        assert!(!unmappable);
        assert_eq!(&back[..], b"\x82\xa0\x82\xa2");
    }

    #[test]
    fn lead_byte_ranges_differ_by_page() {
        // 0x80 is a lead byte in no page we implement, and 0x81 in every
        // DBCS one but Shift_JIS's high half starts later than its low.
        assert!(!is_lead_byte(932, 0x80));
        assert!(is_lead_byte(932, 0x81));
        assert!(is_lead_byte(932, 0xE0));
        assert!(!is_lead_byte(932, 0xFD));
        assert!(is_lead_byte(936, 0x81));
        assert!(!is_lead_byte(936, 0x7F));
        assert!(!is_lead_byte(1252, 0x81));
    }

    #[test]
    fn the_ansi_page_is_western() {
        // `GetACP` answers `CP_ACP`'s page; a title comparing it to 1252
        // takes the same path either way.
        assert_eq!(encoding(CP_ACP), encoding(1252));
        assert_eq!(encoding(CP_OEMCP), encoding(437));
    }
}
