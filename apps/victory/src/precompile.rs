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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::Result;
use rayon::prelude::*;
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
    /// Folds one per-container result into this one (rayon's fold/reduce).
    pub(crate) fn merge(&mut self, other: Report) {
        self.outcomes.extend(other.outcomes);
        self.containers += other.containers;
        self.duplicates += other.duplicates;
    }

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
    data: &[u8],
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
    // Through the cache the run reads, once: `host_shader` translates and
    // stores, so a hit here is exactly the entry `run` will find, and a
    // refusal is recorded so the next boot does not pay for it again.
    match victory_gpu::shader_cache::host_shader(data) {
        Ok(_) => None,
        Err(e) => {
            victory_gpu::shader_cache::record_refusal(data, &e);
            Some(e)
        }
    }
}

/// Every program of one container.
fn container_outcomes(
    source: &Source<'_>,
    path: &Path,
    seen: &dashmap::DashSet<blake3::Hash>,
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
    if !seen.insert(blake3::hash(dxil)) {
        report.duplicates += 1;
        return;
    }
    // Already translated by an earlier boot: nothing to decode, nothing to
    // emit. This is what makes a warm boot cost a walk and no compilation.
    if victory_gpu::shader_cache::is_cached(&bytes) {
        report.outcomes.push(Outcome {
            at,
            hash,
            stage: "-",
            reason: None,
        });
        return;
    }
    // Refused by an earlier boot, for the reason recorded then: nothing to
    // decode, nothing to emit, and most importantly nothing to pay for.
    if let Some(why) = victory_gpu::shader_cache::refusal(&bytes) {
        report.outcomes.push(Outcome {
            at,
            hash,
            stage: "!",
            reason: Some(why),
        });
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
    // A refusal is cached too: without this every boot re-translates and
    // re-refuses the same containers, which is what made a "warm" boot as
    // long as a cold one. The reason is recorded, so the report still shows
    // it without paying for the translation again.
    for p in programs {
        let stage = stage_name(p.stage);
        let mut reason = translate_one(&bytes, &container, &p);
        // The validator only sees emitted modules, so it runs on a second
        // translation: cheap next to the world load this replaces.
        if reason.is_none() && validate {
            if let Some(text) = validate_one(&container, &p, validator) {
                reason = Some(format!("validator: {text}"));
            }
        }
        if let Some(why) = &reason {
            any_failed.store(true, Ordering::Relaxed);
            // Recorded once per container: the first program that explains
            // why the container cannot be translated is the reason.
            victory_gpu::shader_cache::record_refusal(&bytes, why);
            report.outcomes.push(Outcome {
                at: at.clone(),
                hash: hash.clone(),
                stage,
                reason,
            });
            return;
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
    let mut guard = validator.lock();
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

/// Every core the machine has, never a fixed 1 or 8: the walk and the pool
/// are both parallel and a smaller pool only lengthens the boot.
pub(crate) fn all_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

fn lock<T>(m: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    m.lock()
}
/// The boot-time warm-up: every shader in the title's own files goes through
/// the shader cache a run reads, by the same code, so that whatever the
/// title has not needed yet is translated (or refused) before the first
/// frame that would have needed it.
///
/// Only containers the cache does not hold are translated and only those
/// that fail are reported, so a boot after a clean one costs a walk and no
/// compilation. A failure here never stops the boot: the title translates
/// the same container itself on first use and drops the draw, which is
/// exactly what the run would have done without the warm-up.
///
/// `dir` is `<cache>/shaders` (decision 0003), the folder `run` reads.
pub fn warm_on_boot(dir: std::path::PathBuf, jobs: Option<usize>) -> Result<()> {
    victory_hle::modules::d3d12x::use_shader_cache(dir);
    // The warm-up runs on its own thread and this returns when it finishes or
    // when the budget expires, whichever comes first. It used to run inline,
    // which meant a cold cache over a 128 GB title kept the title from
    // starting at all: the process sat in a walk, "sleeping", and never
    // reached the first frame. A boot that starts and caches on first use is
    // always worth more than a warm-up that finishes.
    // Default: do not wait at all. The warm-up is background work; the title
    // starts immediately and the run caches whatever it needs on first use.
    // `VICTORY_WARM_BUDGET=<seconds>` waits that long (a harness that wants a
    // warm cache before the title), and `<0>` is impossible to parse, so only
    // an explicit number ever blocks the boot.
    let budget = std::env::var("VICTORY_WARM_BUDGET")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if budget == 0 {
        std::thread::spawn(move || {
            if let Err(e) = warm_everything(jobs) {
                tracing::warn!(error = %e, "shader precompiler: warm-up ended early");
            }
        });
        return Ok(());
    }
    let (done, finished) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = done.send(warm_everything(jobs));
    });
    match finished.recv_timeout(std::time::Duration::from_secs(budget)) {
        Ok(outcome) => outcome,
        Err(_) => {
            // Still working: leave it running. Everything it stores is
            // content-addressed and written atomically, so a warm-up cut
            // short simply leaves the rest to the run.
            tracing::info!(budget, "shader precompiler: budget reached; the run caches the rest");
            Ok(())
        }
    }
}

/// Warm every shader under the configured roots, however long that takes.
pub fn warm_everything(jobs: Option<usize>) -> Result<()> {
    victory_hle::modules::d3d12x::use_shader_cache(
        victory_core::Config::load()?.cache_subdir("shaders")?,
    );
    let cfg = victory_core::Config::load()?;
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(game) = cfg.game_dir.clone() {
        paths.push(game);
    }
    paths.extend(cfg.test_titles.clone());
    if paths.is_empty() {
        return Ok(());
    }
    // Always every core: precompiling is pure CPU work over a walk that
    // is itself parallel, so a smaller pool only makes the boot longer.
    // `jobs` is honoured only for tests and for reproducing a report.
    let threads = jobs.unwrap_or_else(crate::precompile::all_cores);
    let started = std::time::Instant::now();
    tracing::info!(
        threads,
        roots = paths.len(),
        "shader precompiler: warming every title shader not yet cached"
    );
    let seen = dashmap::DashSet::new();
    // Every container considered, cached or translated: the throughput the
    // closing line reports, and the number a slow boot is judged by.
    let considered = AtomicUsize::new(0);
    // Refused, translated now, already cached: the middle count says how
    // much work this boot saved the run.
    let counts = [const { AtomicUsize::new(0) }; 3];
    let failed = Mutex::new(Vec::new());
    // The precompiler's shape: one producer walking and reading, one pool
    // translating whatever it takes.
    // The walk is parallel (dua-core: the iterator `dua` itself uses), and it
    // sends *paths*, not file contents: reading 200 GB on the one producer
    // thread is what made a warm-up use one core instead of the machine. The
    // pool reads each file it takes.
    // crossbeam-channel: a cloned receiver per worker, so the pool never
    // contends on one mutex just to take the next path.
    // Unbounded: the producer walks at disk speed and the pool drains it;
    // a bounded queue would make the walk wait on the slowest shader.
    let (queue, incoming) = crossbeam_channel::unbounded::<PathBuf>();
    // The warm-up must never keep the title from starting. A cold cache over a
    // 128 GB title is minutes of work, and a booting title is always worth
    // more than a finished warm-up. `VICTORY_WARM_BUDGET` (seconds) bounds it;
    // `0` warms as long as it takes. Past the budget the rest is left to the
    // run, which caches on first use anyway.
    let budget = std::env::var("VICTORY_WARM_BUDGET")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30);
    let deadline = (budget > 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_secs(budget));
    let timed_out = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let roots: Vec<(usize, PathBuf)> = paths.iter().cloned().enumerate().collect();
            let walked = dua_core::walk_roots(
                roots,
                threads,
                dua_core::Order::Completion,
                dua_core::Options::default(),
                |_, _| true,
            );
            for (_, event) in walked {
                // The walk is what `thread::scope` actually waits for, so the
                // deadline has to be here too: without it the pool empties and
                // the scope still sits in the walk over a 128 GB tree, which is
                // the "sleeping" process that never reaches the title.
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    timed_out.store(true, Ordering::Relaxed);
                    break;
                }
                let dua_core::RootEvent::Entry(Ok(entry)) = event else {
                    continue;
                };
                if !entry.file_type.is_file()
                    || entry.metadata.map_or(false, |m| m.map_or(false, |m| m.len() > MAX_FILE))
                {
                    continue;
                }
                let full = entry.parent_path.join(&entry.file_name);
                tracing::trace!(path = %full.display(), "walk: file queued");
                if queue.send(full).is_err() {
                    return;
                }
            }
        });
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let next = incoming.recv();
                    let Ok(path) = next else { break };
                    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                        // Past the budget: take the path and drop it, so the
                        // pool empties and this scope can end.
                        timed_out.store(true, Ordering::Relaxed);
                        continue;
                    }
                    // Map, do not read: `fs::read` copies a whole file
                    // into a fresh heap Vec, which for a 2 GB archive is
                    // the single largest cost of a warm-up. The scan is
                    // read-only and the map dies with this pass.
                    let Ok(file) = std::fs::File::open(&path) else {
                        tracing::warn!(path = %path.display(), "walk: cannot open");
                        continue;
                    };
                    tracing::debug!(path = %path.display(), "walk: file taken");
                    let Ok(data) = (unsafe { memmap2::Mmap::map(&file) }) else {
                        continue;
                    };
                    let jobs = sources(&data);
                    let work = |job: &Source<'_>| -> (bool, bool) {
                        let Some(bytes) = job.bytes() else {
                            return (false, false);
                        };
                        // As the title passes it: the container's own size.
                        let size = bytes.get(0x18..0x1c).map_or(0, |b| {
                            u32::from_le_bytes(b.try_into().expect("4")) as usize
                        });
                        let container = &bytes[..size.min(bytes.len())];
                        let hash = blake3::hash(container);
                        if !seen.insert(hash) {
                            tracing::trace!(hash = %&hash.to_hex()[..16], "duplicate container");
                            return (false, true);
                        }
                        // `counts` tells a cache hit from a translation.
                        let before = victory_gpu::shader_cache::counts().0;
                        let one = std::time::Instant::now();
                        // Refused on an earlier boot: the reason is on disk,
                        // so this costs a hash lookup instead of a
                        // translation. Without this every boot re-translated
                        // and re-refused the same containers, which is what
                        // made a warm boot as slow as a cold one.
                        if let Some(why) = victory_gpu::shader_cache::refusal(container) {
                            counts[0].fetch_add(1, Ordering::Relaxed);
                            lock(&failed).push((why, path.clone()));
                            return (false, true);
                        }
                        let done = victory_gpu::shader_cache::host_shader(container);
                        let after = victory_gpu::shader_cache::counts().0;
                        let failed_here = match &done {
                            Ok(_) => {
                                let k = if after != before { 1 } else { 2 };
                                counts[k].fetch_add(1, Ordering::Relaxed);
                                false
                            }
                            Err(e) => {
                                counts[0].fetch_add(1, Ordering::Relaxed);
                                lock(&failed).push((e.clone(), path.clone()));
                                // Remembered, so the next boot skips it.
                                victory_gpu::shader_cache::record_refusal(container, &e);
                                tracing::warn!(
                                    path = %path.display(),
                                    hash = %&hash.to_hex()[..16],
                                    reason = %e,
                                    "shader refused during warm-up"
                                );
                                true
                            }
                        };
                        tracing::debug!(
                            hash = %&hash.to_hex()[..16],
                            bytes = container.len(),
                            ms = one.elapsed().as_millis() as u64,
                            ok = done.is_ok(),
                            "shader warm-up"
                        );
                        (true, failed_here)
                    };
                    // Inner parallelism: one archive often holds thousands of
                    // containers, so a per-file unit of work would leave it on
                    // one core.
                    jobs.par_iter().for_each(|job| {
                        let _ = work(job);
                    });
                }
            });
        }
    });
    let [refused, translated, cached] = counts.map(AtomicUsize::into_inner);
    let secs = started.elapsed().as_secs_f64();
    tracing::info!(
        translated,
        cached,
        refused,
        considered = considered.load(Ordering::Relaxed),
        secs,
        per_sec = (considered.load(Ordering::Relaxed) as f64 / secs.max(f64::EPSILON)) as u64,
        "shader warm-up complete"
    );
    println!(
        "shaders        {translated} translated, {cached} already cached, {refused} refused, in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    // Every distinct refusal once, commonest first: the point of the
    // warm-up is to name what the translator still cannot do.
    let failed = lock(&failed);
    if !failed.is_empty() {
        let mut reasons: BTreeMap<&str, (usize, String)> = BTreeMap::new();
        for (reason, path) in failed.iter() {
            let e = reasons.entry(reason.as_str()).or_insert((0, String::new()));
            if e.0 == 0 {
                e.1 = path.display().to_string();
            }
            e.0 += 1;
        }
        let mut v: Vec<_> = reasons.into_iter().collect();
        v.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
        for (reason, (n, first)) in v {
            println!("  {n:>7}  {reason}  (e.g. {first})");
        }
    }
    Ok(())
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
    // Open the shader cache the *run* reads, or this walk translates into
    // nothing and every boot pays for all of it again. That was the whole
    // point: compiled once, reused forever.
    let cache = victory_core::Config::load()?.cache_subdir("shaders")?;
    victory_gpu::shader_cache::use_shader_cache(cache.clone());
    tracing::info!(cache = %cache.display(), "shader cache opened");
    if validate {
        let dll = Validator::default_path();
        Validator::load(dll.as_deref()).map_err(anyhow::Error::msg)?;
        println!(
            "validator    {}",
            dll.as_ref()
                .map_or("dxil.dll".into(), |p| p.display().to_string())
        );
    }
    // Always every core: precompiling is pure CPU work over a walk that
    // is itself parallel, so a smaller pool only makes the boot longer.
    // `jobs` is honoured only for tests and for reproducing a report.
    let threads = jobs.unwrap_or_else(crate::precompile::all_cores);
    let seen = dashmap::DashSet::new();
    let validator = Mutex::new(Validator::load(Validator::default_path().as_deref()).ok());
    let any_failed = AtomicBool::new(false);
    let outcomes = Mutex::new(Vec::new());
    let containers = AtomicUsize::new(0);
    let duplicates = AtomicUsize::new(0);
    let started = std::time::Instant::now();
    tracing::info!(
        threads,
        roots = paths.len(),
        validate,
        "shader precompiler: translating every distinct container found"
    );

    // One queue for the whole walk, one pool working through it. The queue is
    // fed by a single producer (the directory walk, which is I/O-bound) and
    // every worker translates whatever it takes, so a file holding one shader
    // no longer pins one core for the length of that shader: the pool stays
    // as busy as there is work for. `pending` lets the producer stop reading
    // ahead when the workers are behind, so the walk's own memory stays
    // bounded by a fixed window rather than by the size of the title.
    // crossbeam-channel: a cloned receiver per worker, so the pool never
    // contends on one mutex just to take the next path.
    // Unbounded: the producer walks at disk speed and the pool drains it;
    // a bounded queue would make the walk wait on the slowest shader.
    let (queue, incoming) = crossbeam_channel::unbounded::<PathBuf>();
    // The warm-up must never keep the title from starting. A cold cache over a
    // 128 GB title is minutes of work, and a booting title is always worth
    // more than a finished warm-up. `VICTORY_WARM_BUDGET` (seconds) bounds it;
    // `0` warms as long as it takes. Past the budget the rest is left to the
    // run, which caches on first use anyway.
    let budget = std::env::var("VICTORY_WARM_BUDGET")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30);
    let deadline = (budget > 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_secs(budget));
    let timed_out = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let roots: Vec<(usize, PathBuf)> = paths.iter().cloned().enumerate().collect();
            let walked = dua_core::walk_roots(
                roots,
                threads,
                dua_core::Order::Completion,
                dua_core::Options::default(),
                |_, _| true,
            );
            for (_, event) in walked {
                if fail_fast && any_failed.load(Ordering::Relaxed) {
                    return;
                }
                let dua_core::RootEvent::Entry(Ok(entry)) = event else {
                    continue;
                };
                if !entry.file_type.is_file()
                    || entry.metadata.map_or(false, |m| m.map_or(false, |m| m.len() > MAX_FILE))
                {
                    continue;
                }
                let full = entry.parent_path.join(&entry.file_name);
                tracing::trace!(path = %full.display(), "walk: file queued");
                if queue.send(full).is_err() {
                    // Every worker is gone (a panic): nothing left to feed.
                    return;
                }
            }
        });
        for _ in 0..threads {
            s.spawn(|| {
                let mut mine = Report::default();
                loop {
                    if fail_fast && any_failed.load(Ordering::Relaxed) {
                        break;
                    }
                    // A worker that finds nothing yet waits on the producer.
                    let next = incoming.recv();
                    let Ok(path) = next else { break };
                    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                        // Past the budget: take the path and drop it, so the
                        // pool empties and this scope can end.
                        timed_out.store(true, Ordering::Relaxed);
                        continue;
                    }
                    let Ok(file) = std::fs::File::open(&path) else {
                        tracing::warn!(path = %path.display(), "walk: cannot open");
                        continue;
                    };
                    tracing::debug!(path = %path.display(), "walk: file taken");
                    let Ok(data) = (unsafe { memmap2::Mmap::map(&file) }) else {
                        continue;
                    };
                    // Inner parallelism too: one archive can hold thousands
                    // of containers, and making the *file* the unit of work
                    // left a single 400 MB archive on a single core. Each
                    // container folds its own Report, then the folds merge.
                    let jobs = sources(&data);
                    let per: Report = jobs
                        .par_iter()
                        .fold(
                            Report::default,
                            |mut mine, job| {
                                if fail_fast && any_failed.load(Ordering::Relaxed) {
                                    return mine;
                                }
                                container_outcomes(
                                    job,
                                    &path,
                                    &seen,
                                    &mut mine,
                                    validate,
                                    &validator,
                                    fail_fast,
                                    &any_failed,
                                );
                                mine
                            },
                        )
                        .reduce(Report::default, |mut a, b| {
                            a.merge(b);
                            a
                        });
                    mine.merge(per);
                }
                containers.fetch_add(mine.containers, Ordering::Relaxed);
                duplicates.fetch_add(mine.duplicates, Ordering::Relaxed);
                lock(&outcomes).extend(mine.outcomes);
            });
        }
    });

    let mut report = Report {
        outcomes: std::mem::take(&mut *lock(&outcomes)),
        containers: containers.into_inner(),
        duplicates: duplicates.into_inner(),
    };
    report.outcomes.sort_by(|a, b| a.at.cmp(&b.at));

    let failed = report.failed();
    tracing::info!(
        programs = report.outcomes.len(),
        containers = report.containers,
        duplicates = report.duplicates,
        failed,
        "precompile complete"
    );
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
