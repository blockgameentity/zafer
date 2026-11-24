//! Each thread that runs title code has a TEB at its GS base, as on
//! Windows (`gs:[0x30]` self, `gs:[0x58]` static TLS, `gs:[0x60]` PEB,
//! `gs:[0x68]` last error). glibc keeps its thread pointer in FS and leaves
//! GS alone.
//!
//! A new Linux thread inherits its creator's GS base: a host thread that
//! calls title code without [`super::thread::attach_current`] shares its
//! creator's TEB instead of faulting. Host code finds the calling thread's
//! TEB with [`current`], never with a `gs:` read.

use std::cell::Cell;
use std::sync::{LazyLock, OnceLock};

/// `TlsAlloc` indices in the TEB itself (`TlsSlots`).
pub const TLS_SLOTS: usize = 64;
/// `TlsAlloc` indices past those (`TlsExpansionSlots`), as on Windows.
pub const TLS_EXPANSION_SLOTS: usize = 1024;
/// `FlsAlloc` indices kept per thread ([`Teb::fls`]).
pub const FLS_SLOTS: usize = 256;
/// Images with static TLS (`__declspec(thread)`): the exe is 0, DLLs 1...
pub const TLS_MODULES: usize = 64;

/// The x64 TEB fields title code and the shipped CRT read, at their Windows
/// offsets, then our own per-thread state past the real structure's end
/// (`0x1838`). Zeroed when a thread attaches.
#[repr(C)]
pub struct Teb {
    /// `NT_TIB.ExceptionList` (unused on x64).
    pub exception_list: usize,
    /// One past the highest stack address.
    pub stack_base: usize,
    /// The lowest committed stack address; a guard page lies below.
    pub stack_limit: usize,
    pub sub_system_tib: usize,
    pub fiber_data: usize,
    pub arbitrary_user_pointer: usize,
    /// `0x30`: this TEB.
    pub self_: *mut Teb,
    pub environment_pointer: usize,
    /// `0x40`: `ClientId.UniqueProcess`.
    pub process_id: usize,
    /// `0x48`: `ClientId.UniqueThread`.
    pub thread_id: usize,
    pub active_rpc_handle: usize,
    /// `0x58`: the static TLS array, one block per image, by TLS index.
    pub thread_local_storage_pointer: *mut *mut u8,
    /// `0x60`.
    pub peb: *mut Peb,
    /// `0x68`: `GetLastError`.
    pub last_error: u32,
    _reserved_6c: [u8; 0x1478 - 0x6C],
    /// `0x1478`: the bottom of the stack's whole reservation (its guard page).
    pub deallocation_stack: usize,
    /// `0x1480`: `TlsGetValue` indices `0..64`.
    pub tls_slots: [usize; TLS_SLOTS],
    _reserved_1680: [u8; 0x1780 - 0x1680],
    /// `0x1780`: `TlsGetValue` indices `64..`, [`Teb::tls_expansion`].
    pub tls_expansion_slots: *mut usize,
    _reserved_1788: [u8; 0x17C8 - 0x1788],
    /// `0x17C8`: points at [`Teb::fls`].
    pub fls_data: *mut usize,
    _reserved_17d0: [u8; 0x1838 - 0x17D0],

    // Ours; title code never reads these.
    /// Static TLS blocks, by TLS index (null: no block).
    pub tls_blocks: [*mut u8; TLS_MODULES],
    pub tls_expansion: [usize; TLS_EXPANSION_SLOTS],
    /// `FlsGetValue` values, by index.
    pub fls: [usize; FLS_SLOTS],
    /// Where `ExitThread` resumes (the saved stack pointer of the thread's
    /// guest frame); 0 on a host thread attached with `attach_current`.
    pub(crate) exit_frame: usize,
}

/// The PEB fields title code and the shipped CRT read, at their Windows
/// offsets (the structure is `0x7C8` bytes).
#[repr(C)]
pub struct Peb {
    pub inherited_address_space: u8,
    pub read_image_file_exec_options: u8,
    /// `0x02`.
    pub being_debugged: u8,
    pub bit_field: u8,
    _pad: u32,
    pub mutant: usize,
    /// `0x10`: the title's exe.
    pub image_base_address: usize,
    /// `0x18`.
    pub ldr: *mut PebLdrData,
    /// `0x20`.
    pub process_parameters: *mut ProcessParameters,
    pub sub_system_data: usize,
    /// `0x30`: `GetProcessHeap`.
    pub process_heap: usize,
    _reserved_38: [u8; 0xB8 - 0x38],
    /// `0xB8`.
    pub number_of_processors: u32,
    pub nt_global_flag: u32,
    _reserved_c0: [u8; 0x118 - 0xC0],
    /// `0x118`.
    pub os_major_version: u32,
    pub os_minor_version: u32,
    pub os_build_number: u16,
    pub os_csd_version: u16,
    pub os_platform_id: u32,
    _reserved_128: [u8; 0x7C8 - 0x128],
}

/// `LIST_ENTRY`.
#[repr(C)]
pub struct ListEntry {
    pub flink: *mut ListEntry,
    pub blink: *mut ListEntry,
}

/// `PEB_LDR_DATA`, with empty module lists.
#[repr(C)]
pub struct PebLdrData {
    pub length: u32,
    pub initialized: u8,
    pub ss_handle: usize,
    pub in_load_order: ListEntry,
    pub in_memory_order: ListEntry,
    pub in_initialization_order: ListEntry,
    pub entry_in_progress: usize,
    pub shutdown_in_progress: u8,
    pub shutdown_thread_id: usize,
}

/// `UNICODE_STRING`: lengths in bytes, without the terminator.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

/// `RTL_USER_PROCESS_PARAMETERS`, up to `Environment`.
#[repr(C)]
pub struct ProcessParameters {
    pub maximum_length: u32,
    pub length: u32,
    pub flags: u32,
    pub debug_flags: u32,
    pub console_handle: usize,
    pub console_flags: u32,
    pub standard_input: usize,
    pub standard_output: usize,
    pub standard_error: usize,
    /// `0x38`: `CURDIR.DosPath`, then its handle.
    pub current_directory: UnicodeString,
    pub current_directory_handle: usize,
    pub dll_path: UnicodeString,
    /// `0x60`.
    pub image_path_name: UnicodeString,
    /// `0x70`.
    pub command_line: UnicodeString,
    /// `0x80`: a block of `NAME=value\0` strings ending in an empty one.
    pub environment: *mut u16,
    _reserved_88: [u8; 0x440 - 0x88],
}

thread_local! {
    static CURRENT: Cell<*mut Teb> = const { Cell::new(std::ptr::null_mut()) };
    /// Last error of a thread without a TEB.
    static LAST_ERROR: Cell<u32> = const { Cell::new(0) };
}

/// The calling thread's TEB; null if it has none.
pub fn current() -> *mut Teb {
    CURRENT.get()
}

pub(crate) fn set_current(teb: *mut Teb) {
    CURRENT.set(teb);
    set_gs_base(teb as usize);
}

/// `GetLastError` of the calling thread.
pub fn last_error() -> u32 {
    let teb = current();
    if teb.is_null() {
        return LAST_ERROR.get();
    }
    // SAFETY: the calling thread's own live TEB.
    unsafe { (*teb).last_error }
}

/// `SetLastError` of the calling thread.
pub fn set_last_error(code: u32) {
    let teb = current();
    if teb.is_null() {
        return LAST_ERROR.set(code);
    }
    // SAFETY: the calling thread's own live TEB.
    unsafe { (*teb).last_error = code };
}

/// `AT_HWCAP2` bit 1: the kernel lets user space run `wrgsbase`.
static WRGSBASE: LazyLock<bool> =
    // SAFETY: getauxval has no preconditions.
    LazyLock::new(|| unsafe { libc::getauxval(libc::AT_HWCAP2) } & 2 != 0);

/// How this process sets the GS base: `"wrgsbase"` or `"arch_prctl"`.
pub fn gs_base_method() -> &'static str {
    if *WRGSBASE { "wrgsbase" } else { "arch_prctl" }
}

fn set_gs_base(value: usize) {
    if *WRGSBASE {
        // SAFETY: the kernel enabled FSGSBASE (checked above); GS is not used
        // by Rust or glibc on x86-64 Linux.
        unsafe { std::arch::asm!("wrgsbase {}", in(reg) value, options(nostack, preserves_flags)) };
        return;
    }
    const ARCH_SET_GS: libc::c_long = 0x1001;
    // SAFETY: sets this thread's GS base; nothing in the process relies on it.
    if unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_GS, value) } != 0 {
        eprintln!(
            "VIctory: arch_prctl(ARCH_SET_GS) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::abort();
    }
}

/// A boxed `T` with every byte zero (`Box::new_zeroed` needs Rust 1.92).
///
/// # Safety
/// All zero bytes must be a valid `T`.
pub(crate) unsafe fn zeroed<T>() -> Box<T> {
    let layout = std::alloc::Layout::new::<T>();
    // SAFETY: TEB and PEB structures have a non-zero size.
    let p = unsafe { std::alloc::alloc_zeroed(layout) } as *mut T;
    if p.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: a fresh allocation of `T`'s layout, valid zeroed (the caller).
    unsafe { Box::from_raw(p) }
}

/// A process-wide pointer, made once and never freed.
struct Leaked<T>(*mut T);
// SAFETY: the pointee lives for the whole process; access is through raw
// pointers, as title code's is.
unsafe impl<T> Send for Leaked<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for Leaked<T> {}

static PEB: OnceLock<Leaked<Peb>> = OnceLock::new();

/// The process's one PEB.
pub fn peb() -> *mut Peb {
    PEB.get_or_init(|| {
        // SAFETY: every field of these structures is valid zeroed.
        let peb: &mut Peb = Box::leak(unsafe { zeroed() });
        // SAFETY: as above.
        let ldr: &mut PebLdrData = Box::leak(unsafe { zeroed() });
        ldr.length = size_of::<PebLdrData>() as u32;
        ldr.initialized = 1;
        for list in [
            &mut ldr.in_load_order,
            &mut ldr.in_memory_order,
            &mut ldr.in_initialization_order,
        ] {
            let at: *mut ListEntry = list;
            *list = ListEntry {
                flink: at,
                blink: at,
            };
        }
        peb.ldr = ldr;
        // SAFETY: as above.
        let params: &mut ProcessParameters = Box::leak(unsafe { zeroed() });
        params.maximum_length = size_of::<ProcessParameters>() as u32;
        params.length = params.maximum_length;
        params.flags = 1; // RTL_USER_PROC_PARAMS_NORMALIZED
        params.environment = Box::leak(Box::new([0u16; 2])).as_mut_ptr();
        peb.process_parameters = params;
        peb.number_of_processors =
            std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
        // Windows 10, as the console's OS reports itself.
        (peb.os_major_version, peb.os_minor_version) = (10, 0);
        peb.os_build_number = 19041;
        peb.os_platform_id = 2; // VER_PLATFORM_WIN32_NT
        Leaked(peb)
    })
    .0
}

/// A leaked, NUL-terminated `UNICODE_STRING` of `text`.
fn unicode(text: &str) -> UnicodeString {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let bytes = (wide.len() - 1) * 2;
    UnicodeString {
        length: bytes.min(0xFFFC) as u16,
        maximum_length: (bytes + 2).min(0xFFFE) as u16,
        buffer: wide.leak().as_mut_ptr(),
    }
}

/// Fills in the PEB's process parameters, as Windows code spells them.
/// Call before title code runs; the old strings are leaked.
pub fn set_process_parameters(image_path: &str, command_line: &str, current_directory: &str) {
    // SAFETY: the PEB and its parameters are ours and live forever; no
    // title code runs yet.
    let params = unsafe { &mut *(*peb()).process_parameters };
    params.image_path_name = unicode(image_path);
    params.command_line = unicode(command_line);
    params.current_directory = unicode(current_directory);
}

/// Sets `PEB.ImageBaseAddress`, which `GetModuleHandle(NULL)` reads.
pub fn set_image_base(base: usize) {
    // SAFETY: our PEB, a plain field write.
    unsafe { (*peb()).image_base_address = base };
}

/// Sets `PEB.ProcessHeap`, the handle `GetProcessHeap` returns.
pub fn set_process_heap(heap: usize) {
    // SAFETY: our PEB, a plain field write.
    unsafe { (*peb()).process_heap = heap };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn layouts_match_windows() {
        assert_eq!(offset_of!(Teb, stack_base), 0x08);
        assert_eq!(offset_of!(Teb, stack_limit), 0x10);
        assert_eq!(offset_of!(Teb, self_), 0x30);
        assert_eq!(offset_of!(Teb, process_id), 0x40);
        assert_eq!(offset_of!(Teb, thread_id), 0x48);
        assert_eq!(offset_of!(Teb, thread_local_storage_pointer), 0x58);
        assert_eq!(offset_of!(Teb, peb), 0x60);
        assert_eq!(offset_of!(Teb, last_error), 0x68);
        assert_eq!(offset_of!(Teb, deallocation_stack), 0x1478);
        assert_eq!(offset_of!(Teb, tls_slots), 0x1480);
        assert_eq!(offset_of!(Teb, tls_expansion_slots), 0x1780);
        assert_eq!(offset_of!(Teb, fls_data), 0x17C8);
        assert_eq!(offset_of!(Teb, tls_blocks), 0x1838);

        assert_eq!(offset_of!(Peb, being_debugged), 0x02);
        assert_eq!(offset_of!(Peb, image_base_address), 0x10);
        assert_eq!(offset_of!(Peb, ldr), 0x18);
        assert_eq!(offset_of!(Peb, process_parameters), 0x20);
        assert_eq!(offset_of!(Peb, process_heap), 0x30);
        assert_eq!(offset_of!(Peb, number_of_processors), 0xB8);
        assert_eq!(offset_of!(Peb, os_major_version), 0x118);
        assert_eq!(offset_of!(Peb, os_build_number), 0x120);
        assert_eq!(offset_of!(Peb, os_platform_id), 0x124);
        assert_eq!(size_of::<Peb>(), 0x7C8);

        assert_eq!(offset_of!(PebLdrData, in_load_order), 0x10);
        assert_eq!(offset_of!(PebLdrData, in_initialization_order), 0x30);
        assert_eq!(size_of::<UnicodeString>(), 16);
        assert_eq!(offset_of!(ProcessParameters, current_directory), 0x38);
        assert_eq!(offset_of!(ProcessParameters, image_path_name), 0x60);
        assert_eq!(offset_of!(ProcessParameters, command_line), 0x70);
        assert_eq!(offset_of!(ProcessParameters, environment), 0x80);
    }

    #[test]
    fn process_parameters_are_wide_and_terminated() {
        set_process_parameters("G:\\a.exe", "a.exe -x", "G:\\");
        // SAFETY: our PEB, just filled.
        let line = unsafe { (*(*peb()).process_parameters).command_line };
        assert_eq!(line.length, 16);
        // SAFETY: `length / 2` units plus the terminator.
        let text = unsafe { std::slice::from_raw_parts(line.buffer, 9) };
        assert_eq!(String::from_utf16_lossy(&text[..8]), "a.exe -x");
        assert_eq!(text[8], 0);
    }

    #[test]
    fn last_error_without_a_teb_is_per_thread() {
        set_last_error(5);
        assert_eq!(std::thread::spawn(last_error).join().unwrap(), 0);
        assert_eq!(last_error(), 5);
    }
}

thread_local! {
    /// TLS slots of a thread without a TEB.
    static NO_TEB_SLOTS: std::cell::UnsafeCell<[usize; TLS_SLOTS]> =
        const { std::cell::UnsafeCell::new([0; TLS_SLOTS]) };
}

/// The calling thread's `TlsSlots` (TEB +0x1480): the 64 values behind
/// `TlsGetValue`, valid for as long as the thread lives.
pub fn tls_slots() -> *mut usize {
    let teb = current();
    if teb.is_null() {
        return NO_TEB_SLOTS.with(|s| s.get().cast());
    }
    // SAFETY: the calling thread's own live TEB.
    unsafe { (&raw mut (*teb).tls_slots).cast() }
}
