//! Lane conditions: structurisation's boolean decision diagrams as SPIR-V
//! booleans.
//!
//! `victory_shader::structure` reduces every per-lane condition of a
//! program to a BDD over [`Atom`]s: a compare's result, a bit of EXEC, an
//! opaque mask, a constant. Each node is `atom ? high : low`, which is
//! exactly an `OpSelect` on the atom's boolean, so a diagram of any size
//! becomes one expression per node. Nodes are memoised, so a condition two
//! instructions share is emitted once.

use std::collections::HashMap;

use victory_shader::structure::{Atom, Bool as Diagram};
use victory_spirv::builder::Builder;
use victory_spirv::module::Id;

use super::Emitter;

impl<'a> Emitter<'a> {
    /// The SPIR-V boolean a diagram denotes, or `None` for one whose atoms
    /// have no lane value.
    pub(crate) fn lane_bit(&mut self, b: &mut Builder, diagram: Diagram) -> Option<Id> {
        self.diagrams.entry(diagram).or_insert(None);
        self.lane_bit_inner(b, diagram)
    }

    fn lane_bit_inner(&mut self, b: &mut Builder, diagram: Diagram) -> Option<Id> {
        if let Some(&known) = self.diagrams.get(&diagram) {
            if known.is_some() {
                return known;
            }
        }
        let atom_at = self.st.bools.node(diagram);
        let result = match atom_at {
            // A constant: SPIR-V has a boolean constant for it. `Bool`
            // indexes the diagram's constants as 0 (false) and 1 (true).
            None => {
                let value = diagram == Diagram::TRUE;
                Some(b.constant_bool(value))
            }
            Some((atom, low, high)) => {
                let condition = self.atom(b, atom)?;
                let when_true = self.lane_bit_inner(b, high)?;
                let when_false = self.lane_bit_inner(b, low)?;
                let bool_ = self.types.bool_;
                Some(b.select(bool_, condition, when_true, when_false))
            }
        };
        self.diagrams.insert(diagram, result);
        result
    }

    /// The SPIR-V boolean an atom is.
    fn atom(&mut self, b: &mut Builder, atom: Atom) -> Option<Id> {
        match atom {
            // A compare's or a carry's lane bit: the compare wrote its
            // per-lane condition to the mask, and the dword the IR recorded
            // holds that condition as 0 or 1, so a nonzero test is the bit.
            Atom::Lane(value) | Atom::Phi(value) | Atom::Opaque(value) => self.load_bit(b, value),
            // A uniform value: SCC is a bool already; any other dword is
            // uniform, so its nonzero test is too.
            Atom::Uniform(value) => self.load_bit(b, value),
            // `s_wqm` widens to whole quads, which only a pixel shader has;
            // treating it as its source is right per lane and wrong only at
            // a quad's boundary, so it is refused rather than approximated.
            Atom::Quad(_) => None,
            // A constant lane set: true when it holds every lane, which is
            // the only case a diagram over it reduces to a constant anyway.
            Atom::Constant(mask) => {
                let all = if self.ir.wave64 { u64::MAX } else { 0xFFFF_FFFF };
                Some(b.constant_bool(mask & !all == 0))
            }
        }
    }
}

/// The memo table's type, named here so the emitter's field reads plainly.
pub(crate) type Diagrams = HashMap<Diagram, Option<Id>>;
