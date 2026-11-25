//! Readable names for the symbols in a report.
//!
//! A console shader's container names its exported symbols in the Itanium
//! C++ ABI (`_Z...`), because the console's toolchain compiles HLSL to
//! native code through a C++-shaped backend. `Unsupported` reports are read
//! by a human looking for the one entry point that failed, so a report has
//! to show the name the way the title's own source spelled it.
//!
//! A name that is not mangled is already readable and is passed through
//! unchanged, which is also the answer for the already-demangled forms some
//! titles ship.

use std::borrow::Cow;

/// `symbol` as a human reads it: demangled when it is a mangled name, and
/// itself when it is not.
///
/// Demangling never fails the caller: a malformed or unsupported mangling
/// is a reason to show the raw name, not a reason to lose the report.
pub fn readable(symbol: &str) -> Cow<'_, str> {
    if !symbol.starts_with("_Z") && !symbol.starts_with("__Z") {
        return Cow::Borrowed(symbol);
    }
    match cpp_demangle::Symbol::new(symbol) {
        Ok(s) => match s.demangle() {
            Ok(text) => Cow::Owned(text),
            Err(_) => Cow::Borrowed(symbol),
        },
        Err(_) => Cow::Borrowed(symbol),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_is_passed_through() {
        assert_eq!(readable("main"), "main");
        assert_eq!(readable("PSCopyRect"), "PSCopyRect");
    }

    #[test]
    fn a_mangled_name_is_demangled() {
        // The mangling of `namespace::Class::method(int)`, which is the
        // shape a translated entry point's name takes.
        let mangled = "_ZN9namespace5Class6methodEi";
        assert_eq!(readable(mangled), "namespace::Class::method(int)");
    }

    #[test]
    fn a_broken_mangling_keeps_the_raw_name() {
        // A declared length longer than the name that follows it: not
        // demanglable, and a report must still name the symbol it is about.
        let broken = "_Z99short";
        assert_eq!(readable(broken), broken);
    }
}
