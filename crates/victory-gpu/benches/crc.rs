//! What the capture's CRC costs, measured: the JPEG-sized buffer a frame
//! dump hashes, once through the bit-at-a-time loop the writer has now and
//! once through `crc32fast`, which the replacement uses.
//!
//! Run: `cargo bench -p victory-gpu --bench crc`.

use std::hint::black_box;
use std::time::Instant;

/// The loop `capture::png` hashes each chunk with today, copied verbatim so
/// the comparison is against the code that is actually there.
fn bitwise(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The median of `runs` timings of `f`, in nanoseconds.
fn median(mut runs: u32, mut f: impl FnMut() -> u32) -> u128 {
    let mut times = Vec::new();
    while runs > 0 {
        let start = Instant::now();
        black_box(f());
        times.push(start.elapsed().as_nanos());
        runs -= 1;
    }
    times.sort_unstable();
    times[times.len() / 2]
}

fn main() {
    // A 1080p frame of raw B8G8R8A8, which is what a dump hashes before
    // compressing, and one 64 KiB chunk, the granularity of a PNG chunk.
    for len in [64 * 1024, 1920 * 1080 * 4] {
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let slow = median(20, || bitwise(black_box(&data)));
        let fast = median(20, || crc32fast::hash(black_box(&data)));
        println!(
            "{len:>9} bytes: bitwise {slow:>10} ns, crc32fast {fast:>8} ns, {:>5.1}x",
            slow as f64 / fast as f64,
        );
        assert_eq!(bitwise(&data), crc32fast::hash(&data), "different answer");
    }
}
