//! Static TLS (`__declspec(thread)`): each image with a TLS directory gets
//! an index, the exe 0 (its code reads `gs:[0x58]` then `[r]` without
//! looking at `_tls_index`), DLLs 1... in load order. Every attached thread
//! has a block for every such image, a copy of the image's template,
//! whether the thread or the image came first.
//!
//! Unlike Windows, nothing needs patching: the exe gets index 0 because we
//! hand out the indices.

use std::alloc::Layout;
use std::sync::Mutex;

use anyhow::{Result, bail};

use super::teb::{TLS_MODULES, Teb};

#[derive(Clone, Copy)]
struct Module {
    /// The template in the mapped image: `raw` bytes, then `zero_fill` zeros.
    template: usize,
    raw: usize,
    layout: Layout,
}

struct State {
    modules: [Option<Module>; TLS_MODULES],
    /// Every attached thread's TEB.
    threads: Vec<usize>,
}

static STATE: Mutex<State> = Mutex::new(State {
    modules: [None; TLS_MODULES],
    threads: Vec::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A fresh block for `m`; null if the image has no TLS data.
fn new_block(m: &Module) -> *mut u8 {
    if m.layout.size() == 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: a non-zero size.
    let block = unsafe { std::alloc::alloc_zeroed(m.layout) };
    if block.is_null() {
        std::alloc::handle_alloc_error(m.layout);
    }
    // SAFETY: the template is `raw` bytes of the mapped image; the block
    // holds at least that many.
    unsafe { std::ptr::copy_nonoverlapping(m.template as *const u8, block, m.raw) };
    block
}

/// The TLS directory of the image at `base` (`directory_rva`), relocated.
/// Takes an index (0 for an exe), writes it to the image's `_tls_index` and
/// gives every attached thread a block. Returns the index.
///
/// # Safety
/// `base` is a mapped, relocated image, its TLS directory at
/// `directory_rva`, no title code of it running yet, and its `_tls_index`
/// still writable.
pub(crate) unsafe fn add(base: usize, directory_rva: u32, is_exe: bool) -> Result<u32> {
    let dir = (base + directory_rva as usize) as *const u64;
    // SAFETY: the directory is inside the image (the caller); the file
    // chooses its alignment, hence unaligned reads.
    let (start, end, index_at, zero_fill, characteristics) = unsafe {
        (
            dir.read_unaligned(),
            dir.add(1).read_unaligned(),
            dir.add(2).read_unaligned(),
            (dir.add(4) as *const u32).read_unaligned(),
            (dir.add(4) as *const u32).add(1).read_unaligned(),
        )
    };
    if end < start {
        bail!("TLS template ends before it starts");
    }
    let raw = (end - start) as usize;
    // Bits 20-23 are IMAGE_SCN_ALIGN_<n>BYTES: 1 is 1 byte, 5 is 16, ...
    let align = match (characteristics >> 20) & 0xF {
        0 => 16,
        n => (1usize << (n - 1)).max(16),
    };
    let layout = Layout::from_size_align(raw + zero_fill as usize, align)?;
    let module = Module {
        template: start as usize,
        raw,
        layout,
    };
    let mut state = state();
    let index = if is_exe && state.modules[0].is_none() {
        Some(0)
    } else {
        (1..TLS_MODULES).find(|&i| state.modules[i].is_none())
    };
    let Some(index) = index else {
        bail!("all {TLS_MODULES} static TLS indices are taken");
    };
    if index_at != 0 {
        // SAFETY: `_tls_index` is in a writable section of the image.
        unsafe { (index_at as *mut u32).write_unaligned(index as u32) };
    }
    state.modules[index] = Some(module);
    for &teb in &state.threads {
        // SAFETY: attached TEBs stay live while listed; no code of this
        // image runs yet, so nothing reads the slot.
        unsafe { (*(teb as *mut Teb)).tls_blocks[index] = new_block(&module) };
    }
    Ok(index as u32)
}

/// Frees the image's block in every thread and its index.
pub(crate) fn remove(index: u32) {
    let mut state = state();
    let Some(module) = state.modules.get_mut(index as usize).and_then(Option::take) else {
        return;
    };
    for &teb in &state.threads {
        // SAFETY: listed TEBs are live; the image's code no longer runs.
        let block = unsafe { &mut (*(teb as *mut Teb)).tls_blocks[index as usize] };
        free(&module, std::mem::replace(block, std::ptr::null_mut()));
    }
}

fn free(m: &Module, block: *mut u8) {
    if !block.is_null() {
        // SAFETY: made by `new_block` with this layout.
        unsafe { std::alloc::dealloc(block, m.layout) };
    }
}

/// Lists a thread's TEB and gives it a block for every image.
///
/// # Safety
/// `teb` is live until [`thread_detached`].
pub(crate) unsafe fn thread_attached(teb: *mut Teb) {
    let mut state = state();
    for (i, m) in state.modules.iter().enumerate() {
        if let Some(m) = m {
            // SAFETY: the caller's live TEB, not yet listed.
            unsafe { (*teb).tls_blocks[i] = new_block(m) };
        }
    }
    state.threads.push(teb as usize);
}

/// Frees a thread's blocks and unlists it.
///
/// # Safety
/// `teb` was passed to [`thread_attached`].
pub(crate) unsafe fn thread_detached(teb: *mut Teb) {
    let mut state = state();
    state.threads.retain(|&t| t != teb as usize);
    for (i, m) in state.modules.iter().enumerate() {
        if let Some(m) = m {
            // SAFETY: the caller's live TEB.
            free(
                m,
                std::mem::replace(unsafe { &mut (*teb).tls_blocks[i] }, std::ptr::null_mut()),
            );
        }
    }
}

/// Calls `f` with every attached thread's TEB (`TlsFree` clears a slot in
/// all of them), under the lock threads attach and detach with: `f` must
/// not start, attach or end threads, nor map images.
pub fn for_each_teb(mut f: impl FnMut(&mut Teb)) {
    let state = state();
    for &teb in &state.threads {
        // SAFETY: listed TEBs are live while the lock is held.
        f(unsafe { &mut *(teb as *mut Teb) });
    }
}
