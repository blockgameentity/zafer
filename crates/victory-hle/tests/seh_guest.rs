//! Title-code exception handling on Linux, end to end: tests/fixtures/seh-probe
//! (__try/__except/__finally, C++ throw/catch, faults, vectored handlers)
//! built with clang-cl against the xwin SDK, run by its static vcruntime's
//! language handlers on our dispatcher. Skips when the toolchain is absent.
#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use victory_hle::{HleResolver, HostPolicy};
use victory_loader::{ImportResolver, Resolution};
use victory_pe::{ImageInfo, ImportSymbol};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn first_entry(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    entries.pop()
}

/// Builds the probe DLL, or says why it cannot.
fn build_probe() -> Result<PathBuf, String> {
    let xwin = std::env::var_os("XWIN_SDK")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share/xwin-sdk")))
        .ok_or("no xwin SDK")?;
    let msvc = first_entry(&xwin.join("VC/Tools/MSVC")).ok_or("no MSVC libraries in the xwin SDK")?;
    let kits = xwin.join("Windows Kits/10/Lib");
    let sdk = first_entry(&kits).ok_or("no Windows SDK libraries in the xwin SDK")?;
    let source = workspace_root().join("tests/fixtures/seh-probe");
    let out = workspace_root().join("target/fixtures/seh-probe");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let run = |c: &mut Command| match c.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{c:?} failed: {s}")),
        Err(e) => Err(format!("{c:?}: {e}")),
    };
    for name in ["seh_probe", "crt"] {
        run(Command::new("clang-cl")
            .args(["--target=x86_64-pc-windows-msvc", "/c", "/O1", "/EHa", "/MT", "/GS-", "/Zl", "-fno-builtin"])
            .arg(format!("/Fo{}", out.join(format!("{name}.obj")).display()))
            .arg(source.join(format!("{name}.cpp"))))?;
    }
    let dll = out.join("seh_probe.dll");
    run(Command::new("lld-link")
        .args(["/dll", "/noentry", "/nodefaultlib", "/fixed", "/base:0x7e0000000"])
        .arg(format!("/out:{}", dll.display()))
        .arg(out.join("seh_probe.obj"))
        .arg(out.join("crt.obj"))
        .arg(msvc.join("lib/x64/libvcruntime.lib"))
        .arg(msvc.join("lib/x64/libcmt.lib"))
        .arg(sdk.join("um/x64/kernel32.lib")))?;
    Ok(dll)
}

// What the static vcruntime imports that the kernel32 slices do not serve yet.
thread_local! {
    static TLS: RefCell<[usize; 64]> = const { RefCell::new([0; 64]) };
    static LAST_ERROR: RefCell<u32> = const { RefCell::new(0) };
}
static NEXT_TLS: AtomicU32 = AtomicU32::new(1);
extern "win64" fn tls_alloc() -> u32 {
    NEXT_TLS.fetch_add(1, Ordering::Relaxed)
}
extern "win64" fn tls_free(_: u32) -> i32 {
    1
}
extern "win64" fn tls_get_value(i: u32) -> usize {
    TLS.with(|t| t.borrow()[i as usize % 64])
}
extern "win64" fn tls_set_value(i: u32, v: usize) -> i32 {
    TLS.with(|t| t.borrow_mut()[i as usize % 64] = v);
    1
}
extern "win64" fn get_last_error() -> u32 {
    LAST_ERROR.with(|e| *e.borrow())
}
extern "win64" fn set_last_error(v: u32) {
    LAST_ERROR.with(|e| *e.borrow_mut() = v);
}
extern "win64" fn identity(p: usize) -> usize {
    p
}
extern "win64" fn returns_one() -> i32 {
    1
}
extern "win64" fn returns_zero() -> i32 {
    0
}
extern "win64" fn current_process() -> isize {
    -1
}

fn fake(name: &str) -> Option<usize> {
    Some(match name {
        "TlsAlloc" => tls_alloc as *const () as usize,
        "TlsFree" => tls_free as *const () as usize,
        "TlsGetValue" => tls_get_value as *const () as usize,
        "TlsSetValue" => tls_set_value as *const () as usize,
        "GetLastError" => get_last_error as *const () as usize,
        "SetLastError" => set_last_error as *const () as usize,
        "EncodePointer" => identity as *const () as usize,
        "InitializeCriticalSectionAndSpinCount" => returns_one as *const () as usize,
        "DeleteCriticalSection" | "IsProcessorFeaturePresent" => returns_zero as *const () as usize,
        "GetCurrentProcess" => current_process as *const () as usize,
        _ => return None,
    })
}

/// Maps the probe at its fixed base (no relocations), binds its imports
/// and registers it with the dispatcher. Returns its exports.
fn map(dll: &Path) -> impl Fn(&str) -> usize {
    let bytes = std::fs::read(dll).unwrap();
    let info = ImageInfo::parse(&bytes).unwrap();
    let base = info.image_base as usize;
    let size = info.size_of_image as usize;
    // SAFETY: a fresh mapping at the probe's base, which nothing else uses.
    let at = unsafe {
        libc::mmap(
            base as *mut libc::c_void,
            size,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    assert_eq!(at as usize, base, "mapping the probe at its base");
    let image = |rva: usize, len: usize| {
        // SAFETY: inside the mapping.
        unsafe { std::slice::from_raw_parts_mut((base + rva) as *mut u8, len) }
    };
    image(0, 0x400).copy_from_slice(&bytes[..0x400]);
    for s in &info.sections {
        let len = (s.file_size as usize).min(s.virtual_size as usize);
        image(s.rva as usize, len).copy_from_slice(&bytes[s.file_offset as usize..][..len]);
    }
    let resolver = HleResolver::new(HostPolicy::default());
    for m in &info.imports {
        for e in &m.entries {
            let ImportSymbol::Name { name, .. } = &e.symbol else { panic!("{:?}", e.symbol) };
            let address = fake(name)
                .or_else(|| match resolver.resolve(&m.dll, &e.symbol) {
                    Resolution::Found { address, .. } => Some(address),
                    Resolution::Missing => None,
                })
                .unwrap_or_else(|| panic!("{}!{name} is not served", m.dll));
            image(e.iat_slot_rva as usize, 8).copy_from_slice(&address.to_le_bytes());
        }
    }
    victory_loader::linux::seh::register_image(base, size);
    let exports: Vec<_> = info.exports.iter().map(|e| (e.name.clone().unwrap_or_default(), e.rva)).collect();
    move |name| {
        let rva = exports.iter().find(|(n, _)| n == name).and_then(|(_, r)| *r).unwrap();
        base + rva as usize
    }
}

const CHILD: &str = "VICTORY_SEH_PROBE_CHILD";

#[test]
fn title_exceptions_reach_title_handlers() {
    let dll = match build_probe() {
        Ok(dll) => dll,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let export = map(&dll);
    let call = |name: &str| {
        // SAFETY: the probe's exports take no arguments and return an int.
        let f: extern "win64" fn() -> i32 = unsafe { std::mem::transmute(export(name)) };
        f()
    };
    assert_eq!(call("probe_init"), 0, "__vcrt_initialize");

    if let Some(log) = std::env::var_os(CHILD) {
        victory_loader::log_crashes_to(PathBuf::from(log));
        call("seh_unhandled");
        unreachable!("an unhandled exception returned");
    }

    assert_eq!(call("seh_faults"), 0, "faults caught by __except");
    assert_eq!(call("seh_raise"), 0, "RaiseException, __finally, continue, nested");
    assert_eq!(call("seh_vectored"), 0, "vectored handlers");
    assert_eq!(call("cpp_throw"), 0, "C++ throw and catch");
    // Twice: the C++ runtime's per-thread state must be back as it was.
    assert_eq!(call("cpp_throw"), 0, "C++ throw and catch, again");
    // And on another thread.
    let thread_result = std::thread::scope(|s| s.spawn(|| call("cpp_throw")).join().unwrap());
    assert_eq!(thread_result, 0, "C++ on a second thread");

    let mut frames = [0usize; 8];
    // SAFETY: probe_backtrace fills at most 8 frames.
    let f: extern "win64" fn(*mut usize) -> i32 = unsafe { std::mem::transmute(export("probe_backtrace")) };
    let n = f(frames.as_mut_ptr()) as usize;
    let base = export("probe_init") & !0xFFFF;
    assert!(n >= 3, "{n} frames");
    assert!(frames[..3].iter().all(|&pc| pc > base && pc < base + 0x10_0000), "{frames:x?}");

    // Nobody handles a fault: a crash report, and the process ends with the code.
    let log = workspace_root().join("target/fixtures/seh-probe/crashes.log");
    let _ = std::fs::remove_file(&log);
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "title_exceptions_reach_title_handlers", "--nocapture"])
        .env(CHILD, &log)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert_eq!(child.status.code(), Some(0x05), "exit status: 0xC0000005's low byte\n{stderr}");
    assert!(
        stderr.contains("VIctory: unhandled exception 0xc0000005 at seh_probe.dll+0x"),
        "{stderr}"
    );
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(logged.contains("unhandled exception 0xc0000005") && logged.contains("(write 0x10)"), "{logged}");
}
