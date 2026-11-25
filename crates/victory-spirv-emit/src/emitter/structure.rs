//! Making the console's control flow legal SPIR-V.
//!
//! SPIR-V requires every block that branches on a condition to name a merge
//! point with `OpSelectionMerge`, and every loop header a merge and a
//! continue target with `OpLoopMerge`. The console's CFG says nothing about
//! either: GFX10 branches are predicated jumps, and a compiler leftover can
//! leave a block that jumps back into the middle of a region. So the merges
//! are computed here, from the CFG, and a header that has no legal merge is
//! a gap rather than an invalid module.
//!
//! The merge of a header is its immediate post-dominator: the first block
//! every path out of the header reaches. That is exactly what the
//! specification asks for (the merge is reachable from every arm, and the
//! header dominates it), and it needs one analysis rather than a search per
//! header. Dominators are computed too: they say which edges are back edges,
//! and so which blocks are loop headers.
//!
//! A block may be the merge of one header only ("block is already a merge
//! block for another header"), so a second header that would merge at the
//! same block gets an empty block of its own that branches there.

use std::collections::{BTreeSet, HashMap};

/// What a block's terminator needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// A plain branch of one target, or the end of the program.
    None,
    /// A selection whose join is this block.
    Selection(usize),
    /// A loop header: this is where it ends and continues.
    Loop { merge: usize, continue_at: usize },
}

/// The merges of a program's blocks, by block.
#[derive(Debug, Clone)]
pub struct Structured {
    pub merges: Vec<Merge>,
    /// The order the blocks must be written in: reverse postorder from the
    /// entry block. SPIR-V requires a block to appear after its dominator
    /// ("block appears in the binary before its dominator"), and the IR's
    /// own order is the machine code's, where a loop body can sit before
    /// the block that dominates it.
    pub order: Vec<usize>,
    /// Loop headers and their continue targets, merged or not: a header
    /// without a merge point has no SPIR-V form at all.
    pub loops: HashMap<usize, usize>,
    /// Blocks whose merge is a fresh block of its own, because the join
    /// they share is already another header's merge. Listed as
    /// (header, join).
    pub trampolines: Vec<(usize, usize)>,
}

impl Structured {
    /// Computes the merges every block's terminator needs.
    pub fn of(blocks: &[victory_shader::ir::Block]) -> Self {
        let n = blocks.len();
        let successors: Vec<Vec<usize>> = blocks.iter().map(|b| b.exit.successors()).collect();
        let dominators = dominators(n, &successors, &blocks.iter().map(|b| b.preds.clone()).collect::<Vec<_>>());
        let post = post_dominators(n, &successors);
        let loops = loops_of(n, &successors, &dominators);
        let mut merges = vec![Merge::None; n];
        for (&header, &continue_at) in &loops {
            let Some(merge) = immediate(&post, header) else {
                continue;
            };
            merges[header] = Merge::Loop { merge, continue_at };
        }
        for (i, targets) in successors.iter().enumerate() {
            if merges[i] != Merge::None || targets.len() < 2 {
                continue;
            }
            if let Some(merge) = immediate(&post, i) {
                merges[i] = Merge::Selection(merge);
            }
        }
        // A merge block serves one header: where two headers join at the
        // same block, the later one is given an empty block of its own that
        // branches to the join.
        let mut trampolines = Vec::new();
        let mut used: BTreeSet<usize> = BTreeSet::new();
        for (i, merge) in merges.iter().enumerate() {
            let join = match *merge {
                Merge::Selection(join) | Merge::Loop { merge: join, .. } => join,
                Merge::None => continue,
            };
            if !used.insert(join) {
                trampolines.push((i, join));
            }
        }
        Structured {
            merges,
            order: reverse_postorder(n, &successors),
            loops,
            trampolines,
        }
    }

    /// Whether block `i`'s branching terminator found a merge.
    pub fn is_structured(&self, i: usize, blocks: &[victory_shader::ir::Block]) -> bool {
        blocks[i].exit.successors().len() < 2 || self.merges[i] != Merge::None
    }
}

/// Reverse postorder from block 0, with any block the entry cannot reach
/// appended in index order so none is dropped.
fn reverse_postorder(n: usize, successors: &[Vec<usize>]) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let mut seen = vec![false; n];
    let mut post = Vec::with_capacity(n);
    // Iterative postorder: (block, next successor index) on the stack.
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    seen[0] = true;
    while let Some((block, next)) = stack.last_mut() {
        let targets = &successors[*block];
        if *next < targets.len() {
            let target = targets[*next];
            *next += 1;
            if !seen[target] {
                seen[target] = true;
                stack.push((target, 0));
            }
        } else {
            post.push(*block);
            stack.pop();
        }
    }
    for (i, slot) in seen.iter().enumerate() {
        if !slot {
            post.push(i);
        }
    }
    post.reverse();
    post
}

/// The blocks that dominate each block, as bitsets (`n` bits each).
///
/// A block starts with itself alone: starting from "everything" leaves a
/// block the entry cannot reach dominated by every block, because the
/// recurrence has nothing to shrink it, and the invented dominances invent
/// loops and back edges (`spirv-val`: "back-edges can only be formed between
/// a block and a loop header").
fn dominators(n: usize, successors: &[Vec<usize>], preds: &[Vec<usize>]) -> Vec<Vec<u64>> {
    let words = n.div_ceil(64);
    let mut dom: Vec<Vec<u64>> = (0..n)
        .map(|i| {
            let mut set = vec![0u64; words];
            set[i / 64] |= 1 << (i % 64);
            set
        })
        .collect();
    let _ = successors;
    let mut changed = true;
    while changed {
        changed = false;
        for i in 1..n {
            let mut next = vec![u64::MAX; words];
            let mut any = false;
            for pred in &preds[i] {
                any = true;
                for (word, value) in next.iter_mut().zip(&dom[*pred]) {
                    *word &= *value;
                }
            }
            if !any {
                continue;
            }
            next[i / 64] |= 1 << (i % 64);
            if next != dom[i] {
                dom[i] = next;
                changed = true;
            }
        }
    }
    dom
}

/// The blocks every path from a block reaches, itself included, over a
/// virtual exit that every block without a successor reaches.
///
/// Bit `n` is the virtual exit. A block that cannot reach the exit (an
/// endless loop) keeps every bit set and so has no post-dominator, which the
/// caller reads as "no legal merge".
fn post_dominators(n: usize, successors: &[Vec<usize>]) -> Vec<Vec<u64>> {
    let words = (n + 1).div_ceil(64);
    let all = {
        let mut set = vec![u64::MAX; words];
        let spare = words * 64 - (n + 1);
        if spare > 0 {
            *set.last_mut().expect("one word") &= u64::MAX >> spare;
        }
        set
    };
    let mut post: Vec<Vec<u64>> = vec![all.clone(); n];
    // The exit post-dominates itself and nothing else.
    let mut exit = vec![0u64; words];
    exit[n / 64] |= 1 << (n % 64);
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n {
            let mut next = all.clone();
            if successors[i].is_empty() {
                next = exit.clone();
            } else {
                for target in &successors[i] {
                    for (word, value) in next.iter_mut().zip(&post[*target]) {
                        *word &= *value;
                    }
                }
            }
            next[i / 64] |= 1 << (i % 64);
            if next != post[i] {
                post[i] = next;
                changed = true;
            }
        }
    }
    post
}

/// The immediate post-dominator of block `i`: the first block every path
/// from it reaches.
///
/// The strict post-dominators of `i` are nested; the nearest is the one with
/// the fewest post-dominators of its own, which is the one with the smallest
/// popcount.
fn immediate(post: &[Vec<u64>], i: usize) -> Option<usize> {
    let n = post.len();
    let set = post.get(i)?;
    let mut best: Option<(u32, usize)> = None;
    for candidate in 0..n {
        if candidate == i || set[candidate / 64] >> (candidate % 64) & 1 == 0 {
            continue;
        }
        let size: u32 = post[candidate].iter().map(|w| w.count_ones()).sum();
        if best.is_none_or(|(best, _)| size < best) {
            best = Some((size, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// Loop headers and their continue targets: a back edge is an edge to a
/// block that dominates its source.
fn loops_of(n: usize, successors: &[Vec<usize>], dominators: &[Vec<u64>]) -> HashMap<usize, usize> {
    let mut loops = HashMap::new();
    for (i, targets) in successors.iter().enumerate() {
        for target in targets {
            let dominates = dominators[i][target / 64] >> (target % 64) & 1 != 0;
            if *target == i || dominates {
                // The back edge's source is the continue target: the block
                // that branches back to the header.
                loops.entry(*target).or_insert(i);
            }
        }
    }
    let _ = n;
    loops
}
