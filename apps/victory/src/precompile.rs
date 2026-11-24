//! The shader pre-compiler: translate every shader found and report,
//! without a GPU and without the title.
//!
//! `translate` tallies what the translator cannot handle; this command is
//! the CI-shaped version of the same walk. It exits non-zero on the first
//! failing shader with `--fail-fast`, or after the walk with
//! `--exit-code` (the default), and writes a JSON report `--report` can
//! keep. The point is to catch shader errors in seconds rather than at the
//! third minute of a world load.
//!
//! The walk itself is `translate`'s: same sources (`shader::sources`), same
//! pipeline (`ir` -> `structure` -> `resource`/`io` -> `emit`), same
//! validator when asked. The difference is the output: a report and an
//! exit code, not a human tally.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::Result;
use victory_shader::emit::{self, validator::Validator};
use victory_shader::{
    Container, FourCC, HwStage, ShaderError, io, ir, native_programs, resource, structure,
};

use crate::shader::{MAX_FILE, Source, sources};

/// One shader program's outcome.
#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    /// `path` plus the source's own label within it.
    pub(crate) at: String,
    /// The DXIL part's BLAKE3, so a report can be diffed across runs.
    pub(crate) hash: String,
    pub(crate) stage: &'static str,
    /// `None` on success, else the same reason `translate` would print.
    pub(crate) reason: Option<String>,
}

impl Outcome {
    fn ok(&self) -> bool {
        self.reason.is_none()
    }
}

/// The whole run's result: what `--report` writes and the exit code reads.
#[derive(Clone, Debug, Default)]
pub(crate) struct Report {
    pub(crate) outcomes: Vec<Outcome>,
    pub(crate) containers: usize,
    pub(crate) duplicates: usize,
}

impl Report {
    pub(crate) fn ok(&self) -> usize {
        self.outcomes.iter().filter(|o| o.ok()).count()
    }

    pub(crate) fn failed(&self) -> usize {
        self.outcomes.len() - self.ok()
    }

    /// Failures by reason, commonest first.
    pub(crate) fn reasons(&self) -> Vec<(String, usize, String)> {
        let mut counts: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
        for o in self.outcomes.iter().filter(|o| !o.ok()) {
            let reason = o.reason.as_deref().unwrap_or_default();
            counts
                .entry(reason)
                .or_insert((0, o.at.as_str()))
                .0 += 1;
        }
        let mut v: Vec<(String, usize, String)> = counts
            .into_iter()
            .map(|(reason, (n, first))| (reason.to_owned(), n, first.to_owned()))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Per-stage counts of (ok, failed).
    pub(crate) fn stages(&self) -> BTreeMap<&'static str, (usize, usize)> {
        let mut map: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
        for o in &self.outcomes {
            let e = map.entry(o.stage).or_default();
            if o.ok() {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        map
    }

    /// The report as JSON (hand-rolled: one object per outcome, then the
    /// summary; the repository adds no dependency for this).
    pub(crate) fn json(&self) -> String {
        let mut s = String::from("{\n  \"summary\": {");
        s.push_str(&format!(
            "\n    \"containers\": {}, \"duplicates\": {}, \"programs\": {}, \"ok\": {}, \"failed\": {}",
            self.containers,
            self.duplicates,
            self.outcomes.len(),
            self.ok(),
            self.failed()
        ));
        s.push_str(",\n    \"stages\": {");
        let stages: Vec<String> = self
            .stages()
            .iter()
            .map(|(stage, (ok, bad))| format!("\"{stage}\": {{\"ok\": {ok}, \"failed\": {bad}}}"))
            .collect();
        s.push_str(&stages.join(", "));
        s.push_str("},\n    \"reasons\": {");
        let reasons: Vec<String> = self
            .reasons()
            .iter()
            .map(|(reason, n, _)| format!("{}: {n}", quote(reason)))
            .collect();
        s.push_str(&reasons.join(", "));
        s.push_str("\n  },\n  \"programs\": [");
        for (i, o) in self.outcomes.iter().enumerate() {
            s.push_str(if i == 0 { "\n" } else { ",\n" });
            s.push_str(&format!(
                "    {{\"at\": {}, \"hash\": \"{}\", \"stage\": \"{}\", \"ok\": {}{}}}",
                quote(&o.at),
                o.hash,
                o.stage,
                o.ok(),
                o.reason
                    .as_ref()
                    .map_or(String::new(), |r| format!(", \"reason\": {}", quote(r)))
            ));
        }
        s.push_str("\n  ]\n}\n");
        s
    }
}

/// A JSON string with the escapes the format requires.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One shader program, from its container bytes to an outcome.
///
/// This mirrors `translate`'s pipeline step for step; when that one grows a
/// stage, this one must too (they are deliberately not shared, because
/// `translate` tallies as it goes and this only needs a verdict).
fn translate_one(
    container: &Container<'_>,
    p: &victory_shader::NativeProgram,
) -> Option<String> {
    let fail = |e: ShaderError| {
        let reason = match e {
            ShaderError::Unsupported { what, .. } => what.to_owned(),
            e => e.to_string(),
        };
        Some(reason)
    };
    let program = match ir::build(&p.patched_code(), p.wave64, p.vgprs) {
        Ok(program) => program,
        Err(e) => return fail(e),
    };
    let structured = match structure::structure(&program) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let res = match resource::track(&program, &p.layout, p.stage, Some(&structured)) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let raised = match io::raise(&program, p) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    match emit::emit(container, p, &program, &structured, &raised, &res) {
        Ok(_) => None,
        Err(e) => fail(e),
    }
}

/// Every program of one container.
fn container_outcomes(
    source: &Source<'_>,
    path: &Path,
    seen: &Mutex<HashSet<blake3::Hash>>,
    report: &mut Report,
    validate: bool,
    validator: &Mutex<Option<Validator>>,
    fail_fast: bool,
    any_failed: &AtomicBool,
) {
    let at = format!("{}{}", path.display(), source.label());
    let Some(bytes) = source.bytes() else {
        report.outcomes.push(Outcome {
            at,
            hash: String::new(),
            stage: "?",
            reason: Some("corrupt record".into()),
        });
        any_failed.store(true, Ordering::Relaxed);
        return;
    };
    report.containers += 1;
    let Ok(container) = Container::parse(&bytes) else {
        report.outcomes.push(Outcome {
            at,
            hash: String::new(),
            stage: "?",
            reason: Some("no DXIL part (SM5 or older)".into()),
        });
        any_failed.store(true, Ordering::Relaxed);
        return;
    };
    let Some(dxil) = container.part(FourCC::DXIL).map(|p| p.data) else {
        report.outcomes.push(Outcome {
            at,
            hash: String::new(),
            stage: "?",
            reason: Some("no DXIL part (SM5 or older)".into()),
        });
        any_failed.store(true, Ordering::Relaxed);
        return;
    };
    let hash = blake3::hash(dxil).to_hex().to_string();
    if !crate::precompile::lock(seen).insert(blake3::hash(dxil)) {
        report.duplicates += 1;
        return;
    }
    let programs = match native_programs(dxil) {
        Ok(p) => p,
        Err(e) => {
            report.outcomes.push(Outcome {
                at,
                hash,
                stage: "?",
                reason: Some(e.to_string()),
            });
            any_failed.store(true, Ordering::Relaxed);
            return;
        }
    };
    for p in programs {
        let stage = stage_name(p.stage);
        let mut reason = translate_one(&container, &p);
        // The validator only sees emitted modules, so it runs on a second
        // translation: cheap next to the world load this replaces.
        if reason.is_none() && validate {
            if let Some(text) = validate_one(&container, &p, validator) {
                reason = Some(format!("validator: {text}"));
            }
        }
        if reason.is_some() {
            any_failed.store(true, Ordering::Relaxed);
        }
        report.outcomes.push(Outcome {
            at: at.clone(),
            hash: hash.clone(),
            stage,
            reason,
        });
        if fail_fast && any_failed.load(Ordering::Relaxed) {
            return;
        }
    }
}

fn validate_one(
    container: &Container<'_>,
    p: &victory_shader::NativeProgram,
    validator: &Mutex<Option<Validator>>,
) -> Option<String> {
    let program = ir::build(&p.patched_code(), p.wave64, p.vgprs).ok()?;
    let structured = structure::structure(&program).ok()?;
    let res = resource::track(&program, &p.layout, p.stage, Some(&structured)).ok()?;
    let raised = io::raise(&program, p).ok()?;
    let mut shader = emit::emit(container, p, &program, &structured, &raised, &res).ok()?;
    let mut guard = validator.lock().unwrap_or_else(|e| e.into_inner());
    let v = guard.as_mut()?;
    v.validate(&mut shader.dxil).err()
}

fn stage_name(stage: HwStage) -> &'static str {
    match stage {
        HwStage::Ls => "ls",
        HwStage::Hs => "hs",
        HwStage::Es => "es",
        HwStage::Gs => "gs",
        HwStage::Vs => "vs",
        HwStage::Ps => "ps",
        HwStage::Cs => "cs",
        HwStage::Ms => "ms",
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
/// `victory precompile`: the walk, the report, the exit code.
pub fn run(
    paths: Vec<PathBuf>,
    report_path: Option<PathBuf>,
    validate: bool,
    jobs: Option<usize>,
    fail_fast: bool,
    exit_code: bool,
    list: bool,
) -> Result<()> {
    let paths = crate::default_paths(paths)?;
    if validate {
        let dll = Validator::default_path();
        Validator::load(dll.as_deref()).map_err(anyhow::Error::msg)?;
        println!(
            "validator    {}",
            dll.as_ref()
                .map_or("dxil.dll".into(), |p| p.display().to_string())
        );
    }
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let seen = Mutex::new(HashSet::new());
    let validator = Mutex::new(Validator::load(Validator::default_path().as_deref()).ok());
    let any_failed = AtomicBool::new(false);
    let outcomes = Mutex::new(Vec::new());
    let containers = AtomicUsize::new(0);
    let duplicates = AtomicUsize::new(0);
    let started = std::time::Instant::now();

    for root in &paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() || entry.metadata().map_or(0, |m| m.len()) > MAX_FILE {
                continue;
            }
            let Ok(data) = std::fs::read(entry.path()) else {
                continue;
            };
            let jobs = sources(&data);
            let next = AtomicUsize::new(0);
            std::thread::scope(|s| {
                for _ in 0..threads.min(jobs.len()) {
                    s.spawn(|| {
                        let mut mine = Report::default();
                        while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                            container_outcomes(
                                job,
                                entry.path(),
                                &seen,
                                &mut mine,
                                validate,
                                &validator,
                                fail_fast,
                                &any_failed,
                            );
                            if fail_fast && any_failed.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        containers.fetch_add(mine.containers, Ordering::Relaxed);
                        duplicates.fetch_add(mine.duplicates, Ordering::Relaxed);
                        lock(&outcomes).extend(mine.outcomes);
                    });
                }
            });
            if fail_fast && any_failed.load(Ordering::Relaxed) {
                break;
            }
        }
        if fail_fast && any_failed.load(Ordering::Relaxed) {
            break;
        }
    }

    let mut report = Report {
        outcomes: std::mem::take(&mut *lock(&outcomes)),
        containers: containers.into_inner(),
        duplicates: duplicates.into_inner(),
    };
    report.outcomes.sort_by(|a, b| a.at.cmp(&b.at));

    let failed = report.failed();
    println!(
        "precompiled  {} programs from {} containers ({} duplicates) in {:.1} s",
        report.outcomes.len(),
        report.containers,
        report.duplicates,
        started.elapsed().as_secs_f64()
    );
    for (stage, (ok, bad)) in report.stages() {
        println!("  {stage:<2}  {ok} ok, {bad} failed");
    }
    for (reason, n, first) in report.reasons() {
        println!("  {n:>6}  {reason}  (e.g. {first})");
    }
    if list {
        for o in &report.outcomes {
            match &o.reason {
                None => println!("{}  {}  ok", o.at, o.stage),
                Some(r) => println!("{}  {}  {r}", o.at, o.stage),
            }
        }
    }
    if let Some(path) = report_path {
        std::fs::write(&path, report.json())
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
        println!("report       {}", path.display());
    }
    anyhow::ensure!(
        !(exit_code && failed > 0),
        "{failed} shader program(s) failed to precompile"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(at: &str, stage: &'static str, reason: Option<&str>) -> Outcome {
        Outcome {
            at: at.into(),
            hash: "abc".into(),
            stage,
            reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn counts_split_by_stage_and_outcome() {
        let report = Report {
            outcomes: vec![
                outcome("a", "vs", None),
                outcome("b", "vs", Some("not supported")),
                outcome("c", "cs", None),
            ],
            containers: 2,
            duplicates: 1,
        };
        assert_eq!((report.ok(), report.failed()), (2, 1));
        let stages = report.stages();
        assert_eq!(stages["vs"], (1, 1));
        assert_eq!(stages["cs"], (1, 0));
    }

    #[test]
    fn reasons_are_grouped_commonest_first_with_a_first_example() {
        let report = Report {
            outcomes: vec![
                outcome("a", "vs", Some("rare")),
                outcome("b", "cs", Some("common")),
                outcome("c", "cs", Some("common")),
                outcome("d", "cs", Some("common")),
            ],
            ..Report::default()
        };
        let reasons = report.reasons();
        assert_eq!(reasons[0], ("common".to_owned(), 3, "b".to_owned()));
        assert_eq!(reasons[1], ("rare".to_owned(), 1, "a".to_owned()));
    }

    #[test]
    fn the_report_is_json_with_escaped_strings() {
        let report = Report {
            outcomes: vec![
                outcome("a\", \n", "ps", None),
                outcome("b", "ms", Some("a \"quoted\" reason")),
            ],
            containers: 3,
            duplicates: 0,
        };
        let json = report.json();
        assert!(json.starts_with("{\n  \"summary\": {"));
        assert!(json.contains("\"containers\": 3"));
        assert!(json.contains("\"ps\": {\"ok\": 1, \"failed\": 0}"));
        assert!(json.contains("\"ms\": {\"ok\": 0, \"failed\": 1}"));
        assert!(json.contains("\"a\\\", \\n\""));
        assert!(json.contains("a \\\"quoted\\\" reason"));
        // Every program is one object, in order, with its stage and verdict.
        assert_eq!(json.matches("\"stage\": ").count(), 2);
        assert!(json.contains("\"programs\": [\n    {\"at\":"));
    }

    #[test]
    fn an_empty_run_is_a_success() {
        let report = Report::default();
        assert_eq!((report.ok(), report.failed()), (0, 0));
        assert!(!report.json().contains("\"reason\""));
    }
}
