//! Module handles for the system DLLs this layer serves: on Windows,
//! `GetModuleHandle("kernel32")` and `LoadLibrary("bcrypt.dll")` return the
//! host's real modules; here they return a pseudo module per DLL name that
//! `GetProcAddress` resolves against [`super::lookup`].

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Pseudo modules by lower-case file name, and their names by handle.
#[derive(Default)]
struct Table {
    by_name: HashMap<String, usize>,
    by_handle: HashMap<usize, String>,
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(Default::default);

/// The file name a module name means: no folder, `.dll` when there is no
/// extension, lower case.
fn file_name(name: &str) -> String {
    let file = name.rsplit(['\\', '/']).next().unwrap_or(name);
    let file = file.to_ascii_lowercase();
    if file.contains('.') { file } else { format!("{file}.dll") }
}

/// The pseudo module of `name`, if we serve that DLL.
pub(crate) fn handle(name: &str) -> Option<usize> {
    let file = file_name(name);
    if !super::serves(&file) {
        return None;
    }
    let mut table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&h) = table.by_name.get(&file) {
        return Some(h);
    }
    // A zeroed, 64 KiB-aligned page: never title memory, and a title that
    // looks for a PE header there finds none.
    let layout = std::alloc::Layout::from_size_align(0x1000, 0x1_0000).ok()?;
    // SAFETY: a non-zero size; the page is never freed (modules stay loaded).
    let page = unsafe { std::alloc::alloc_zeroed(layout) } as usize;
    if page == 0 {
        return None;
    }
    table.by_name.insert(file.clone(), page);
    table.by_handle.insert(page, file);
    Some(page)
}

/// The DLL file name behind a pseudo module.
pub(crate) fn name(handle: usize) -> Option<String> {
    let table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    table.by_handle.get(&handle).cloned()
}

/// `GetProcAddress` on a pseudo module: by name, or by ordinal for values
/// below 0x10000.
pub(crate) fn proc_address(handle: usize, name: Option<&str>, ordinal: u16) -> Option<usize> {
    let file = self::name(handle)?;
    match name {
        Some(name) => super::lookup(&file, name),
        None => super::lookup_ordinal(&file, ordinal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn served_dlls_have_one_handle_that_resolves_their_exports() {
        let h = handle("KERNEL32").expect("kernel32 is served");
        assert_eq!(handle(r"C:\Windows\System32\kernel32.dll"), Some(h));
        assert_eq!(h % 0x1_0000, 0);
        assert_eq!(name(h).as_deref(), Some("kernel32.dll"));
        let sleep = proc_address(h, Some("SleepConditionVariableCS"), 0);
        assert_eq!(sleep, super::super::lookup("kernel32.dll", "SleepConditionVariableCS"));
        assert!(sleep.is_some());
        assert!(handle("wininet.dll").is_none());
    }
}
