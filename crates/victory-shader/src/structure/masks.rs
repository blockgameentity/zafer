use super::*;

impl<'a> Lanes<'a> {
    /// A mask joined by phis at a block entry, and whether a mask coming in
    /// on a forward edge derives from EXEC or a VALU result. Where every
    /// lane arriving so far has the incoming mask set (or clear), the join
    /// is that constant, for a lane can only read it after arriving.
    pub(crate) fn join(&mut self, k: Key) -> Option<(Bool, bool)> {
        let p = self.p;
        let (lo, hi) = (self.phis.get(&k.0).copied(), self.phis.get(&k.1).copied());
        let block = match (lo, hi) {
            (Some((a, _)), Some((b, _))) if a != b => return None,
            (Some((b, _)), _) | (_, Some((b, _))) => b,
            _ => return None,
        };
        let target = &p.blocks[block];
        let arg = |phi: Option<(usize, usize)>, own: Value, n: usize| {
            phi.map_or(own, |(_, i)| target.phis[i].args[n])
        };
        let args: Vec<Key> = (0..target.preds.len())
            .map(|n| (arg(lo, k.0, n), arg(hi, k.1, n)))
            .collect();
        let id = lo.map_or(k.1, |_| k.0);
        let mut derived = false;
        // (edge condition, mask) per forward edge; back edges come later.
        let mut forward = Vec::new();
        for (n, &a) in args.iter().enumerate() {
            let (q, taken) = self.incoming[block][n];
            if self.index[q] < self.index[block] {
                let (m, d) = self.mask(a);
                derived |= d;
                let e = self.edge(self.first[q], q, taken);
                forward.push((e, m));
            }
        }
        if forward.len() == args.len()
            && let Some(&(_, m)) = forward.first()
            && forward.iter().all(|f| f.1 == m)
        {
            return Some((m, derived));
        }
        if !self.unresolved.contains(&id) {
            for value in [true, false] {
                let b = &mut self.b;
                let holds = forward.iter().all(|&(e, m)| {
                    let m = if value { m } else { b.not(m) };
                    b.implies(e, m)
                });
                if holds {
                    self.joins.push(Join {
                        block,
                        id,
                        value,
                        args,
                    });
                    let m = if value { Bool::TRUE } else { Bool::FALSE };
                    return Some((m, derived));
                }
            }
        }
        let born = self.index[block];
        Some((self.b.atom(Atom::Phi(id), born), derived))
    }

    pub(crate) fn operand(&mut self, inst: &Inst, op: &Operand) -> Option<(Bool, bool)> {
        let wave64 = self.p.wave64;
        let constant = |b: &mut Bools, v: u64| {
            let v = if wave64 { v } else { v & 0xFFFF_FFFF };
            match v {
                0 => Bool::FALSE,
                _ if v == u64::MAX >> (32 * u8::from(!wave64)) => Bool::TRUE,
                _ => b.atom(Atom::Constant(v), 0),
            }
        };
        Some(match *op {
            Operand::Scalar { code, .. } if code != NULL => {
                let k = self.key(|r| inst.read(r), code)?;
                self.mask(k)
            }
            Operand::Const(c @ 128..=208) => {
                let v = if c <= 192 {
                    i64::from(c) - 128
                } else {
                    192 - i64::from(c)
                };
                (constant(&mut self.b, v as u64), false)
            }
            Operand::Literal(x) => (constant(&mut self.b, u64::from(x)), false),
            _ => return None,
        })
    }

    /// Lane-mask meaning of a lane-width scalar operation.
    pub(crate) fn sop(&mut self, inst: &Inst) -> Option<SopMasks> {
        let Fields::Sop { dst, src, .. } = &inst.fields else {
            return None;
        };
        let width = if self.p.wave64 { "_b64" } else { "_b32" };
        let op = inst.name().strip_prefix("s_")?.strip_suffix(width)?;
        let dst = match dst {
            Some(Operand::Scalar { code, .. }) if *code != NULL => {
                self.key(|r| inst.write(r), *code)
            }
            _ => None,
        };
        let exec_out = self.key(|r| inst.write(r), EXEC);
        let (save, f) = match (op.strip_suffix("_saveexec"), op.strip_suffix("_wrexec")) {
            (Some(f), _) => (Some(true), f),
            (_, Some(f)) => (Some(false), f),
            _ => (None, op),
        };
        let mut derived = exec_out.is_some() || save.is_some();
        let mut arg = |n: usize, l: &mut Self| {
            let (m, d) = l.operand(inst, src.get(n)?)?;
            derived |= d;
            Some(m)
        };
        if let Some(saveexec) = save {
            let s = arg(0, self)?;
            let exec = self.exec(inst);
            let new = self.combine(f, s, exec)?;
            let saved = if saveexec { exec } else { new };
            return Some(SopMasks {
                dst: dst.map(|d| (d, saved)),
                exec: exec_out.map(|e| (e, new)),
                derived,
                scc: Some(new),
            });
        }
        let (result, scc) = match op {
            "mov" => (arg(0, self)?, None),
            "not" => {
                let a = arg(0, self)?;
                let r = self.b.not(a);
                (r, Some(r))
            }
            "wqm" => {
                let a = arg(0, self)?;
                let lo = dst.or(exec_out)?.0;
                let born = self.born(lo);
                let q = self.b.atom(Atom::Quad(lo), born);
                let r = self.b.or(a, q);
                (r, Some(r))
            }
            "cselect" => {
                let (a, c) = (arg(0, self)?, arg(1, self)?);
                let scc = inst.read(Reg::Scc)?;
                let born = self.born(scc);
                let u = self.b.atom(Atom::Uniform(scc), born);
                (self.b.ite(u, a, c), None)
            }
            _ => {
                let (a, c) = (arg(0, self)?, arg(1, self)?);
                let r = self.combine(op, a, c)?;
                (r, Some(r))
            }
        };
        Some(SopMasks {
            dst: dst.map(|d| (d, result)),
            exec: exec_out.map(|e| (e, result)),
            derived,
            scc,
        })
    }

    pub(crate) fn combine(&mut self, f: &str, a: Bool, c: Bool) -> Option<Bool> {
        let b = &mut self.b;
        Some(match f {
            "and" => b.and(a, c),
            "or" => b.or(a, c),
            "xor" => b.xor(a, c),
            "andn2" => {
                let nc = b.not(c);
                b.and(a, nc)
            }
            "orn2" => {
                let nc = b.not(c);
                b.or(a, nc)
            }
            "andn1" => {
                let na = b.not(a);
                b.and(na, c)
            }
            "orn1" => {
                let na = b.not(a);
                b.or(na, c)
            }
            "nand" => {
                let r = b.and(a, c);
                b.not(r)
            }
            "nor" => {
                let r = b.or(a, c);
                b.not(r)
            }
            "xnor" => {
                let r = b.xor(a, c);
                b.not(r)
            }
            _ => return None,
        })
    }
}
