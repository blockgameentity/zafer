//! kernel32: the machine as title code sizes itself by it: processors and
//! their topology (from sysfs), memory, processor features.
//!
//! Guest processor `i` (bit `i` of an affinity mask, processor `i` of
//! `GetLogicalProcessorInformation`) is entry `i` of [`topology`]`.host`:
//! the host's online processors ordered so that the hardware threads of
//! one core are neighbours, as Windows numbers them (Linux often numbers
//! all first threads before all second ones). One group, so at most 64.

#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::sync::LazyLock;

use victory_loader::linux::teb::set_last_error;

use super::kernel32_wait::ERROR_INVALID_PARAMETER;
use crate::{HleModule, hle_export};

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

pub(crate) struct Cache {
    level: u8,
    kind: u32, // PROCESSOR_CACHE_TYPE: 0 unified, 1 instruction, 2 data
    size: u32,
    line: u16,
    ways: u8,
    mask: u64,
}

pub(crate) struct Topology {
    /// Guest processor -> host CPU number.
    pub(crate) host: Vec<usize>,
    /// Each core's guest mask.
    cores: Vec<u64>,
    packages: Vec<u64>,
    caches: Vec<Cache>,
}

impl Topology {
    /// Every guest processor.
    pub(crate) fn all(&self) -> u64 {
        mask_below(self.host.len())
    }
}

fn mask_below(n: usize) -> u64 {
    if n >= 64 { u64::MAX } else { (1 << n) - 1 }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// A sysfs CPU list ("0-3,8,10-11").
fn cpu_list(text: &str) -> Vec<usize> {
    text.split(',')
        .filter_map(|part| {
            let (a, b) = part.split_once('-').unwrap_or((part, part));
            Some(a.trim().parse::<usize>().ok()?..=b.trim().parse::<usize>().ok()?)
        })
        .flatten()
        .collect()
}

pub(crate) fn topology() -> &'static Topology {
    static TOPOLOGY: LazyLock<Topology> = LazyLock::new(|| {
        let cpu = |n: usize, file: &str| read(&format!("/sys/devices/system/cpu/cpu{n}/{file}"));
        let mut online = read("/sys/devices/system/cpu/online").map(|l| cpu_list(&l)).unwrap_or_default();
        if online.is_empty() {
            online = (0..std::thread::available_parallelism().map_or(1, |n| n.get())).collect();
        }
        // (package, the core's first sibling, cpu): siblings end up adjacent.
        let mut order: Vec<(usize, usize, usize)> = online
            .iter()
            .map(|&c| {
                let package = cpu(c, "topology/physical_package_id").and_then(|p| p.parse().ok()).unwrap_or(0);
                let first = cpu(c, "topology/thread_siblings_list")
                    .and_then(|l| cpu_list(&l).into_iter().min())
                    .unwrap_or(c);
                (package, first, c)
            })
            .collect();
        order.sort_unstable();
        order.truncate(64);
        let host: Vec<usize> = order.iter().map(|o| o.2).collect();
        let guest_mask = |cpus: &[usize]| {
            host.iter().enumerate().filter(|(_, h)| cpus.contains(h)).fold(0u64, |m, (i, _)| m | 1 << i)
        };
        let mut cores = BTreeMap::new();
        let mut packages = BTreeMap::new();
        for (i, &(package, first, _)) in order.iter().enumerate() {
            *cores.entry((package, first)).or_insert(0u64) |= 1 << i;
            *packages.entry(package).or_insert(0u64) |= 1 << i;
        }
        let mut caches: Vec<Cache> = Vec::new();
        for &c in &host {
            for index in 0.. {
                let file = |f: &str| cpu(c, &format!("cache/index{index}/{f}"));
                let Some(level) = file("level").and_then(|l| l.parse().ok()) else {
                    break;
                };
                let kind = match file("type").as_deref() {
                    Some("Instruction") => 1,
                    Some("Data") => 2,
                    _ => 0,
                };
                let size = file("size").map_or(0, |s| {
                    let shift = match s.chars().last() {
                        Some('K') => 10,
                        Some('M') => 20,
                        _ => 0,
                    };
                    s.trim_end_matches(['K', 'M']).parse::<u32>().map_or(0, |n| n << shift)
                });
                let mask = guest_mask(&file("shared_cpu_list").map(|l| cpu_list(&l)).unwrap_or(vec![c]));
                if caches.iter().any(|x| x.level == level && x.kind == kind && x.mask == mask) {
                    continue;
                }
                caches.push(Cache {
                    level,
                    kind,
                    size,
                    line: file("coherency_line_size").and_then(|l| l.parse().ok()).unwrap_or(64),
                    ways: file("ways_of_associativity").and_then(|l| l.parse().ok()).unwrap_or(0),
                    mask,
                });
            }
        }
        caches.sort_by_key(|c| (c.mask.trailing_zeros(), c.level, c.kind));
        Topology {
            host,
            cores: cores.into_values().collect(),
            packages: packages.into_values().collect(),
            caches,
        }
    });
    &TOPOLOGY
}

/// `SYSTEM_INFO`.
#[repr(C)]
pub struct SystemInfo {
    processor_architecture: u16,
    reserved: u16,
    page_size: u32,
    minimum_application_address: usize,
    maximum_application_address: usize,
    active_processor_mask: usize,
    number_of_processors: u32,
    processor_type: u32,
    allocation_granularity: u32,
    processor_level: u16,
    processor_revision: u16,
}

pub extern "win64" fn GetNativeSystemInfo(info: *mut SystemInfo) {
    // SAFETY: cpuid leaf 1 exists on every x86-64 processor (safe to call
    // from Rust 1.89 on; unsafe before).
    #[allow(unused_unsafe)]
    let id = unsafe { std::arch::x86_64::__cpuid(1) }.eax;
    let family = (id >> 8) & 0xF;
    let (family, model) = match family {
        0xF => (family + ((id >> 20) & 0xFF), ((id >> 4) & 0xF) | ((id >> 12) & 0xF0)),
        6 => (family, ((id >> 4) & 0xF) | ((id >> 12) & 0xF0)),
        _ => (family, (id >> 4) & 0xF),
    };
    let t = topology();
    // SAFETY: the title's SYSTEM_INFO.
    unsafe {
        info.write_unaligned(SystemInfo {
            processor_architecture: 9, // PROCESSOR_ARCHITECTURE_AMD64
            reserved: 0,
            page_size: 4096,
            minimum_application_address: 0x1_0000,
            maximum_application_address: 0x7FFF_FFFE_FFFF,
            active_processor_mask: t.all() as usize,
            number_of_processors: t.host.len() as u32,
            processor_type: 8664, // PROCESSOR_AMD_X8664
            allocation_granularity: 0x1_0000,
            processor_level: family as u16,
            processor_revision: ((model << 8) | (id & 0xF)) as u16,
        })
    };
}

/// `SYSTEM_LOGICAL_PROCESSOR_INFORMATION`.
#[repr(C)]
pub struct LogicalProcessorInformation {
    processor_mask: usize,
    relationship: u32,
    padding: u32, // the union is 8-aligned
    /// The union: `Flags`, `NodeNumber` or a `CACHE_DESCRIPTOR`.
    data: [u8; 16],
}

const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_NUMA_NODE: u32 = 1;
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
const RELATION_GROUP: u32 = 4;
const RELATION_ALL: u32 = 0xFFFF;
const LTP_PC_SMT: u8 = 1;

fn cache_descriptor(c: &Cache) -> [u8; 12] {
    let mut d = [0u8; 12];
    d[0] = c.level;
    d[1] = c.ways;
    d[2..4].copy_from_slice(&c.line.to_le_bytes());
    d[4..8].copy_from_slice(&c.size.to_le_bytes());
    d[8..12].copy_from_slice(&c.kind.to_le_bytes());
    d
}

/// Answers a size-checked query: copies `data` if `*length` allows, else
/// `ERROR_INSUFFICIENT_BUFFER`; `*length` is the size either way.
fn answer(data: &[u8], buffer: *mut u8, length: *mut u32) -> i32 {
    if length.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: the title's in-out length.
    let room = unsafe { length.read_unaligned() } as usize;
    // SAFETY: as above.
    unsafe { length.write_unaligned(data.len() as u32) };
    if room < data.len() || buffer.is_null() {
        set_last_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the title's buffer holds `room` >= len bytes.
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len()) };
    1
}

pub extern "win64" fn GetLogicalProcessorInformation(buffer: *mut u8, length: *mut u32) -> i32 {
    let t = topology();
    let mut out = Vec::new();
    let mut push = |mask: u64, relationship: u32, data: &[u8]| {
        let mut entry = LogicalProcessorInformation {
            processor_mask: mask as usize,
            relationship,
            padding: 0,
            data: [0; 16],
        };
        entry.data[..data.len()].copy_from_slice(data);
        // SAFETY: a repr(C) struct of plain integers, read as bytes.
        out.extend_from_slice(unsafe {
            std::slice::from_raw_parts((&raw const entry).cast::<u8>(), size_of_val(&entry))
        });
    };
    for &core in &t.cores {
        push(core, RELATION_PROCESSOR_CORE, &[if core.count_ones() > 1 { LTP_PC_SMT } else { 0 }]);
    }
    for c in &t.caches {
        push(c.mask, RELATION_CACHE, &cache_descriptor(c));
    }
    for &package in &t.packages {
        push(package, RELATION_PROCESSOR_PACKAGE, &[]);
    }
    push(t.all(), RELATION_NUMA_NODE, &0u32.to_le_bytes());
    answer(&out, buffer, length)
}

/// The variable-sized `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` records:
/// `Relationship`, `Size`, then the relationship's body.
fn ex_records(relationship: u32) -> Vec<u8> {
    let t = topology();
    let mut out = Vec::new();
    let wanted = |r: u32| relationship == RELATION_ALL || relationship == r;
    // A GROUP_AFFINITY: mask, group 0.
    let group_mask = |mask: u64| {
        let mut g = [0u8; 16];
        g[..8].copy_from_slice(&mask.to_le_bytes());
        g
    };
    let mut record = |r: u32, body: Vec<u8>| {
        out.extend_from_slice(&r.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32 + 8).to_le_bytes());
        out.extend(body);
    };
    // PROCESSOR_RELATIONSHIP: Flags, EfficiencyClass, Reserved[20],
    // GroupCount at 22, GroupMask at 24.
    let processor = |mask: u64, flags: u8| {
        let mut b = vec![0u8; 40];
        b[0] = flags;
        b[22..24].copy_from_slice(&1u16.to_le_bytes());
        b[24..40].copy_from_slice(&group_mask(mask));
        b
    };
    if wanted(RELATION_PROCESSOR_CORE) {
        for &core in &t.cores {
            record(RELATION_PROCESSOR_CORE, processor(core, if core.count_ones() > 1 { LTP_PC_SMT } else { 0 }));
        }
    }
    if wanted(RELATION_NUMA_NODE) {
        // NUMA_NODE_RELATIONSHIP: NodeNumber, Reserved[18], GroupCount at 22, GroupMask at 24.
        let mut b = vec![0u8; 40];
        b[22..24].copy_from_slice(&1u16.to_le_bytes());
        b[24..40].copy_from_slice(&group_mask(t.all()));
        record(RELATION_NUMA_NODE, b);
    }
    if wanted(RELATION_CACHE) {
        // CACHE_RELATIONSHIP: the descriptor's fields, Reserved[18],
        // GroupCount at 30, GroupMask at 32.
        for c in &t.caches {
            let mut b = vec![0u8; 48];
            b[..12].copy_from_slice(&cache_descriptor(c));
            b[30..32].copy_from_slice(&1u16.to_le_bytes());
            b[32..48].copy_from_slice(&group_mask(c.mask));
            record(RELATION_CACHE, b);
        }
    }
    if wanted(RELATION_PROCESSOR_PACKAGE) {
        for &package in &t.packages {
            record(RELATION_PROCESSOR_PACKAGE, processor(package, 0));
        }
    }
    if wanted(RELATION_GROUP) {
        // GROUP_RELATIONSHIP: MaximumGroupCount, ActiveGroupCount,
        // Reserved[20], then one PROCESSOR_GROUP_INFO at 24 (maximum and
        // active count, Reserved[38], ActiveProcessorMask at 40).
        let mut b = vec![0u8; 72];
        b[0..2].copy_from_slice(&1u16.to_le_bytes());
        b[2..4].copy_from_slice(&1u16.to_le_bytes());
        b[24] = t.host.len() as u8;
        b[25] = t.host.len() as u8;
        b[64..72].copy_from_slice(&t.all().to_le_bytes());
        record(RELATION_GROUP, b);
    }
    out
}

pub extern "win64" fn GetLogicalProcessorInformationEx(relationship: u32, buffer: *mut u8, length: *mut u32) -> i32 {
    if relationship > RELATION_GROUP && relationship != RELATION_ALL {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    answer(&ex_records(relationship), buffer, length)
}

/// `MEMORYSTATUSEX`.
#[repr(C)]
pub struct MemoryStatusEx {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

pub extern "win64" fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32 {
    // SAFETY: the title's MEMORYSTATUSEX; its length is checked first.
    if status.is_null() || unsafe { (*status).length } as usize != size_of::<MemoryStatusEx>() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kib = |key: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            << 10
    };
    let (total, avail) = (kib("MemTotal"), kib("MemAvailable"));
    let total_virtual = 0x7FFF_FFFE_0000u64;
    let used_virtual = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmSize:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        })
        .unwrap_or(0)
        << 10;
    // SAFETY: as above.
    unsafe {
        status.write_unaligned(MemoryStatusEx {
            length: size_of::<MemoryStatusEx>() as u32,
            memory_load: (avail * 100).checked_div(total).map_or(0, |free| 100 - free as u32),
            total_phys: total,
            avail_phys: avail,
            total_page_file: total + kib("SwapTotal"),
            avail_page_file: avail + kib("SwapFree"),
            total_virtual,
            avail_virtual: total_virtual.saturating_sub(used_virtual),
            avail_extended_virtual: 0,
        })
    };
    1
}

pub extern "win64" fn IsProcessorFeaturePresent(feature: u32) -> i32 {
    use std::arch::is_x86_feature_detected as has;
    let present = match feature {
        // compare-exchange double, MMX, SSE, RDTSC, PAE, SSE2, NX
        2 | 3 | 6 | 8 | 9 | 10 | 12 => true,
        13 => has!("sse3"),
        14 => has!("cmpxchg16b"),
        17 => has!("xsave"),
        // PF_RDWRFSGSBASE: only where the kernel lets user code use them.
        // SAFETY: getauxval has no preconditions.
        22 => (unsafe { libc::getauxval(libc::AT_HWCAP2) } & 2) != 0,
        23 => true,         // PF_FASTFAIL_AVAILABLE
        36 => has!("ssse3"),
        37 => has!("sse4.1"),
        38 => has!("sse4.2"),
        39 => has!("avx"),
        40 => has!("avx2"),
        41 => has!("avx512f"),
        _ => false,
    };
    present.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-system",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-sysinfo-*",
            "api-ms-win-core-processthreads-*",
        ],
        exports: vec![
            hle_export!(GetNativeSystemInfo),
            hle_export!(GetLogicalProcessorInformation),
            hle_export!(GetLogicalProcessorInformationEx),
            hle_export!(GlobalMemoryStatusEx),
            hle_export!(IsProcessorFeaturePresent),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn guest_layouts_match_windows() {
        assert_eq!(size_of::<SystemInfo>(), 48);
        assert_eq!(offset_of!(SystemInfo, active_processor_mask), 24);
        assert_eq!(offset_of!(SystemInfo, number_of_processors), 32);
        assert_eq!(offset_of!(SystemInfo, allocation_granularity), 40);
        assert_eq!(offset_of!(SystemInfo, processor_revision), 46);
        assert_eq!(size_of::<LogicalProcessorInformation>(), 32);
        assert_eq!(offset_of!(LogicalProcessorInformation, data), 16);
        assert_eq!(size_of::<MemoryStatusEx>(), 64);
    }

    #[test]
    fn topology_covers_every_processor_once() {
        let t = topology();
        assert!(!t.host.is_empty());
        assert_eq!(t.cores.iter().fold(0, |m, c| { assert_eq!(m & c, 0); m | c }), t.all());
        assert_eq!(t.packages.iter().fold(0, |m, p| m | p), t.all());
        // Siblings are neighbours: every core's mask is one run of bits.
        for core in &t.cores {
            let run = core >> core.trailing_zeros();
            assert_eq!((run + 1) & run, 0);
        }
        assert_eq!(cpu_list("0-2,5,7-8"), [0, 1, 2, 5, 7, 8]);
    }

    #[test]
    fn processor_queries_size_and_fill() {
        let mut length = 0;
        assert_eq!(GetLogicalProcessorInformation(std::ptr::null_mut(), &mut length), 0);
        assert_eq!(length % 32, 0);
        let mut buffer = vec![0u8; length as usize];
        assert_eq!(GetLogicalProcessorInformation(buffer.as_mut_ptr(), &mut length), 1);
        let cores = buffer.chunks(32).filter(|e| e[8] == 0).count();
        assert_eq!(cores, topology().cores.len());

        let mut length = 0;
        assert_eq!(GetLogicalProcessorInformationEx(RELATION_ALL, std::ptr::null_mut(), &mut length), 0);
        let mut buffer = vec![0u8; length as usize];
        assert_eq!(GetLogicalProcessorInformationEx(RELATION_ALL, buffer.as_mut_ptr(), &mut length), 1);
        let mut at = 0;
        let mut sizes = BTreeMap::new();
        while at < buffer.len() {
            let r = u32::from_le_bytes(buffer[at..at + 4].try_into().unwrap());
            let size = u32::from_le_bytes(buffer[at + 4..at + 8].try_into().unwrap());
            sizes.insert(r, size);
            at += size as usize;
        }
        assert_eq!(at, buffer.len());
        assert_eq!(sizes[&RELATION_PROCESSOR_CORE], 48);
        assert_eq!(sizes[&RELATION_NUMA_NODE], 48);
        assert_eq!(sizes[&RELATION_GROUP], 80);
        if let Some(&cache) = sizes.get(&RELATION_CACHE) {
            assert_eq!(cache, 56);
        }

        // SAFETY: zeroed SYSTEM_INFO is valid.
        let mut info: SystemInfo = unsafe { std::mem::zeroed() };
        GetNativeSystemInfo(&mut info);
        assert_eq!((info.processor_architecture, info.page_size), (9, 4096));
        assert_eq!(info.number_of_processors as usize, topology().host.len());

        // SAFETY: zeroed MEMORYSTATUSEX is valid.
        let mut memory: MemoryStatusEx = unsafe { std::mem::zeroed() };
        assert_eq!(GlobalMemoryStatusEx(&mut memory), 0);
        memory.length = 64;
        assert_eq!(GlobalMemoryStatusEx(&mut memory), 1);
        assert!(memory.total_phys > 0 && memory.avail_phys <= memory.total_phys);
        assert_eq!(IsProcessorFeaturePresent(10), 1);
    }
}
