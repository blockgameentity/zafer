//! What a corpus of console shaders translates to, and what stops it.
//!
//! The DXIL emitter's acceptance test was "translate every shader in a real
//! title and tally the failures". This is the same tally for the SPIR-V
//! emitter, so the remaining work is a list of mnemonics with counts rather
//! than an opinion — and so a title that regresses is visible the moment it
//! does.
//!
//! It is a *library* rather than part of the CLI because the CLI is not the
//! only caller: a test over a checked-in corpus uses it directly.

use std::collections::BTreeMap;

use victory_shader::native::NativeProgram;

use crate::driver::translate_program;
use crate::Error;

/// How one translation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A SPIR-V module was emitted.
    Emitted,
    /// The emitter has no SPIR-V form for an instruction, by mnemonic.
    Unsupported(String),
    /// A stage before the emitter failed.
    Earlier(String),
}

/// What a run over many programs found.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// How many programs were offered.
    pub programs: usize,
    /// How many emitted a module.
    pub emitted: usize,
    /// Every mnemonic that stopped a program, with how many programs it
    /// stopped. Sorted, because this is read by a human deciding what to
    /// implement next.
    pub gaps: BTreeMap<String, usize>,
    /// Failures from the stages before the emitter, deduplicated the same
    /// way.
    pub earlier: BTreeMap<String, usize>,
}

impl Report {
    /// Translates each program and records the outcome.
    pub fn run(&mut self, programs: &[NativeProgram]) {
        for program in programs {
            self.programs += 1;
            match translate_program(program) {
                Ok(_) => self.emitted += 1,
                Err(Error::Unsupported { what, .. }) => {
                    *self.gaps.entry(what.to_owned()).or_default() += 1;
                }
                Err(Error::Shader(e)) => {
                    let key = match e {
                        victory_shader::ShaderError::Unsupported { what, .. } => what.to_owned(),
                        other => other.to_string(),
                    };
                    *self.earlier.entry(key).or_default() += 1;
                }
            }
        }
    }

    /// Folds another thread's tally into this one.
    ///
    /// The corpus is walked in parallel, so each worker keeps its own report
    /// and they are summed at the end; the gap counts add and the program
    /// counts add, which is what makes the merge exact rather than an
    /// approximation.
    pub fn merge(&mut self, other: Report) {
        self.programs += other.programs;
        self.emitted += other.emitted;
        for (k, n) in other.gaps {
            *self.gaps.entry(k).or_default() += n;
        }
        for (k, n) in other.earlier {
            *self.earlier.entry(k).or_default() += n;
        }
    }

    /// The share of programs that emitted a module, `0.0` when none ran.
    pub fn coverage(&self) -> f32 {
        if self.programs == 0 {
            return 0.0;
        }
        self.emitted as f32 / self.programs as f32
    }

    /// The gaps as `mnemonic: count` lines, most frequent first — the order
    /// worth fixing things in.
    pub fn ranked(&self) -> Vec<(String, usize)> {
        let mut ranked: Vec<(String, usize)> =
            self.gaps.iter().map(|(k, n)| (k.clone(), *n)).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked
    }
}

/// Translates one program and names its outcome, for a single file.
pub fn outcome(program: &NativeProgram) -> Outcome {
    match translate_program(program) {
        Ok(_) => Outcome::Emitted,
        Err(Error::Unsupported { what, offset }) => {
            Outcome::Unsupported(format!("{what} at {offset:#x}"))
        }
        Err(Error::Shader(e)) => Outcome::Earlier(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_report_has_no_coverage() {
        let report = Report::default();
        assert_eq!(report.coverage(), 0.0);
        assert!(report.ranked().is_empty());
    }

    #[test]
    fn ranked_puts_the_most_common_gap_first() {
        let mut report = Report::default();
        report.gaps.insert("v_mul_f32".to_owned(), 2);
        report.gaps.insert("v_div_f32".to_owned(), 9);
        report.gaps.insert("v_exp_f32".to_owned(), 2);
        assert_eq!(
            report.ranked(),
            vec![
                ("v_div_f32".to_owned(), 9),
                // Equal counts break by name, so the order is stable across
                // runs and a diff of two reports is meaningful.
                ("v_exp_f32".to_owned(), 2),
                ("v_mul_f32".to_owned(), 2),
            ]
        );
    }
}
