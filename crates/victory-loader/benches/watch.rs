//! What write watching costs, measured rather than asserted.
//!
//! Guarding a block is `mprotect`, a write to a guarded block is a `SIGSEGV`
//! and the handler's round trip, and a write to an open block is a plain
//! write. Those are three very different numbers, and the ratio between the
//! last two decides whether watching is affordable for a title's
//! GPU-visible memory — so it is measured here, not estimated.
//!
//! Run with `cargo bench -p victory-loader --bench watch`.

use std::hint::black_box;
use std::time::Instant;

use victory_loader::linux::{signals, vm, watch};

/// `n` runs of `f`, in nanoseconds each, as the median.
fn per_run(n: u32, mut f: impl FnMut()) -> u64 {
    let mut samples = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let start = Instant::now();
        f();
        samples.push(start.elapsed().as_nanos() as u64);
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// A committed read-write block of `blocks` 64 KiB blocks.
fn region(blocks: usize) -> (usize, usize) {
    let size = blocks * watch::BLOCK;
    let at = vm::alloc(0, size, vm::MEM_RESERVE | vm::MEM_COMMIT, vm::PAGE_READWRITE, 0x1000)
        .expect("reserve and commit");
    (at, at + size)
}

fn main() {
    // The taken-fault measurement needs the real handler: without it a
    // write to a guarded block is a fatal `SIGSEGV`, which is also the
    // reason the guard can only ever be used with it installed.
    signals::install();
    let runs = 2000;
    let (lo, _hi) = region(1);

    let open = per_run(runs, || {
        // SAFETY: committed writable memory of this process.
        unsafe { (lo as *mut u64).write_volatile(black_box(1u64)) };
        black_box(lo);
    });

    let one_protect = per_run(runs, || {
                vm::protect(lo, watch::BLOCK, vm::PAGE_READONLY).expect("protect");
        vm::protect(lo, watch::BLOCK, vm::PAGE_READWRITE).expect("protect");
        black_box(lo);
    }) / 2;

    let n = 64usize;
    let (wide, wide_hi) = region(n);
    let batch = per_run(200, || {
        let mut at = wide;
        while at < wide_hi {
                        vm::protect(at, watch::BLOCK, vm::PAGE_READONLY).expect("protect");
            at += watch::BLOCK;
        }
                vm::protect(wide, n * watch::BLOCK, vm::PAGE_READWRITE).expect("protect");
        black_box(wide);
    });

    // The number that matters: one guarded block, written once, taken by
    // the real fault handler.
    let fault = per_run(200, || {
        vm::protect(lo, watch::BLOCK, vm::PAGE_READONLY).expect("protect");
        watch::mark(lo / watch::BLOCK);
        // SAFETY: the handler opens the block and retries the write.
        unsafe { (lo as *mut u64).write_volatile(black_box(2u64)) };
        black_box(lo);
    });

    println!("write watching, ns per operation (median of {runs}):");
    println!("  write to an open block            {open:>9}");
    println!("  one mprotect pair (guard + open)  {one_protect:>9}");
    println!("  {n} blocks guarded, one batch        {batch:>9}");
    println!("  per block of that batch           {:>9}", batch / n as u64);
    println!("  one taken write fault             {fault:>9}");
    println!(
        "  a taken fault costs {:.0}x a plain write",
        fault as f64 / open.max(1) as f64
    );
    println!(
        "  guarding a 1 MiB copy (16 blocks)  {:.1} us",
        (batch / n as u64) as f64 * 16.0 / 1000.0
    );
}
