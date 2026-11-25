//! What the `rustix` rewrite of the image mapping costs, measured: the
//! `mmap` + `mprotect` + `munmap` sequence the loader performs, timed
//! directly. `rustix` is a thin wrapper over the same syscalls, so the
//! numbers are the check that it added no work — which is the reason the
//! crate was safe to adopt.
//!
//! Run: `cargo bench -p victory-loader --bench mmap`.

use std::hint::black_box;
use std::time::Instant;

use rustix::mm::{MapFlags, MprotectFlags, ProtFlags};

fn main() {
    // 8 MiB: a plausible image, and a size the loader maps whole.
    let size = 8 << 20;
    let runs = 400;

    let start = Instant::now();
    for _ in 0..runs {
        // SAFETY: a fresh anonymous mapping, then a protection change and
        // release of that same mapping.
        unsafe {
            let p = black_box(
                rustix::mm::mmap_anonymous(
                    std::ptr::null_mut(),
                    size,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::PRIVATE,
                )
                .expect("mmap"),
            );
            rustix::mm::mprotect(p, size, MprotectFlags::READ | MprotectFlags::WRITE)
                .expect("mprotect");
            rustix::mm::munmap(p, size).expect("munmap");
        }
    }
    let per = start.elapsed().as_nanos() as f64 / runs as f64;

    println!("mmap + mprotect + munmap of 8 MiB, ns per sequence: {per:.0}");
    println!();
    println!("Three syscalls on a region this size; anything much above the");
    println!("cost of the syscalls themselves would mean `rustix` is doing work");
    println!("of its own, which it must not.");
}
