use super::*;

impl<'a> Lanes<'a> {
    pub(crate) fn new(p: &'a Program) -> Result<Self, ShaderError> {
        let blocks = &p.blocks;
        let order = crate::ir::reverse_postorder(blocks);
        let mut index = vec![usize::MAX; blocks.len()];
        for (i, &b) in order.iter().enumerate() {
            index[b] = i;
        }
        check_reducible(p, &order, &index)?;
        let mut incoming = vec![Vec::new(); blocks.len()];
        let (mut defs, mut phis) = (Map::default(), Map::default());
        for (b, block) in blocks.iter().enumerate() {
            for (k, s) in block.exit.successors().into_iter().enumerate() {
                incoming[s].push((b, k == 0));
            }
            for (i, phi) in block.phis.iter().enumerate() {
                phis.insert(phi.value, (b, i));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                for &(_, v) in &inst.writes {
                    defs.insert(v, (b, i));
                }
            }
        }
        Ok(Lanes {
            p,
            order,
            index,
            incoming,
            defs,
            phis,
            unresolved: HashSet::new(),
            b: Bools::new(),
            masks: Map::default(),
            other: Map::default(),
            scc_lane: Map::default(),
            joins: Vec::new(),
            first: vec![Bool::FALSE; blocks.len()],
            when: vec![None; blocks.len()],
            active: HashSet::new(),
        })
    }

    pub(crate) fn reset(&mut self) {
        self.b = Bools::new();
        self.masks.clear();
        self.other.clear();
        self.scc_lane.clear();
        self.joins.clear();
        self.first.fill(Bool::FALSE);
        self.when.fill(None);
        self.active.clear();
    }

    /// Lane masks, branch conditions and forward-edge knowledge, block by
    /// block in reverse postorder; returns EXEC at each instruction reading it.
    pub(crate) fn masks_in_order(&mut self) -> Vec<Vec<Option<Bool>>> {
        let p = self.p;
        let input = |reg| p.inputs.iter().find(|i| i.0 == reg).map(|i| i.1);
        if let Some(lo) = input(Reg::S(EXEC)) {
            let hi = if p.wave64 {
                input(Reg::S(EXEC + 1)).unwrap_or(lo)
            } else {
                lo
            };
            self.masks.insert((lo, hi), Bool::TRUE);
            self.active.insert(lo);
        }
        let mut execs: Vec<Vec<Option<Bool>>> =
            p.blocks.iter().map(|b| vec![None; b.insts.len()]).collect();
        for n in 0..self.order.len() {
            let b = self.order[n];
            let mut c = if n == 0 { Bool::TRUE } else { Bool::FALSE };
            for k in 0..self.incoming[b].len() {
                let (q, taken) = self.incoming[b][k];
                if self.index[q] < n {
                    let e = self.edge(self.first[q], q, taken);
                    c = self.b.or(c, e);
                }
            }
            self.first[b] = c;
            // EXEC joined at the block's entry holds the lanes that arrive.
            for phi in &p.blocks[b].phis {
                if phi.reg == Reg::S(EXEC) {
                    self.active.insert(phi.value);
                }
            }
            // EXEC where known in this block (read or written so far).
            let mut now = None;
            for (i, inst) in p.blocks[b].insts.iter().enumerate() {
                if inst.read(Reg::S(EXEC)).is_some() {
                    let e = self.exec(inst);
                    execs[b][i] = Some(e);
                    now = Some(e);
                }
                self.step(inst, now);
                if let Some(k) = self.key(|r| inst.write(r), EXEC) {
                    now = self.masks.get(&k).or(self.other.get(&k)).copied();
                }
            }
            if let Exit::Branch { .. } = p.blocks[b].exit
                && let Some(last) = p.blocks[b].insts.last()
            {
                self.when[b] = Some(self.branch(last));
            }
        }
        execs
    }

    /// `c` narrowed by the branch at the end of `q` towards one successor.
    pub(crate) fn edge(&mut self, c: Bool, q: usize, taken: bool) -> Bool {
        match self.when[q] {
            Some(w) if taken => self.b.and(c, w),
            Some(w) => {
                let nw = self.b.not(w);
                self.b.and(c, nw)
            }
            None => c,
        }
    }

    /// Joins whose constant fails on some edge, given what lanes know.
    pub(crate) fn wrong_joins(&mut self, cond: &[Bool]) -> Vec<Value> {
        let mut wrong = Vec::new();
        let mut j = 0;
        // Checking an argument can resolve another join; it is checked too.
        while j < self.joins.len() {
            let (block, id, value) = (self.joins[j].block, self.joins[j].id, self.joins[j].value);
            for (n, a) in self.joins[j].args.clone().into_iter().enumerate() {
                let (q, taken) = self.incoming[block][n];
                let e = self.edge(cond[q], q, taken);
                let mut m = self.mask(a).0;
                if !value {
                    m = self.b.not(m);
                }
                if !self.b.implies(e, m) {
                    wrong.push(id);
                    break;
                }
            }
            j += 1;
        }
        wrong
    }

    pub(crate) fn finish(
        mut self,
        execs: &[Vec<Option<Bool>>],
        cond: &[Bool],
    ) -> Result<Structured, ShaderError> {
        let blocks = &self.p.blocks;
        let b = &mut self.b;
        let guards = blocks
            .iter()
            .enumerate()
            .map(|(n, block)| {
                block
                    .insts
                    .iter()
                    .zip(&execs[n])
                    .map(|(inst, exec)| match (exec, &inst.fields) {
                        (None, _)
                        | (_, Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. }) => {
                            None
                        }
                        (&Some(e), _) if b.implies(cond[n], e) => None,
                        (&Some(e), _) => Some(b.given(e, cond[n])),
                    })
                    .collect()
            })
            .collect();
        let exits = blocks
            .iter()
            .enumerate()
            .map(|(n, block)| match block.exit {
                Exit::Jump(t) => LaneExit::Jump(t),
                Exit::End => LaneExit::End,
                Exit::Indirect => LaneExit::Indirect,
                Exit::Branch { taken, not_taken } => LaneExit::Branch {
                    when: b.given(self.when[n].unwrap_or(Bool::FALSE), cond[n]),
                    taken,
                    not_taken,
                },
            })
            .collect();
        if self.b.overflow {
            return Err(TOO_LARGE);
        }
        Ok(Structured {
            masks: self.masks.iter().map(|(&(lo, _), &m)| (lo, m)).collect(),
            highs: self
                .masks
                .keys()
                .filter(|(lo, hi)| lo != hi)
                .map(|&(lo, hi)| (hi, lo))
                .collect(),
            paired_masks: if self.p.wave64 { self.masks } else { Map::default() },
            active: self.active,
            bools: self.b,
            exits,
            guards,
            reach: cond.to_vec(),
        })
    }

    pub(crate) fn key(&self, value: impl Fn(Reg) -> Option<Value>, code: u8) -> Option<Key> {
        let lo = value(Reg::S(code))?;
        let hi = if self.p.wave64 {
            value(Reg::S(code + 1))?
        } else {
            lo
        };
        Some((lo, hi))
    }

    pub(crate) fn born(&self, v: Value) -> usize {
        match (self.defs.get(&v), self.phis.get(&v)) {
            (Some(&(b, _)), _) | (_, Some(&(b, _))) => self.index[b],
            _ => 0,
        }
    }

    pub(crate) fn exec(&mut self, inst: &Inst) -> Bool {
        match self.key(|r| inst.read(r), EXEC) {
            Some(k) => self.mask(k).0,
            None => Bool::TRUE,
        }
    }

    /// The mask a value pair holds, and whether it derives from EXEC or a
    /// VALU result.
    pub(crate) fn mask(&mut self, k: Key) -> (Bool, bool) {
        if let Some(&m) = self.masks.get(&k) {
            return (m, true);
        }
        if let Some(&m) = self.other.get(&k) {
            return (m, false);
        }
        let p = self.p;
        let (m, derived) = match self.join(k) {
            Some(join) => join,
            None => {
                let (lo, hi) = (self.defs.get(&k.0).copied(), self.defs.get(&k.1).copied());
                let def = |d: Option<(usize, usize)>| d.map(|(b, i)| &p.blocks[b].insts[i]);
                let m = match (lo, hi) {
                    (Some(at), Some(hi)) if at == hi => def(lo)
                        .and_then(|i| self.sop(i))
                        .and_then(|r| r.dst)
                        .map(|d| d.1),
                    _ => None,
                };
                // What an EXEC write the pass does not follow leaves there.
                let exec_write = |d: Option<(usize, usize)>, v: Value| {
                    def(d).is_some_and(|i| {
                        i.write(Reg::S(EXEC)) == Some(v) || i.write(Reg::S(EXEC + 1)) == Some(v)
                    })
                };
                let derived = exec_write(lo, k.0) || exec_write(hi, k.1);
                let m = m.unwrap_or_else(|| {
                    let born = self.born(k.0);
                    self.b.atom(Atom::Opaque(k.0), born)
                });
                (m, derived)
            }
        };
        let cache = if derived {
            &mut self.masks
        } else {
            &mut self.other
        };
        cache.insert(k, m);
        (m, derived)
    }
}
