//! Making the console's control flow legal SPIR-V.
//!
//! SPIR-V requires every block that branches on a condition to name a merge
//! point with `OpSelectionMerge`, and every loop header a continue target
//! with `OpLoopMerge`. The console's CFG says nothing about either: GFX10
//! branches are just predicated jumps, and a compiler leftover can leave a
//! block that jumps back into the middle of a region. So the merges are
//! computed here, from the CFG, and a header that has no legal merge is
//! reported as a gap rather than emitted as an invalid module.
//!
//! The analysis is the textbook one: dominators first (a block dominates
//! everything reachable only through it), then for each header the set of
//! blocks it dominates, and the merge is the nearest block in that set that
//! the region's exits reach.

use std::collections::{BTreeSet, HashMap};

use victory_shader::ir::Exit;

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
}

impl Structured {
    /// Computes the merges every block's terminator needs.
    pub fn of(blocks: &[victory_shader::ir::Block]) -> Self {
        let n = blocks.len();
        let dominators = dominators(blocks);
        let mut merges = vec![Merge::None; n];
        // Back edges first: a header with one is a loop, whatever else it
        // looks like, and its continue target decides its own merge.
        let loops = loops_of(blocks, &dominators);
        for (&header, &continue_at) in &loops {
            let Some(merge) = loop_merge(blocks, &dominators, header, continue_at) else {
                continue;
            };
            merges[header] = Merge::Loop { merge, continue_at };
        }
        for (i, block) in blocks.iter().enumerate() {
            if merges[i] != Merge::None {
                continue;
            }
            let targets: Vec<usize> = block.exit.successors();
            if targets.len() < 2 {
                continue;
            }
            if let Some(merge) = selection_merge(blocks, &dominators, i, &targets) {
                merges[i] = Merge::Selection(merge);
            }
        }
        Structured { merges }
    }

    /// Whether block `i`'s terminator is structured (its merge was found).
    pub fn is_structured(&self, i: usize, blocks: &[victory_shader::ir::Block]) -> bool {
        let targets = blocks[i].exit.successors();
        targets.len() < 2 || self.merges[i] != Merge::None
    }
}

/// The dominator set of each block, as a bitset.
fn dominators(blocks: &[victory_shader::ir::Block]) -> Vec<BTreeSet<usize>> {
    let n = blocks.len();
    let all: BTreeSet<usize> = (0..n).collect();
    let mut dom = vec![all.clone(); n];
    dom[0] = BTreeSet::from([0]);
    let mut changed = true;
    while changed {
        changed = false;
        for i in 1..n {
            let mut next = all.clone();
            for (k, pred) in blocks[i].preds.iter().enumerate() {
                if k == 0 {
                    next = dom[*pred].clone();
                } else {
                    next = next.intersection(&dom[*pred]).copied().collect();
                }
            }
            next.insert(i);
            if next != dom[i] {
                dom[i] = next;
                changed = true;
            }
        }
    }
    dom
}

/// Loop headers and their continue targets: a back edge is an edge to a
/// block that dominates its source.
fn loops_of(
    blocks: &[victory_shader::ir::Block],
    dominators: &[BTreeSet<usize>],
) -> HashMap<usize, usize> {
    let mut loops = HashMap::new();
    for (i, block) in blocks.iter().enumerate() {
        for target in block.exit.successors() {
            if dominators[i].contains(&target) {
                // The source is the continue target unless it is the header
                // itself; a later back edge to the same header wins, since a
                // continue target has to be dominated by the header.
                loops.entry(target).or_insert(i);
            }
        }
    }
    loops
}

/// The merge of a loop: the nearest block the header dominates that every
/// exit of the loop reaches.
fn loop_merge(
    blocks: &[victory_shader::ir::Block],
    dominators: &[BTreeSet<usize>],
    header: usize,
    continue_at: usize,
) -> Option<usize> {
    let body = loop_body(blocks, header, continue_at);
    // Whatever leaves the loop must reach the merge; anything reached only
    // from outside must not be it.
    let exits: Vec<usize> = body
        .iter()
        .flat_map(|i| blocks[*i].exit.successors())
        .filter(|t| !body.contains(t))
        .collect();
    if exits.is_empty() {
        // A loop that never exits: any dominated block outside the body will
        // do, and the alternative is no SPIR-V at all. The `s_endpgm` inside
        // it terminates the lanes, so the merge is never reached.
        return dominators[header]
            .iter()
            .copied()
            .find(|b| !body.contains(b) && *b != header);
    }
    nearest_common(dominators, header, &exits, |b| !body.contains(&b) || b == header)
}

/// The blocks of the loop headed by `header`.
fn loop_body(
    blocks: &[victory_shader::ir::Block],
    header: usize,
    continue_at: usize,
) -> BTreeSet<usize> {
    let mut body = BTreeSet::from([header, continue_at]);
    let mut stack = vec![header, continue_at];
    while let Some(i) = stack.pop() {
        for target in blocks[i].exit.successors() {
            if body.insert(target) {
                stack.push(target);
            }
        }
    }
    body
}

/// The nearest block that dominates the header and every one of `exits`.
fn nearest_common(
    dominators: &[BTreeSet<usize>],
    header: usize,
    exits: &[usize],
    accept: impl Fn(usize) -> bool,
) -> Option<usize> {
    let mut candidates: BTreeSet<usize> = dominators[header].clone();
    for exit in exits {
        candidates = candidates.intersection(&dominators[*exit]).copied().collect();
    }
    candidates
        .into_iter()
        .filter(|b| *b != header && accept(*b))
        .min()
}

/// The merge of a selection: the nearest block the header dominates that all
/// of its arms reach.
fn selection_merge(
    blocks: &[victory_shader::ir::Block],
    dominators: &[BTreeSet<usize>],
    header: usize,
    targets: &[usize],
) -> Option<usize> {
    // The blocks the selection covers: everything reachable from an arm that
    // the header also dominates, stopping at the merge.
    let mut candidates: BTreeSet<usize> = dominators[header].clone();
    candidates.remove(&header);
    // The merge dominates every arm, and no arm may reach it without being
    // inside the selection, so the candidate set is walked from the arms.
    let reachable: BTreeSet<usize> = targets
        .iter()
        .flat_map(|t| reach(blocks, *t, &candidates))
        .collect();
    let mut best: Option<usize> = None;
    for candidate in &candidates {
        // Every arm must reach it, and it must not re-enter the selection.
        if !targets
            .iter()
            .all(|t| *t == *candidate || reach(blocks, *t, &candidates).contains(candidate))
        {
            continue;
        }
        // Blocks between the header and the candidate must all be inside the
        // selection: a block that leaves without reaching it makes this an
        // illegal merge.
        let header_region: BTreeSet<usize> = reach(blocks, header, &candidates);
        let mut legal = true;
        for block in &header_region {
            if *block == *candidate {
                continue;
            }
            if !reachable.contains(block) && !targets.contains(block) {
                legal = false;
                break;
            }
        }
        if !legal {
            continue;
        }
        best = Some(best.map_or(*candidate, |b: usize| b.min(*candidate)));
    }
    best
}

/// Every block reachable from `start` without leaving `allowed`.
fn reach(
    blocks: &[victory_shader::ir::Block],
    start: usize,
    allowed: &BTreeSet<usize>,
) -> BTreeSet<usize> {
    let mut seen = BTreeSet::from([start]);
    let mut stack = vec![start];
    while let Some(i) = stack.pop() {
        for target in blocks[i].exit.successors() {
            if allowed.contains(&target) && seen.insert(target) {
                stack.push(target);
            }
        }
    }
    seen
}
