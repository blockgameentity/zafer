//! End-to-end on Linux: map, bind and call into a stand-in title
//! (tests/fixtures/hle-probe) and a DLL it ships (tests/fixtures/probe-title),
//! on guest threads, without any game data. The fixtures are PE files built
//! with `lld-link` against an xwin SDK (`XWIN_DIR`, default
//! `~/.local/share/xwin-sdk`); without one the test skips.
#![cfg(target_os = "linux")]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use victory_loader::linux::{teb, thread};
use victory_loader::{
    ImportResolver, Provenance, Resolution, UnimplementedPolicy, WithTitleDlls,
    unimplemented_call_count,
};
use victory_pe::ImportSymbol;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `<dir>/<one subdirectory>/<rest>`: the SDK's versioned folders.
fn versioned(dir: &Path, rest: &str) -> Option<PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path().join(rest))
        .collect();
    found.sort();
    found.pop().filter(|p| p.is_dir())
}

/// The `LIB` search path of the xwin SDK; `None` if a piece is missing.
fn xwin_lib() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let xwin = std::env::var_os("XWIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join(".local/share/xwin-sdk"));
    let kits = xwin.join("Windows Kits/10/Lib");
    let dirs = [
        versioned(&xwin.join("VC/Tools/MSVC"), "lib/x64")?,
        versioned(&kits, "ucrt/x64")?,
        versioned(&kits, "um/x64")?,
    ];
    Some(dirs.map(|d| d.display().to_string()).join(";"))
}

/// Builds a fixture into its own target dir (the outer cargo holds the lock
/// on the main one while tests run); `None` when the toolchain is absent.
fn build_fixture(name: &str, file: &str) -> Option<PathBuf> {
    let lib = xwin_lib()?;
    if !Path::new("/usr/bin/lld-link").exists() && which("lld-link").is_none() {
        return None;
    }
    let root = workspace_root();
    let target = root.join("target/fixtures");
    let status = Command::new(env!("CARGO"))
        .args([
            "build",
            "--quiet",
            "--target",
            "x86_64-pc-windows-msvc",
            "--manifest-path",
        ])
        .arg(root.join("tests/fixtures").join(name).join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .env("LIB", lib)
        .env("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER", "lld-link")
        .status()
        .ok()?;
    // A missing Rust target is a missing toolchain, not a failure.
    status
        .success()
        .then(|| target.join("x86_64-pc-windows-msvc/debug").join(file))
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(tool))
        .find(|p| p.is_file())
}

extern "win64" fn fake_xgameruntime_initialize() -> i32 {
    0x1234
}
extern "win64" fn fake_get_tick_count() -> u32 {
    777
}
extern "win64" fn fake_get_current_process_id() -> u32 {
    std::process::id()
}
extern "win64" fn fake_delayed() -> u32 {
    4242
}

// What the probe's Rust std needs to allocate and copy: the real ones are
// victory-hle's.
extern "win64" fn fake_get_process_heap() -> usize {
    1
}
extern "win64" fn fake_heap_alloc(_: usize, flags: u32, size: usize) -> *mut c_void {
    const HEAP_ZERO_MEMORY: u32 = 8;
    // SAFETY: plain C allocations.
    unsafe {
        if flags & HEAP_ZERO_MEMORY != 0 {
            libc::calloc(1, size)
        } else {
            libc::malloc(size)
        }
    }
}
extern "win64" fn fake_heap_realloc(_: usize, _: u32, p: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: `p` came from `fake_heap_alloc`.
    unsafe { libc::realloc(p, size) }
}
extern "win64" fn fake_heap_free(_: usize, _: u32, p: *mut c_void) -> i32 {
    // SAFETY: `p` came from `fake_heap_alloc`.
    unsafe { libc::free(p) };
    1
}
extern "win64" fn fake_memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    // SAFETY: the caller's buffers; memmove is a correct memcpy.
    unsafe { libc::memmove(d, s, n) }
}
extern "win64" fn fake_memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void {
    // SAFETY: the caller's buffer.
    unsafe { libc::memset(d, c, n) }
}
extern "win64" fn fake_memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    // SAFETY: the caller's buffers.
    unsafe { libc::memcmp(a, b, n) }
}
extern "win64" fn fake_get_last_error() -> u32 {
    teb::last_error()
}
extern "win64" fn fake_set_last_error(code: u32) {
    teb::set_last_error(code)
}

struct TestResolver;

impl ImportResolver for TestResolver {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let ImportSymbol::Name { name, .. } = symbol else {
            return Resolution::Missing;
        };
        let address = match (dll.to_ascii_lowercase().as_str(), name.as_str()) {
            ("xgameruntime.dll", "XGameRuntimeInitialize") => {
                fake_xgameruntime_initialize as *const () as usize
            }
            ("kernel32.dll", "GetTickCount") => fake_get_tick_count as *const () as usize,
            ("kernel32.dll", "GetCurrentProcessId") => {
                fake_get_current_process_id as *const () as usize
            }
            ("victory-probe-delayed.dll", "VIctoryProbeDelayed") => {
                fake_delayed as *const () as usize
            }
            ("kernel32.dll", "GetProcessHeap") => fake_get_process_heap as *const () as usize,
            ("kernel32.dll", "HeapAlloc") => fake_heap_alloc as *const () as usize,
            ("kernel32.dll", "HeapReAlloc") => fake_heap_realloc as *const () as usize,
            ("kernel32.dll", "HeapFree") => fake_heap_free as *const () as usize,
            ("kernel32.dll", "GetLastError") => fake_get_last_error as *const () as usize,
            ("kernel32.dll", "SetLastError") => fake_set_last_error as *const () as usize,
            ("vcruntime140.dll", "memcpy") => fake_memcpy as *const () as usize,
            ("vcruntime140.dll", "memset") => fake_memset as *const () as usize,
            ("vcruntime140.dll", "memcmp") => fake_memcmp as *const () as usize,
            _ => return Resolution::Missing,
        };
        Resolution::Found {
            address,
            provenance: Provenance::Hle,
        }
    }
}

/// Runs `f` on a fresh guest thread and returns its result.
fn on_guest_thread(f: impl FnOnce() -> u64 + Send + 'static) -> u64 {
    type Job = Box<dyn FnOnce() -> u64 + Send>;
    unsafe extern "win64" fn start(job: *mut c_void) -> u32 {
        // SAFETY: the box below, given to this thread only.
        let (job, out) = *unsafe { Box::from_raw(job as *mut (Job, std::sync::mpsc::Sender<u64>)) };
        out.send(job()).unwrap();
        0
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let job: Box<(Job, _)> = Box::new((Box::new(f), tx));
    thread::spawn(start, Box::into_raw(job).cast(), 0, Box::new(|_| {})).unwrap();
    rx.recv().unwrap()
}

#[test]
fn maps_binds_and_calls_probe_on_guest_threads() {
    let (Some(probe), Some(title)) = (
        build_fixture("hle-probe", "hle_probe.dll"),
        build_fixture("probe-title", "probe_title.dll"),
    ) else {
        eprintln!("skipped: no lld-link, xwin SDK or x86_64-pc-windows-msvc target");
        return;
    };
    let title_dir = tempfile::tempdir().unwrap();
    // Another case than the import's, as a title's folder may have.
    std::fs::copy(&title, title_dir.path().join("Victory-Probe-Title.DLL")).unwrap();

    let image = victory_loader::load_file(&probe).unwrap();
    assert_eq!(image.base() % 0x1_0000, 0);
    // VirtualQuery sees the headers read-only and code as execute-read.
    let protect = |rva: u32| victory_loader::linux::vm::query(image.base() + rva as usize).unwrap().protect;
    assert_eq!(protect(0), 0x02, "PAGE_READONLY");
    let text = image.info().sections.iter().find(|s| s.name == ".text").unwrap();
    assert_eq!(protect(text.rva), 0x20, "PAGE_EXECUTE_READ");
    assert!(
        image.tls_index().is_some_and(|i| i >= 1),
        "a DLL never takes index 0"
    );
    let dlls =
        victory_loader::load_title_dlls(image.info(), title_dir.path(), title_dir.path(), &|_| {
            true
        })
        .unwrap();
    let names: Vec<_> = dlls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["victory-probe-title.dll"]);
    let resolver = WithTitleDlls {
        inner: &TestResolver,
        dlls: &dlls,
    };
    let report = image
        .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
        .unwrap();
    assert_eq!((report.hle, report.title), (13, 1));
    assert!(
        report
            .missing
            .iter()
            .any(|m| m.symbol == "VIctoryProbeNotImplemented")
    );

    let image = Arc::new(image);
    let export = |name: &str| image.export(name).unwrap_or_else(|| panic!("{name}"));
    // The export directory read from memory agrees with the parsed one.
    // SAFETY: a mapped image.
    let at = |name: &str| unsafe { victory_loader::linux::image::export_at(image.base(), name) };
    assert_eq!(at("probe_tick"), Some(export("probe_tick")));
    assert_eq!(at("probe_tic"), None);
    let (xgr, tick, pid, title_fn, delayed, missing, starts) = (
        export("probe_xgameruntime_init"),
        export("probe_tick"),
        export("probe_pid"),
        export("probe_title"),
        export("probe_delayed"),
        export("probe_missing"),
        export("probe_thread_starts"),
    );
    let call = |f: usize| -> u64 {
        // SAFETY: probe exports take no arguments and return at most 64 bits.
        let f: extern "win64" fn() -> u64 = unsafe { std::mem::transmute(f) };
        f()
    };

    // Title code that touches its static TLS needs a TEB: on a guest thread.
    let main = image.clone();
    let dlls_main = dlls.clone();
    let results = on_guest_thread(move || {
        let _ = &main;
        for (_, dll) in &dlls_main {
            dll.bind_imports(&TestResolver, UnimplementedPolicy::ReturnZero)
                .unwrap();
            // SAFETY: bound (it imports nothing), on a guest thread.
            unsafe { dll.attach() }.unwrap();
        }
        assert_eq!(
            call(xgr) as i32,
            0x1234,
            "console API must reach our implementation"
        );
        assert_eq!(call(tick) as u32, 777, "imports must be overridable");
        assert_eq!(call(pid) as u32, std::process::id());
        assert_eq!(
            call(title_fn) as u32,
            0x5EED_0001,
            "title DLL export, attached once"
        );
        assert_eq!(call(delayed) as u32, 4242, "delay-loaded API must be bound");
        let before = unimplemented_call_count();
        assert_eq!(call(missing), 0);
        assert_eq!(unimplemented_call_count(), before + 1);
        // The TLS block is the image's template, at gs:[0x58][_tls_index].
        let teb = teb::current();
        assert!(!teb.is_null());
        // SAFETY: this thread's TEB.
        let block = unsafe { (*teb).tls_blocks[main.tls_index().unwrap() as usize] };
        assert!(!block.is_null());
        // SAFETY: imports bound; runs the probe's CRT entry point.
        unsafe { main.attach() }.unwrap();
        call(starts)
    });

    // Once attached, a guest thread's start reaches the TLS callback and,
    // through the CRT's entry point, DllMain.
    let after = on_guest_thread(move || call(starts));
    assert!(
        after >> 32 > results >> 32,
        "TLS callback: {results:#x} -> {after:#x}"
    );
    assert!(
        after as u32 > results as u32,
        "DllMain: {results:#x} -> {after:#x}"
    );
    assert_eq!(
        victory_loader::disable_thread_calls(image.base()),
        Some(false)
    );
    assert_eq!(victory_loader::disable_thread_calls(1), None);
}
