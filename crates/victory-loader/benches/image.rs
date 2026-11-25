//! What mapping a title image saves over reading it, measured.
//!
//! A title's main image is tens of megabytes, and the loader only reads it.
//! `fs::read` copies all of it through a `Vec`; `Mmap` hands back the page
//! cache. This times both on a file of the size a title actually has, so the
//! claim "no copy" is a number rather than an argument.
//!
//! Run: `cargo bench -p victory-loader --bench image`.

use std::hint::black_box;
use std::io::Write;
use std::time::Instant;

/// The size measured: a large title image, larger than any we have seen.
const SIZE: usize = 256 * 1024 * 1024;

/// The median of `runs` timings of `f`, in nanoseconds.
fn median(runs: usize, mut f: impl FnMut()) -> u128 {
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        f();
        times.push(start.elapsed().as_nanos());
    }
    times.sort_unstable();
    times[times.len() / 2]
}

fn main() {
    let dir = std::env::temp_dir().join("victory-image-bench");
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let path = dir.join("image.bin");
    // Contents do not matter; only the size does, and a little entropy so
    // the filesystem cannot store it as a hole.
    let mut file = std::fs::File::create(&path).expect("create");
    let chunk = vec![0x90u8; 1024 * 1024];
    for _ in 0..(SIZE / chunk.len()) {
        file.write_all(&chunk).expect("write");
    }
    drop(file);

    let runs = 20;
    let read = median(runs, || {
        black_box(std::fs::read(&path).expect("read"));
    });
    let mapped = median(runs, || {
        let f = std::fs::File::open(&path).expect("open");
        // SAFETY: read-only mapping of an unchanging file.
        let m = unsafe { memmap2::Mmap::map(&f) }.expect("map");
        black_box(m.len());
    });

    println!(
        "reading vs mapping a {} MiB image, ns (median of {runs}):",
        SIZE / (1024 * 1024)
    );
    println!("  fs::read  {read}");
    println!("  Mmap::map {mapped}");
    if mapped > 0 {
        println!("  speedup   {:.1}x", read as f64 / mapped as f64);
    }
    std::fs::remove_dir_all(&dir).ok();
}
