//! Reading host memory as typed data, measured: what the `from_raw_parts`
//! sites cost today (a copy into an owned `Vec`) against `bytemuck::cast_slice`
//! (one length-and-alignment check, then a borrow).
//!
//! Run: `cargo bench -p victory-gpu --bench cast`.

use std::hint::black_box;
use std::time::Instant;

/// The copy the hand-written sites do: reinterpret, then own the bytes.
fn copying(bytes: &[u8]) -> u32 {
    // SAFETY: `bytes` is a `&[u32]` in disguise: it came from one.
    let words =
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u32>(), bytes.len() / 4) };
    let owned: Vec<u32> = words.to_vec();
    black_box(&owned);
    owned.len() as u32
}

/// The borrow `bytemuck` allows: the same words, no second buffer.
fn borrowing(bytes: &[u8]) -> u32 {
    let words: &[u32] = bytemuck::cast_slice(bytes);
    black_box(words);
    words.len() as u32
}

/// The median of `runs` timings, in nanoseconds.
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
    // A 4 KiB draw's worth of words and a 4 MiB upload, both laid out as
    // dwords because that is what a command buffer and a vertex buffer are.
    for words in [1024usize, 1024 * 1024] {
        let bytes: Vec<u8> = (0..words * 4).map(|i| (i % 251) as u8).collect();
        let copy = median(200, || copying(black_box(&bytes)));
        let borrow = median(200, || borrowing(black_box(&bytes)));
        println!(
            "{:>9} words: copy {copy:>8} ns, borrow {borrow:>6} ns, {:>5.1}x",
            words,
            copy as f64 / borrow as f64,
        );
        assert_eq!(copying(&bytes), borrowing(&bytes), "different answer");
    }
}
