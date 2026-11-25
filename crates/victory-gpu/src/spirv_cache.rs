//! Translated shaders on Vulkan: the SPIR-V modules the run actually binds.
//!
//! Under the Vulkan backend the shader a pipeline needs is a SPIR-V module,
//! and the translation that matters is `victory-spirv-emit`'s. This is the
//! same content-addressed cache as [`crate::shader_cache`] (same folder,
//! same build fingerprint) but keyed by a converter id of `shader.spirv`,
//! so a DXIL entry and a SPIR-V entry for one container never collide: the
//! two encoders disagree about what a translation is, and a run that
//! switched backends must not serve the other one's answer.
//!
//! A refusal is recorded the same way (`record_refusal`), so the warm-up
//! and the run agree about what each container costs.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use victory_pipeline::{Cache, Converter};
use victory_spirv_emit::Translation;

/// A cached translation as the cache stores it: the module and how many
/// bytes of it the pipeline must bind.
struct Entry {
    spirv: Vec<u8>,
}

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps translated SPIR-V in `dir` from now on.
///
/// The same folder [`crate::shader_cache::use_shader_cache`] opens: the
/// entries are told apart by the converter id, not by where they live.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no SPIR-V cache"),
    }
}

/// Modules found in the cache and translated since the start.
pub fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// The SPIR-V module for `container`, translated and cached on a miss.
///
/// This is what the Vulkan backend binds; the DXIL translation
/// ([`crate::shader_cache::host_shader`]) is not consulted, so a shader the
/// DXIL emitter cannot express is still runnable if the SPIR-V emitter can.
pub fn spirv_module(container: &[u8]) -> Result<Vec<u8>, String> {
    let translate = |c: &[u8]| victory_spirv_emit::translate(c).map(|t| Entry { spirv: t.spirv }).map_err(|e| e.to_string());
    match CACHE.get() {
        Some(cache) => cached(cache, build(), container, translate),
        None => translate(container).map(|e| e.spirv),
    }
}

/// The whole translation, for a pipeline that also needs what to bind.
pub fn translate(container: &[u8]) -> Result<Translation, String> {
    victory_spirv_emit::translate(container).map_err(|e| e.to_string())
}

/// The cache entry's converter: the build and the container in, the encoded
/// module out.
struct Encode<F> {
    translate: F,
    ran: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<Entry, String> + Send + Sync> Converter for Encode<F> {
    fn id(&self) -> &'static str {
        // Distinct from `shader.dxil`: the two encoders' entries are not
        // interchangeable, so they must not share a key.
        "shader.spirv"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let e = (self.translate)(&input[32..]).map_err(anyhow::Error::msg)?;
        // The module, prefixed by its length so a truncated entry is
        // recognisable rather than a shorter module.
        let mut out = (e.spirv.len() as u64).to_le_bytes().to_vec();
        out.extend_from_slice(&e.spirv);
        Ok(out)
    }
}

fn build() -> &'static [u8; 32] {
    crate::shader_cache::build()
}

/// A cache hit, or a translation that is then stored: the miss path, with
/// an unreadable entry removed and retried once (a half-written entry is
/// cheaper to redo than to report).
fn cached<F>(cache: &Cache, build: &[u8; 32], container: &[u8], translate: F) -> Result<Vec<u8>, String>
where
    F: Fn(&[u8]) -> Result<Entry, String> + Send + Sync,
{
    let input = [&build[..], container].concat();
    let converter = Encode {
        translate,
        ran: AtomicBool::new(false),
    };
    for _ in 0..2 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(e) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let (counter, what) = if converter.ran.load(Ordering::Relaxed) {
                (&MISSES, "translated and cached")
            } else {
                (&HITS, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "SPIR-V {what}");
            return Ok(e.spirv);
        }
        tracing::debug!(path = %path.display(), "unreadable SPIR-V entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container).map(|e| e.spirv)
}

fn decode(bytes: &[u8]) -> Option<Entry> {
    let (head, rest) = bytes.split_at_checked(8)?;
    let n = u64::from_le_bytes(head.try_into().ok()?) as usize;
    (rest.len() == n).then(|| Entry {
        spirv: rest.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_round_trips() {
        let e = decode(&{
            let mut v = 4u64.to_le_bytes().to_vec();
            v.extend_from_slice(&[1, 2, 3, 4]);
            v
        });
        assert_eq!(e.map(|e| e.spirv), Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn a_truncated_entry_is_refused() {
        let mut v = 8u64.to_le_bytes().to_vec();
        v.extend_from_slice(&[1, 2, 3, 4]);
        assert!(decode(&v).is_none(), "a short body is not a module");
        assert!(decode(&[1, 2]).is_none(), "nor is a short header");
    }
}
