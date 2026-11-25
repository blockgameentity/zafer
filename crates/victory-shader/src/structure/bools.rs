use super::*;

const TERMINAL: u32 = u32::MAX;
const MAX_NODES: usize = 1 << 21;
/// Case splits [`Bools::meet`] makes before it gives up.
const MAX_MEET: usize = 1 << 16;

impl Bools {
    pub(crate) fn new() -> Self {
        Bools {
            nodes: vec![
                (TERMINAL, Bool::FALSE, Bool::FALSE),
                (TERMINAL, Bool::TRUE, Bool::TRUE),
            ],
            unique: Map::default(),
            memo: Map::default(),
            atoms: Vec::new(),
            born: Vec::new(),
            ids: Map::default(),
            overflow: false,
        }
    }

    /// The atom tested at the top of `b` and the results when it is false
    /// and true; `None` for the constants.
    pub fn node(&self, b: Bool) -> Option<(Atom, Bool, Bool)> {
        let (v, lo, hi) = self.nodes[b.0 as usize];
        (v != TERMINAL).then(|| (self.atoms[v as usize], lo, hi))
    }

    /// The atoms `b` depends on, in variable order.
    pub fn support(&self, b: Bool) -> Vec<Atom> {
        let mut vars = self.vars(b);
        vars.sort_unstable();
        vars.into_iter().map(|v| self.atoms[v as usize]).collect()
    }

    pub(crate) fn vars(&self, b: Bool) -> Vec<u32> {
        let (mut seen, mut vars, mut stack) = (Vec::new(), Vec::new(), vec![b]);
        while let Some(b) = stack.pop() {
            let (v, lo, hi) = self.nodes[b.0 as usize];
            if v == TERMINAL || seen.contains(&b) {
                continue;
            }
            seen.push(b);
            if !vars.contains(&v) {
                vars.push(v);
            }
            stack.extend([lo, hi]);
        }
        vars
    }

    pub(crate) fn atom(&mut self, atom: Atom, born: usize) -> Bool {
        let v = *self.ids.entry(atom).or_insert_with(|| {
            self.atoms.push(atom);
            self.born.push(born);
            (self.atoms.len() - 1) as u32
        });
        self.mk(v, Bool::FALSE, Bool::TRUE)
    }

    pub(crate) fn mk(&mut self, v: u32, lo: Bool, hi: Bool) -> Bool {
        if lo == hi {
            return lo;
        }
        if let Some(&b) = self.unique.get(&(v, lo, hi)) {
            return b;
        }
        if self.nodes.len() >= MAX_NODES {
            // The caller fails once it sees `overflow`.
            self.overflow = true;
            return Bool::FALSE;
        }
        let b = Bool(self.nodes.len() as u32);
        self.nodes.push((v, lo, hi));
        self.unique.insert((v, lo, hi), b);
        b
    }

    /// Cofactors of `b` for `v`, which is `b`'s top variable or above it.
    pub(crate) fn split(&self, b: Bool, v: u32) -> (Bool, Bool) {
        match self.nodes[b.0 as usize] {
            (bv, lo, hi) if bv == v => (lo, hi),
            _ => (b, b),
        }
    }

    pub(crate) fn ite(&mut self, f: Bool, g: Bool, h: Bool) -> Bool {
        if f == Bool::TRUE || g == h {
            return g;
        }
        if f == Bool::FALSE {
            return h;
        }
        if (g, h) == (Bool::TRUE, Bool::FALSE) {
            return f;
        }
        if let Some(&r) = self.memo.get(&(f, g, h)) {
            return r;
        }
        let top = |b: Bool| self.nodes[b.0 as usize].0;
        let v = top(f).min(top(g)).min(top(h));
        let ((f0, f1), (g0, g1), (h0, h1)) = (self.split(f, v), self.split(g, v), self.split(h, v));
        let lo = self.ite(f0, g0, h0);
        let hi = self.ite(f1, g1, h1);
        let r = self.mk(v, lo, hi);
        self.memo.insert((f, g, h), r);
        r
    }

    pub(crate) fn and(&mut self, a: Bool, b: Bool) -> Bool {
        self.ite(a, b, Bool::FALSE)
    }

    pub(crate) fn or(&mut self, a: Bool, b: Bool) -> Bool {
        self.ite(a, Bool::TRUE, b)
    }

    pub(crate) fn not(&mut self, a: Bool) -> Bool {
        self.ite(a, Bool::FALSE, Bool::TRUE)
    }

    pub(crate) fn xor(&mut self, a: Bool, b: Bool) -> Bool {
        let nb = self.not(b);
        self.ite(a, nb, b)
    }

    /// Whether some lane can satisfy every `(b, holds)` of `all` at once:
    /// `b` where `holds`, its negation where not. Yes when the search grows
    /// too large to finish.
    pub fn meet(&self, all: &[(Bool, bool)]) -> bool {
        let mut budget = MAX_MEET;
        self.meet_in(all.to_vec(), &mut budget)
    }

    pub(crate) fn meet_in(&self, mut all: Vec<(Bool, bool)>, budget: &mut usize) -> bool {
        let fails = |&(b, holds): &(Bool, bool)| b == if holds { Bool::FALSE } else { Bool::TRUE };
        if all.iter().any(fails) {
            return false;
        }
        all.retain(|&(b, _)| b != Bool::FALSE && b != Bool::TRUE);
        if all.is_empty() || *budget == 0 {
            return true;
        }
        *budget -= 1;
        let top = |b: Bool| self.nodes[b.0 as usize].0;
        let v = all.iter().map(|&(b, _)| top(b)).min().expect("not empty");
        [false, true].into_iter().any(|value| {
            let next = all
                .iter()
                .map(|&(b, holds)| {
                    let (lo, hi) = self.split(b, v);
                    (if value { hi } else { lo }, holds)
                })
                .collect();
            self.meet_in(next, budget)
        })
    }

    pub(crate) fn implies(&mut self, a: Bool, b: Bool) -> bool {
        let nb = self.not(b);
        self.and(a, nb) == Bool::FALSE
    }

    /// `b` with every atom born at reverse-postorder index `from` or later
    /// existentially quantified: what still holds once those are redefined.
    pub(crate) fn forget(&mut self, b: Bool, from: usize, memo: &mut Map<Bool, Bool>) -> Bool {
        let (v, lo, hi) = self.nodes[b.0 as usize];
        if v == TERMINAL {
            return b;
        }
        if let Some(&r) = memo.get(&b) {
            return r;
        }
        let lo = self.forget(lo, from, memo);
        let hi = self.forget(hi, from, memo);
        let r = if self.born[v as usize] >= from {
            self.or(lo, hi)
        } else {
            self.mk(v, lo, hi)
        };
        memo.insert(b, r);
        r
    }

    pub(crate) fn restrict(&mut self, b: Bool, v: u32, value: bool, memo: &mut Map<Bool, Bool>) -> Bool {
        let (bv, lo, hi) = self.nodes[b.0 as usize];
        if bv == TERMINAL || bv > v {
            return b;
        }
        if bv == v {
            return if value { hi } else { lo };
        }
        if let Some(&r) = memo.get(&b) {
            return r;
        }
        let l = self.restrict(lo, v, value, memo);
        let h = self.restrict(hi, v, value, memo);
        let r = self.mk(bv, l, h);
        memo.insert(b, r);
        r
    }

    /// `b` with each atom that `known` decides replaced by its value.
    pub(crate) fn given(&mut self, mut b: Bool, known: Bool) -> Bool {
        for v in self.vars(b) {
            let x = self.mk(v, Bool::FALSE, Bool::TRUE);
            let nx = self.not(x);
            if self.implies(known, x) {
                b = self.restrict(b, v, true, &mut Map::default());
            } else if self.implies(known, nx) {
                b = self.restrict(b, v, false, &mut Map::default());
            }
        }
        b
    }
}
