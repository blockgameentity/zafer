//! The x64 unwinder: the function-table registry (mapped images, tables
//! added at run time, table callbacks), `RtlLookupFunctionEntry` and
//! `RtlVirtualUnwind`, in the public format ("x64 exception handling" in
//! the Microsoft C++ documentation): `RUNTIME_FUNCTION`, `UNWIND_INFO`
//! versions 1 and 2, `UNWIND_CODE`.

use std::sync::{LazyLock, RwLock};

use super::asm;
use super::context::*;

/// One function table: the code `[start, end)` it covers.
struct Table {
    start: usize,
    end: usize,
    base: usize,
    entries: usize,
    count: usize,
    /// `PGET_RUNTIME_FUNCTION_CALLBACK` and its context, for a callback table.
    callback: usize,
    context: usize,
    /// What it is removed by: the image base, the table pointer, or the
    /// callback table's identifier.
    id: usize,
    image: Option<Box<str>>,
}

static TABLES: LazyLock<RwLock<Vec<Table>>> = LazyLock::new(|| {
    // The dispatcher's own frame call (asm.rs) is found like any other function.
    // SAFETY: the entry in asm.rs.
    let end = unsafe { (*asm::frame_entry()).end } as usize;
    RwLock::new(vec![Table {
        start: asm::base(),
        end: asm::base() + end,
        base: asm::base(),
        entries: asm::frame_entry() as usize,
        count: 1,
        callback: 0,
        context: 0,
        id: asm::frame_entry() as usize,
        image: None,
    }])
});

fn tables_mut() -> std::sync::RwLockWriteGuard<'static, Vec<Table>> {
    TABLES.write().unwrap_or_else(|e| e.into_inner())
}

/// Registers the image mapped at `base` (`size` bytes): its exception
/// directory, and its name for reports (its export directory's, until
/// [`name_image`] gives one).
pub(crate) fn add_image(base: usize, size: usize) {
    // SAFETY: the caller mapped a PE image here, headers included.
    let (entries, count, name) = unsafe { read_image(base, size) };
    let mut tables = tables_mut();
    tables.retain(|t| !(t.image.is_some() && t.id == base));
    tables.push(Table {
        start: base,
        end: base + size,
        base,
        entries,
        count,
        callback: 0,
        context: 0,
        id: base,
        image: Some(name.into()),
    });
}

pub(crate) fn remove_image(base: usize) {
    tables_mut().retain(|t| !(t.image.is_some() && t.id == base));
}

pub(crate) fn name_image(base: usize, name: &str) {
    for t in tables_mut().iter_mut() {
        if t.image.is_some() && t.id == base {
            t.image = Some(name.into());
        }
    }
}

/// The exception directory and the export name of the image at `base`.
///
/// # Safety
/// A PE image is mapped at `base`, `size` bytes.
unsafe fn read_image(base: usize, size: usize) -> (usize, usize, String) {
    // SAFETY: each read is bounds-checked against `size` first.
    let read = |at: usize| -> u32 {
        if at + 4 > size {
            return 0;
        }
        // SAFETY: inside the mapped image (checked above).
        unsafe { ((base + at) as *const u32).read_unaligned() }
    };
    let nt = read(0x3c) as usize;
    if read(0) & 0xffff != 0x5a4d || read(nt) != 0x4550 {
        return (0, 0, String::new());
    }
    // IMAGE_OPTIONAL_HEADER64's data directories start at +112.
    let directory = |index: usize| {
        let at = nt + 24 + 112 + 8 * index;
        (read(at) as usize, read(at + 4) as usize)
    };
    let (rva, len) = directory(3);
    let (entries, count) = if rva != 0 && rva + len <= size {
        (base + rva, len / 12)
    } else {
        (0, 0)
    };
    let (exports, _) = directory(0);
    let name_rva = if exports != 0 { read(exports + 12) as usize } else { 0 };
    let name = if name_rva != 0 && name_rva < size {
        let bytes = (base + name_rva) as *const u8;
        let mut len = 0;
        // SAFETY: inside the image, NUL-terminated or cut at its end.
        while name_rva + len < size && unsafe { *bytes.add(len) } != 0 && len < 260 {
            len += 1;
        }
        // SAFETY: the `len` bytes read above.
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(bytes, len) }).into_owned()
    } else {
        format!("image@{base:#x}")
    };
    (entries, count, name)
}

/// `RtlAddFunctionTable`: `count` entries, RVAs from `base`.
pub(crate) fn add_table(entries: usize, count: usize, base: usize) -> bool {
    if entries == 0 || count == 0 {
        return false;
    }
    // SAFETY: the caller's table of `count` entries.
    let list = unsafe { std::slice::from_raw_parts(entries as *const RuntimeFunction, count) };
    let start = list.iter().map(|f| f.begin).min().unwrap_or(0) as usize;
    let end = list.iter().map(|f| f.end).max().unwrap_or(0) as usize;
    tables_mut().push(Table {
        start: base + start,
        end: base + end,
        base,
        entries,
        count,
        callback: 0,
        context: 0,
        id: entries,
        image: None,
    });
    true
}

/// `RtlInstallFunctionTableCallback`.
pub(crate) fn add_callback(id: usize, base: usize, len: usize, callback: usize, context: usize) -> bool {
    if id & 3 != 3 || callback == 0 {
        return false;
    }
    tables_mut().push(Table {
        start: base,
        end: base + len,
        base,
        entries: 0,
        count: 0,
        callback,
        context,
        id,
        image: None,
    });
    true
}

/// `RtlDeleteFunctionTable`, for both kinds above.
pub(crate) fn remove_table(id: usize) -> bool {
    let mut tables = tables_mut();
    let before = tables.len();
    tables.retain(|t| t.image.is_some() || t.id != id);
    tables.len() != before
}

/// Every registered image's `[start, end)`.
#[cfg(feature = "diagnostics")]
pub(crate) fn image_ranges() -> Vec<(usize, usize)> {
    let tables = TABLES.read().unwrap_or_else(|e| e.into_inner());
    tables.iter().filter(|t| t.image.is_some()).map(|t| (t.start, t.end)).collect()
}

/// The registered image `pc` is in: its base, and its name.
pub(crate) fn image_of(pc: usize) -> Option<(usize, String)> {
    let tables = TABLES.try_read().ok()?;
    let t = tables.iter().find(|t| t.image.is_some() && (t.start..t.end).contains(&pc))?;
    Some((t.base, t.image.as_deref().unwrap_or("").to_owned()))
}

/// What [`lookup`] finds for a pc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// A function entry and its image base.
    Function(*mut RuntimeFunction, usize),
    /// Our own code: the walk ends here.
    Boundary,
    /// No entry: a leaf function (its return address at rsp), or a call
    /// through a bad pointer. The base of the image it is in, or 0.
    Leaf(usize),
}

/// `RtlLookupFunctionEntry`.
pub(crate) fn lookup(pc: u64) -> Entry {
    let pc = pc as usize;
    let found = {
        let tables = TABLES.read().unwrap_or_else(|e| e.into_inner());
        tables
            .iter()
            .find(|t| (t.start..t.end).contains(&pc))
            .map(|t| (t.base, t.entries, t.count, t.callback, t.context))
    };
    let Some((base, entries, count, callback, context)) = found else {
        return if is_host_code(pc) { Entry::Boundary } else { Entry::Leaf(0) };
    };
    if callback != 0 {
        type Callback = unsafe extern "win64" fn(u64, usize) -> *mut RuntimeFunction;
        // SAFETY: the title registered this callback for this range.
        let f = unsafe { std::mem::transmute::<usize, Callback>(callback)(pc as u64, context) };
        return if f.is_null() { Entry::Leaf(base) } else { Entry::Function(f, base) };
    }
    // SAFETY: a registered table of `count` entries, sorted by begin.
    let list = unsafe { std::slice::from_raw_parts(entries as *mut RuntimeFunction, count) };
    let rva = (pc - base) as u32;
    let i = list.partition_point(|f| f.begin <= rva);
    if i == 0 || rva >= list[i - 1].end {
        return Entry::Leaf(base);
    }
    let mut f = &list[i - 1] as *const RuntimeFunction as *mut RuntimeFunction;
    // SAFETY: an entry of the table; the indirect form points at the one with the data.
    let data = unsafe { (*f).unwind_data };
    if data & 1 != 0 {
        f = (base + (data & !1) as usize) as *mut RuntimeFunction;
    }
    Entry::Function(f, base)
}

/// Whether `pc` is in an executable segment of our program, one of its
/// shared libraries or the vDSO.
pub(crate) fn is_host_code(pc: usize) -> bool {
    struct Query {
        pc: usize,
        found: bool,
    }
    unsafe extern "C" fn each(info: *mut libc::dl_phdr_info, _: usize, data: *mut libc::c_void) -> i32 {
        // SAFETY: dl_iterate_phdr passes a valid object and our query.
        let (info, q) = unsafe { (&*info, &mut *(data as *mut Query)) };
        for i in 0..usize::from(info.dlpi_phnum) {
            // SAFETY: dlpi_phnum headers.
            let p = unsafe { &*info.dlpi_phdr.add(i) };
            let start = info.dlpi_addr as usize + p.p_vaddr as usize;
            if p.p_type == libc::PT_LOAD
                && p.p_flags & libc::PF_X != 0
                && q.pc.wrapping_sub(start) < p.p_memsz as usize
            {
                q.found = true;
            }
        }
        i32::from(q.found)
    }
    let mut q = Query { pc, found: false };
    // SAFETY: the callback only reads what it is given.
    unsafe { libc::dl_iterate_phdr(Some(each), &mut q as *mut Query as *mut libc::c_void) };
    q.found
}

const UWOP_PUSH_NONVOL: u8 = 0;
const UWOP_ALLOC_LARGE: u8 = 1;
const UWOP_ALLOC_SMALL: u8 = 2;
const UWOP_SET_FPREG: u8 = 3;
const UWOP_SAVE_NONVOL: u8 = 4;
const UWOP_SAVE_NONVOL_FAR: u8 = 5;
/// Version 2's epilogue code; in version 1 the retired `UWOP_SAVE_XMM` (low half).
const UWOP_EPILOG: u8 = 6;
/// Version 2's spare code; in version 1 the retired `UWOP_SAVE_XMM_FAR`.
const UWOP_SPARE_CODE: u8 = 7;
const UWOP_SAVE_XMM128: u8 = 8;
const UWOP_SAVE_XMM128_FAR: u8 = 9;
const UWOP_PUSH_MACHFRAME: u8 = 10;

/// An `UNWIND_INFO` header and its codes.
struct Info {
    at: usize,
    version: u8,
    flags: u32,
    prolog: u32,
    count: usize,
    frame_reg: usize,
    frame_offset: u64,
}

impl Info {
    /// # Safety
    /// `at` is an `UNWIND_INFO` in a registered image.
    unsafe fn read(at: usize) -> Info {
        // SAFETY: as the caller says.
        let b = unsafe { std::slice::from_raw_parts(at as *const u8, 4) };
        Info {
            at,
            version: b[0] & 7,
            flags: u32::from(b[0] >> 3),
            prolog: u32::from(b[1]),
            count: usize::from(b[2]),
            frame_reg: usize::from(b[3] & 15),
            frame_offset: u64::from(b[3] >> 4) * 16,
        }
    }

    fn of(base: usize, f: *const RuntimeFunction) -> Info {
        // SAFETY: a function entry of a registered table, and its data.
        unsafe { Info::read(base + ((*f).unwind_data & !1) as usize) }
    }

    /// Code slot `i`: (prologue offset, op, op info).
    fn code(&self, i: usize) -> (u32, u8, u8) {
        // SAFETY: slot i < count of this UNWIND_INFO.
        let [offset, op] = unsafe { ((self.at + 4 + 2 * i) as *const [u8; 2]).read() };
        (u32::from(offset), op & 15, op >> 4)
    }

    /// The 16-bit and 32-bit operands after slot `i`.
    fn operand16(&self, i: usize) -> u64 {
        // SAFETY: the slots after a code that has operands.
        u64::from(unsafe { ((self.at + 4 + 2 * i + 2) as *const u16).read_unaligned() })
    }

    fn operand32(&self, i: usize) -> u64 {
        // SAFETY: as above.
        u64::from(unsafe { ((self.at + 4 + 2 * i + 2) as *const u32).read_unaligned() })
    }

    /// Slots code `i` takes.
    fn slots(&self, i: usize) -> usize {
        let (_, op, info) = self.code(i);
        match op {
            UWOP_ALLOC_LARGE if info != 0 => 3,
            UWOP_ALLOC_LARGE | UWOP_SAVE_NONVOL | UWOP_SAVE_XMM128 => 2,
            UWOP_SAVE_NONVOL_FAR | UWOP_SAVE_XMM128_FAR | UWOP_SPARE_CODE => 3,
            UWOP_EPILOG if self.version != 2 => 2,
            _ => 1,
        }
    }

    /// What follows the (even-padded) codes: the handler RVA and its data,
    /// or with `UNW_FLAG_CHAININFO` the parent's entry.
    fn tail(&self) -> usize {
        self.at + 4 + 2 * ((self.count + 1) & !1)
    }
}

/// The entry at the end of the chain `f` hangs from.
fn primary(base: usize, mut f: *const RuntimeFunction) -> *const RuntimeFunction {
    for _ in 0..32 {
        let info = Info::of(base, f);
        if info.flags & UNW_FLAG_CHAININFO == 0 {
            break;
        }
        f = info.tail() as *const RuntimeFunction;
    }
    f
}

fn byte(at: u64) -> u8 {
    // SAFETY: code bytes of a function in a registered image, at or just past pc.
    unsafe { (at as *const u8).read() }
}

fn rel32(at: u64) -> i64 {
    // SAFETY: as above.
    i64::from(unsafe { (at as *const i32).read_unaligned() })
}

/// Version 1 has no epilogue data: the unwinder recognises the only forms an
/// epilogue may take (`add rsp, imm` / `lea rsp, [fp + disp]`; `pop reg`...;
/// `ret` or a jump out of the function) from the bytes at pc.
fn in_epilog_v1(base: usize, pc: u64, f: *const RuntimeFunction, info: &Info) -> bool {
    let mut p = pc;
    let (b0, b1, b2) = (byte(p), byte(p + 1), byte(p + 2));
    if b0 == 0x48 && b1 == 0x83 && b2 == 0xC4 {
        p += 4;
    } else if b0 == 0x48 && b1 == 0x81 && b2 == 0xC4 {
        p += 7;
    } else if b0 & 0xFE == 0x48 && b1 == 0x8D {
        let reg = (usize::from(b0 & 1) << 3) | usize::from(b2 & 7);
        if reg != 0 && reg == info.frame_reg {
            match b2 & 0xF8 {
                0x60 => p += 4,
                0xA0 => p += 7,
                _ => {}
            }
        }
    }
    loop {
        if byte(p) & 0xF8 == 0x58 {
            p += 1;
        } else if byte(p) & 0xF0 == 0x40 && byte(p + 1) & 0xF8 == 0x58 {
            p += 2;
        } else {
            break;
        }
    }
    if byte(p) == 0xF2 {
        p += 1; // bnd
    }
    match (byte(p), byte(p + 1)) {
        (0xC3 | 0xC2, _) | (0xF3, 0xC3) => true,
        (0xEB | 0xE9, _) => {
            let short = byte(p) == 0xEB;
            let next = p + if short { 2 } else { 5 };
            let disp = if short { i64::from(byte(p + 1) as i8) } else { rel32(p + 1) };
            let target = (next as i64 + disp) as u64 - base as u64;
            // SAFETY: the entry the caller looked up.
            let (begin, end) = unsafe { (u64::from((*f).begin), u64::from((*f).end)) };
            if target < begin || target >= end {
                // A jump to another function (or to the start of this one's primary: a
                // recursive tail call) ends an epilogue; to a fragment of this one it does not.
                match lookup(base as u64 + target) {
                    Entry::Function(other, other_base) if other_base == base => {
                        let p_other = primary(base, other);
                        // SAFETY: entries of a registered table.
                        p_other != primary(base, f) || target == u64::from(unsafe { (*p_other).begin })
                    }
                    _ => true,
                }
            } else {
                target == begin && info.flags & UNW_FLAG_CHAININFO == 0
            }
        }
        (0xFF, 0x25) => true,
        (rex, 0xFF) if rex & 0xF8 == 0x48 && byte(p + 2) & 0x38 == 0x20 => true,
        _ => false,
    }
}

/// Version 2 lists its epilogues: the first `UWOP_EPILOG` holds their size
/// and whether one ends the function, each further one an offset back from
/// the function's end.
fn in_epilog_v2(base: usize, pc: u64, f: *const RuntimeFunction, info: &Info) -> bool {
    if info.count == 0 {
        return false;
    }
    let (size, op, flags) = info.code(0);
    if op != UWOP_EPILOG {
        return false;
    }
    // SAFETY: the entry the caller looked up.
    let end = unsafe { (*f).end };
    let rva = (pc - base as u64) as u32;
    let inside = |back: u32| back != 0 && back <= end && rva >= end - back && rva - (end - back) < size;
    if flags & 1 != 0 && inside(size) {
        return true;
    }
    (1..info.count)
        .take_while(|&i| info.code(i).1 == UWOP_EPILOG)
        .any(|i| {
            let (low, _, high) = info.code(i);
            inside(low | u32::from(high) << 8)
        })
}

/// A read of the stack that failed: the frame is not there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BadFrame;

fn set_register(
    c: &mut Context,
    pointers: &mut Option<&mut NonvolatilePointers>,
    reg: usize,
    at: u64,
) -> Result<(), BadFrame> {
    c.gpr[reg] = asm::read64(at).ok_or(BadFrame)?;
    if let Some(p) = pointers {
        p.integer[reg] = at as *mut u64;
    }
    Ok(())
}

fn set_xmm(
    c: &mut Context,
    pointers: &mut Option<&mut NonvolatilePointers>,
    reg: usize,
    at: u64,
    full: bool,
) -> Result<(), BadFrame> {
    let low = asm::read64(at).ok_or(BadFrame)?;
    let high = if full { asm::read64(at + 8).ok_or(BadFrame)? } else { 0 };
    c.xmm()[reg] = M128A { low, high };
    if let Some(p) = pointers {
        p.floating[reg] = at as *mut M128A;
    }
    Ok(())
}

/// The rest of an epilogue from pc: the stack adjustment, the pops and the
/// return (a tail jump is unwound as the return it stands for).
fn unwind_epilog(
    pc: u64,
    c: &mut Context,
    pointers: &mut Option<&mut NonvolatilePointers>,
) -> Result<(), BadFrame> {
    let mut p = pc;
    if byte(p) & 0xF8 == 0x48 {
        match (byte(p + 1), byte(p + 2)) {
            (0x83, 0xC4) => {
                c.gpr[RSP] = c.gpr[RSP].wrapping_add(byte(p + 3) as i8 as u64);
                p += 4;
            }
            (0x81, 0xC4) => {
                c.gpr[RSP] = c.gpr[RSP].wrapping_add(rel32(p + 3) as u64);
                p += 7;
            }
            (0x8D, modrm) => {
                let fp = c.gpr[(usize::from(byte(p) & 1) << 3) | usize::from(modrm & 7)];
                match modrm & 0xF8 {
                    0x60 => {
                        c.gpr[RSP] = fp.wrapping_add(byte(p + 3) as i8 as u64);
                        p += 4;
                    }
                    0xA0 => {
                        c.gpr[RSP] = fp.wrapping_add(rel32(p + 3) as u64);
                        p += 7;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    loop {
        let reg = if byte(p) & 0xF8 == 0x58 {
            p += 1;
            usize::from(byte(p - 1) & 7)
        } else if byte(p) & 0xF0 == 0x40 && byte(p + 1) & 0xF8 == 0x58 {
            p += 2;
            (usize::from(byte(p - 2) & 1) << 3) | usize::from(byte(p - 1) & 7)
        } else {
            break;
        };
        set_register(c, pointers, reg, c.gpr[RSP])?;
        c.gpr[RSP] += 8;
    }
    c.rip = asm::read64(c.gpr[RSP]).ok_or(BadFrame)?;
    c.gpr[RSP] += 8;
    Ok(())
}

/// What [`virtual_unwind`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Unwound {
    /// The language handler of `handler_type`, or 0.
    pub handler: usize,
    pub handler_data: usize,
    pub establisher_frame: u64,
}

/// `RtlVirtualUnwind`: `c` becomes the caller's context. A read of the stack
/// that would fault ends it with [`BadFrame`] (the context is then partly
/// unwound).
pub(crate) fn virtual_unwind(
    handler_type: u32,
    base: usize,
    pc: u64,
    function: *const RuntimeFunction,
    c: &mut Context,
    mut pointers: Option<&mut NonvolatilePointers>,
) -> Result<Unwound, BadFrame> {
    if function == asm::boundary() {
        c.rip = 0;
        return Ok(Unwound { establisher_frame: c.gpr[RSP], ..Default::default() });
    }
    let mut f = function;
    let mut info = Info::of(base, f);
    // SAFETY: a registered entry.
    let mut offset = (pc - base as u64).wrapping_sub(u64::from(unsafe { (*f).begin })) as u32;
    let in_prolog = offset < info.prolog;

    // The establisher frame is what the body addresses its locals from: the frame register
    // once it is set (which may be before the prologue ends), else rsp.
    let mut frame = c.gpr[RSP];
    if info.frame_reg != 0 {
        let mut established = !in_prolog || info.flags & UNW_FLAG_CHAININFO != 0;
        let mut i = 0;
        while !established && i < info.count {
            let (at, op, _) = info.code(i);
            if op == UWOP_SET_FPREG {
                established = offset >= at;
                break;
            }
            i += info.slots(i);
        }
        if established {
            frame = c.gpr[info.frame_reg].wrapping_sub(info.frame_offset);
        }
    }

    let epilog = !in_prolog
        && match info.version {
            2 => in_epilog_v2(base, pc, f, &info),
            _ => in_epilog_v1(base, pc, f, &info),
        };
    if epilog {
        // The frame register may hold the caller's value again (after `pop rbp`), and no
        // handler runs for a frame being torn down: rsp is always inside this frame.
        let establisher_frame = c.gpr[RSP];
        unwind_epilog(pc, c, &mut pointers)?;
        return Ok(Unwound { establisher_frame, ..Default::default() });
    }

    // Undo the prologue: codes are listed last instruction first, and one whose instruction
    // had not run at pc is skipped. Chained data continues with the parent's.
    let mut machine_frame = false;
    for depth in 0.. {
        if info.version != 1 && info.version != 2 {
            break; // unknown data: a frame without codes
        }
        let mut i = 0;
        while i < info.count {
            let (at, op, reg) = info.code(i);
            let next = i + info.slots(i);
            let reg = usize::from(reg);
            if offset < at {
                i = next;
                continue;
            }
            let rsp = c.gpr[RSP];
            match op {
                UWOP_PUSH_NONVOL => {
                    set_register(c, &mut pointers, reg, rsp)?;
                    c.gpr[RSP] = rsp + 8;
                }
                UWOP_ALLOC_LARGE if reg != 0 => c.gpr[RSP] = rsp.wrapping_add(info.operand32(i)),
                UWOP_ALLOC_LARGE => c.gpr[RSP] = rsp.wrapping_add(info.operand16(i) * 8),
                UWOP_ALLOC_SMALL => c.gpr[RSP] = rsp.wrapping_add(reg as u64 * 8 + 8),
                UWOP_SET_FPREG => c.gpr[RSP] = c.gpr[info.frame_reg].wrapping_sub(info.frame_offset),
                UWOP_SAVE_NONVOL => set_register(c, &mut pointers, reg, frame + info.operand16(i) * 8)?,
                UWOP_SAVE_NONVOL_FAR => set_register(c, &mut pointers, reg, frame + info.operand32(i))?,
                UWOP_EPILOG if info.version == 1 => {
                    set_xmm(c, &mut pointers, reg, frame + info.operand16(i) * 8, false)?
                }
                UWOP_SPARE_CODE if info.version == 1 => {
                    set_xmm(c, &mut pointers, reg, frame + info.operand32(i), false)?
                }
                UWOP_SAVE_XMM128 => set_xmm(c, &mut pointers, reg, frame + info.operand16(i) * 16, true)?,
                UWOP_SAVE_XMM128_FAR => set_xmm(c, &mut pointers, reg, frame + info.operand32(i), true)?,
                UWOP_PUSH_MACHFRAME => {
                    // [error code,] rip, cs, eflags, rsp, ss as a trap pushes them.
                    let top = rsp + if reg != 0 { 8 } else { 0 };
                    c.rip = asm::read64(top).ok_or(BadFrame)?;
                    c.gpr[RSP] = asm::read64(top + 24).ok_or(BadFrame)?;
                    machine_frame = true;
                }
                _ => {}
            }
            i = next;
        }
        if info.flags & UNW_FLAG_CHAININFO == 0 || depth == 32 {
            break;
        }
        f = info.tail() as *const RuntimeFunction;
        info = Info::of(base, f);
        // SAFETY: the chained entry, inside the image.
        offset = (pc - base as u64).wrapping_sub(u64::from(unsafe { (*f).begin })) as u32;
    }
    if !machine_frame {
        c.rip = asm::read64(c.gpr[RSP]).ok_or(BadFrame)?;
        c.gpr[RSP] += 8;
    }
    let mut out = Unwound { establisher_frame: frame, ..Default::default() };
    // The handler belongs to the function (the end of the chain), not to its prologue.
    if offset >= info.prolog && info.flags & handler_type & (UNW_FLAG_EHANDLER | UNW_FLAG_UHANDLER) != 0 {
        let tail = info.tail();
        // SAFETY: the handler RVA after the codes.
        out.handler = base + unsafe { (tail as *const u32).read_unaligned() } as usize;
        out.handler_data = tail + 4;
    }
    Ok(out)
}

/// One step of a walk outwards from `c`: the caller's context. False where
/// the walk ends (our code, a zero return address, a bad stack).
pub(crate) fn step(c: &mut Context) -> bool {
    match lookup(c.rip) {
        Entry::Boundary => false,
        Entry::Leaf(_) => match asm::read64(c.gpr[RSP]) {
            Some(rip) => {
                c.rip = rip;
                c.gpr[RSP] += 8;
                rip != 0
            }
            None => false,
        },
        Entry::Function(f, base) => {
            virtual_unwind(UNW_FLAG_NHANDLER, base, c.rip, f, c, None).is_ok() && c.rip != 0
        }
    }
}

/// Up to `frames.len()` code addresses: `context.rip` first, then each
/// caller's return address, until the title frames end. Never faults.
pub(crate) fn walk(context: &Context, frames: &mut [u64]) -> usize {
    let mut c = *context;
    let mut n = 0;
    while n < frames.len() {
        frames[n] = c.rip;
        n += 1;
        let rsp = c.gpr[RSP];
        if !step(&mut c) || c.gpr[RSP] <= rsp {
            break;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake image: `RUNTIME_FUNCTION`s, `UNWIND_INFO`s and code in one
    /// buffer, and a fake stack.
    struct Image {
        bytes: Box<[u8; 0x2000]>,
    }

    impl Image {
        fn new() -> Image {
            Image { bytes: Box::new([0xCC; 0x2000]) }
        }
        fn base(&self) -> usize {
            self.bytes.as_ptr() as usize
        }
        fn put(&mut self, at: usize, bytes: &[u8]) {
            self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
        }
        /// An entry at `at` for code `[begin, end)` with its info at `info`.
        fn function(&mut self, at: usize, begin: u32, end: u32, info: u32) -> *const RuntimeFunction {
            let mut b = Vec::new();
            for v in [begin, end, info] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            self.put(at, &b);
            (self.base() + at) as *const RuntimeFunction
        }
    }

    fn stack(values: &[u64]) -> Vec<u64> {
        values.to_vec()
    }

    fn context(rsp: u64) -> Context {
        let mut c = Context::zeroed();
        c.gpr[RSP] = rsp;
        c
    }

    const CODE: u32 = 0x1000;

    #[test]
    fn prologue_codes_are_undone_in_order() {
        // push rbx (offset 1); push r12 (3); sub rsp, 0x28 (7): codes last first.
        let mut image = Image::new();
        image.put(0x100, &[1, 7, 3, 0, 7, 0x42, 3, 0xC0, 1, 0x30]);
        let f = image.function(0, CODE, CODE + 0x40, 0x100);
        // Stack: 0x28 of locals, r12, rbx, return address.
        let mut s = stack(&[0; 5]);
        s.extend([0x1212, 0xBBBB, 0xDEAD]);
        let rsp = s.as_ptr() as u64;
        let pc = image.base() as u64 + u64::from(CODE) + 0x10;
        let mut c = context(rsp);
        // SAFETY: null pointers are a valid NonvolatilePointers.
        let mut pointers: NonvolatilePointers = unsafe { std::mem::zeroed() };
        let u = virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, Some(&mut pointers)).unwrap();
        assert_eq!((c.gpr[R12], c.gpr[RBX], c.rip), (0x1212, 0xBBBB, 0xDEAD));
        assert_eq!(c.gpr[RSP], rsp + 8 * 8);
        assert_eq!(u.establisher_frame, rsp);
        assert_eq!(pointers.integer[RBX] as u64, rsp + 6 * 8);

        // Inside the prologue, after `push rbx` only.
        let mut c = context(rsp + 6 * 8);
        let pc = image.base() as u64 + u64::from(CODE) + 1;
        virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!((c.gpr[RBX], c.rip, c.gpr[R12]), (0xBBBB, 0xDEAD, 0));
    }

    #[test]
    fn large_allocations_and_saves_from_the_frame() {
        // sub rsp, 0x1000 (ALLOC_LARGE 16-bit, /8) at 7; mov [rsp+0x18], rsi (SAVE_NONVOL 3) at 12;
        // movaps [rsp+0x20], xmm6 (SAVE_XMM128 2) at 17; then ALLOC_LARGE 32-bit of 8 at 21.
        let mut image = Image::new();
        image.put(
            0x100,
            &[1, 21, 9, 0, 21, 0x11, 8, 0, 0, 0, 17, 0x68, 2, 0, 12, 0x64, 3, 0, 7, 0x01, 0x00, 0x02],
        );
        let f = image.function(0, CODE, CODE + 0x40, 0x100);
        let mut s = vec![0u64; 0x1000 / 8 + 2];
        s[3] = 0x5151; // rsi at frame + 0x18
        s[4] = 0x66; // xmm6 at frame + 0x20
        s[5] = 0x77;
        *s.last_mut().unwrap() = 0xFEED;
        let rsp = s.as_ptr() as u64;
        let mut c = context(rsp);
        let pc = image.base() as u64 + u64::from(CODE) + 0x30;
        virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!(c.gpr[RSI], 0x5151);
        assert_eq!(c.xmm()[6], M128A { low: 0x66, high: 0x77 });
        assert_eq!(c.rip, 0xFEED);
        assert_eq!(c.gpr[RSP], rsp + 0x1010);
    }

    #[test]
    fn frame_register_sets_the_establisher_frame() {
        // push rbp (1); sub rsp, 0x40 (5); lea rbp, [rsp+0x20] (10): SET_FPREG rbp, offset 2*16.
        let mut image = Image::new();
        image.put(0x100, &[1, 10, 3, 0x25, 10, 0x03, 5, 0x72, 1, 0x50]);
        let f = image.function(0, CODE, CODE + 0x40, 0x100);
        let s = stack(&[0, 0, 0, 0, 0, 0, 0, 0, 0x8888, 0xABC]);
        let rsp = s.as_ptr() as u64;
        let mut c = context(0x1234); // rsp is irrelevant once rbp is set
        c.gpr[RBP] = rsp + 0x20;
        let pc = image.base() as u64 + u64::from(CODE) + 0x20;
        let u = virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!(u.establisher_frame, rsp);
        assert_eq!((c.gpr[RBP], c.rip, c.gpr[RSP]), (0x8888, 0xABC, rsp + 0x50));
    }

    #[test]
    fn machine_frames_take_rip_and_rsp_from_the_frame() {
        // PUSH_MACHFRAME with an error code.
        let mut image = Image::new();
        image.put(0x100, &[1, 0, 1, 0, 0, 0x1A]);
        let f = image.function(0, CODE, CODE + 0x40, 0x100);
        let s = stack(&[7, 0x4444, 0x33, 0x202, 0x9999_0000, 0x2b]);
        let mut c = context(s.as_ptr() as u64);
        let pc = image.base() as u64 + u64::from(CODE) + 4;
        virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!((c.rip, c.gpr[RSP]), (0x4444, 0x9999_0000));
    }

    #[test]
    fn chained_info_continues_with_the_parent() {
        // Parent: push rdi (1). Child (a fragment, offset 0 codes): push rsi; chained to parent.
        let mut image = Image::new();
        image.put(0x100, &[1, 1, 1, 0, 1, 0x70]);
        image.function(0, CODE, CODE + 0x10, 0x100);
        image.put(0x120, &[1 | (4 << 3), 0, 1, 0, 0, 0x60, 0, 0]);
        image.function(0x128, CODE, CODE + 0x10, 0x100); // the parent entry, after the codes
        let child = image.function(0x20, CODE + 0x20, CODE + 0x30, 0x120);
        let s = stack(&[0x5151, 0x7171, 0xCAFE]);
        let mut c = context(s.as_ptr() as u64);
        let pc = image.base() as u64 + u64::from(CODE) + 0x24;
        let u = virtual_unwind(UNW_FLAG_EHANDLER, image.base(), pc, child, &mut c, None).unwrap();
        assert_eq!((c.gpr[RSI], c.gpr[RDI], c.rip), (0x5151, 0x7171, 0xCAFE));
        assert_eq!(u.handler, 0);
    }

    #[test]
    fn retired_xmm_codes_load_the_low_half() {
        // Version 1 UWOP_SAVE_XMM (6) xmm7 at frame + 1*8.
        let mut image = Image::new();
        image.put(0x100, &[1, 0, 2, 0, 0, 0x76, 1, 0]);
        let f = image.function(0, CODE, CODE + 0x10, 0x100);
        let s = stack(&[0xAAAA, 0x7777, 0]);
        let mut c = context(s.as_ptr() as u64);
        c.xmm()[7].high = 5;
        // pc on an int3 (0xCC): not an epilogue.
        let pc = image.base() as u64 + u64::from(CODE) + 2;
        virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!(c.xmm()[7], M128A { low: 0x7777, high: 0 });
        assert_eq!(c.rip, 0xAAAA);
    }

    #[test]
    fn version_1_epilogues_are_read_from_the_code() {
        // push rbx; push rbp; sub rsp, 0x18 / add rsp, 0x18; pop rbp; pop rbx; ret
        let mut image = Image::new();
        image.put(0x100, &[1, 6, 3, 0, 6, 0x22, 2, 0x50, 1, 0x30]);
        let f = image.function(0, CODE, CODE + 0x40, 0x100);
        image.put(CODE as usize + 0x20, &[0x48, 0x83, 0xC4, 0x18, 0x5D, 0x5B, 0xC3]);
        let s = stack(&[0, 0, 0, 0xBB, 0xAA, 0xF00D]);
        let rsp = s.as_ptr() as u64;
        for (at, rsp_there) in [(0x20, rsp), (0x24, rsp + 0x18), (0x25, rsp + 0x20), (0x26, rsp + 0x28)] {
            let mut c = context(rsp_there);
            c.gpr[RBP] = 0xBB;
            c.gpr[RBX] = 0xAA;
            let pc = image.base() as u64 + u64::from(CODE) + at;
            let u = virtual_unwind(UNW_FLAG_EHANDLER, image.base(), pc, f, &mut c, None).unwrap();
            assert_eq!((c.rip, c.gpr[RSP], c.gpr[RBP], c.gpr[RBX]), (0xF00D, rsp + 0x30, 0xBB, 0xAA), "{at:#x}");
            assert_eq!(u.establisher_frame, rsp_there);
        }
        // A tail jump out of the function ends an epilogue too.
        image.put(CODE as usize + 0x30, &[0x48, 0x83, 0xC4, 0x18, 0x5D, 0x5B, 0xE9, 0, 1, 0, 0]);
        let mut c = context(rsp);
        let pc = image.base() as u64 + u64::from(CODE) + 0x30;
        virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!(c.rip, 0xF00D);
        // A jump inside the function is not an epilogue: the prologue is undone instead.
        image.put(CODE as usize + 0x30, &[0x48, 0x83, 0xC4, 0x18, 0x5D, 0x5B, 0xEB, 0xF0]);
        let mut c = context(rsp);
        virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!((c.rip, c.gpr[RBP], c.gpr[RBX]), (0xF00D, 0xBB, 0xAA));
    }

    #[test]
    fn version_2_lists_its_epilogues() {
        // Epilogue of 2 bytes at the end (flag 1), one more 0x10 back from the end.
        // Codes: EPILOG(size 2, flags 1), EPILOG(offset 0x10), push rbx at 1.
        let mut image = Image::new();
        image.put(0x100, &[2, 1, 3, 0, 2, 0x16, 0x10, 0x06, 1, 0x30]);
        let f = image.function(0, CODE, CODE + 0x40, 0x100);
        image.put(CODE as usize + 0x3E, &[0x5B, 0xC3]);
        image.put(CODE as usize + 0x30, &[0x5B, 0xC3]);
        let s = stack(&[0xB, 0xD00D]);
        let rsp = s.as_ptr() as u64;
        for at in [0x3E, 0x30] {
            let mut c = context(rsp);
            let pc = image.base() as u64 + u64::from(CODE) + at;
            virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
            assert_eq!((c.gpr[RBX], c.rip), (0xB, 0xD00D), "{at:#x}");
        }
        // Elsewhere: the body, unwound through its codes (the same result here).
        let mut c = context(rsp);
        let pc = image.base() as u64 + u64::from(CODE) + 0x10;
        virtual_unwind(UNW_FLAG_NHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!(c.rip, 0xD00D);
    }

    #[test]
    fn handlers_come_from_the_info() {
        // EHANDLER, no codes, handler RVA 0x1800, data after it.
        let mut image = Image::new();
        image.put(0x100, &[1 | (1 << 3), 0, 0, 0, 0x00, 0x18, 0, 0, 0xAB]);
        let f = image.function(0, CODE, CODE + 0x10, 0x100);
        let s = stack(&[0x1]);
        let pc = image.base() as u64 + u64::from(CODE) + 2;
        let mut c = context(s.as_ptr() as u64);
        let u = virtual_unwind(UNW_FLAG_EHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!(u.handler, image.base() + 0x1800);
        assert_eq!(u.handler_data, image.base() + 0x108);
        let mut c = context(s.as_ptr() as u64);
        let u = virtual_unwind(UNW_FLAG_UHANDLER, image.base(), pc, f, &mut c, None).unwrap();
        assert_eq!(u.handler, 0);
    }

    #[test]
    fn lookups_find_tables_and_end_in_our_code() {
        let mut image = Image::new();
        image.put(0x100, &[1, 0, 0, 0]);
        image.function(0, CODE, CODE + 0x10, 0x100);
        image.function(12, CODE + 0x20, CODE + 0x30, 0x100);
        let table = image.base();
        assert!(add_table(table, 2, image.base()));
        let at = |rva: u32| lookup(image.base() as u64 + u64::from(rva));
        assert_eq!(at(CODE + 4), Entry::Function(table as *mut _, image.base()));
        assert_eq!(at(CODE + 0x2F), Entry::Function((table + 12) as *mut _, image.base()));
        assert_eq!(at(CODE + 0x18), Entry::Leaf(image.base()));
        assert!(remove_table(table) && !remove_table(table));
        assert_ne!(at(CODE + 4), Entry::Function(table as *mut _, image.base()));
        // Our code: the boundary, whose unwind ends a walk.
        let here = lookups_find_tables_and_end_in_our_code as *const () as u64;
        assert_eq!(lookup(here), Entry::Boundary);
        let mut c = asm::capture();
        assert!(!step(&mut c));
        // The frame call is found like title code.
        let frame_call = asm::base() as u64 + 8;
        assert_eq!(lookup(frame_call), Entry::Function(asm::frame_entry() as *mut _, asm::base()));
    }

    #[test]
    fn capture_keeps_the_registers_and_returns_here() {
        let c = asm::capture();
        let here = capture_keeps_the_registers_and_returns_here as *const () as u64;
        assert!(c.rip > here && c.rip < here + 0x400, "{:#x} {here:#x}", c.rip);
        assert_eq!(c.context_flags, CONTEXT_FULL | CONTEXT_SEGMENTS);
        assert_eq!(c.vector_control, 0);
        assert_ne!(c.seg_cs, 0);
        assert_eq!(c.mx_csr, c.flt_save.mx_csr);
        let local = 0u8;
        assert!(c.gpr[RSP] < &local as *const u8 as u64 + 0x1000 && c.gpr[RSP] > &local as *const u8 as u64 - 0x1000);
    }
}
