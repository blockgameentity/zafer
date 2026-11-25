//! Checking a module with `spirv-val`, the Khronos validator.
//!
//! The DXIL side has `dxil.dll` for this (`victory_shader::emit::validator`);
//! SPIR-V's is `spirv-val`, the tool SPIRV-Tools installs, and Vulkan's own
//! rules are the ones it checks. A module we emit is only known to be a
//! module because something outside this repository says so, so the batch
//! (`victory translate --spirv --validate`) and the tests run it.
//!
//! The tool is found at `VICTORY_SPIRV_VAL` if set, else on `PATH`. A
//! machine without it validates nothing rather than failing: the modules are
//! still emitted, and `Victory` logs once that it could not check them.

use std::io::Write;
use std::process::{Command, Stdio};

/// What the validator found in a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validity {
    Valid,
    /// The validator's own message, one line per finding.
    Invalid(String),
    /// No `spirv-val` on this machine, so nothing was checked.
    Unavailable,
}

impl Validity {
    /// The finding, if the module is invalid.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Validity::Invalid(why) => Some(why),
            _ => None,
        }
    }
}

/// Runs `spirv-val` over `spirv` as Vulkan 1.1 takes it.
///
/// The module is written to the tool's standard input, so nothing is left
/// on disk and a read-only cache directory still validates.
pub fn validate(spirv: &[u8]) -> Validity {
    let tool = std::env::var_os("VICTORY_SPIRV_VAL").unwrap_or_else(|| "spirv-val".into());
    let child = Command::new(&tool)
        .arg("--target-env")
        .arg("vulkan1.1")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        // Not installed: an answer, not a failure.
        Err(_) => return Validity::Unavailable,
    };
    // The tool reads all of its input before it answers, so writing the
    // module and then closing the pipe cannot deadlock on a full buffer:
    // a module is far smaller than one pipe's capacity.
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(spirv).is_err() {
            return Validity::Unavailable;
        }
    }
    match child.wait_with_output() {
        Ok(out) if out.status.success() => Validity::Valid,
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&out.stdout).into_owned();
            }
            if let Some(dir) = std::env::var_os("VICTORY_SPIRV_DUMP") {
                let at = format!(
                    "{}/invalid.spv",
                    std::path::Path::new(&dir).display()
                );
                let _ = std::fs::write(at, spirv);
            }
            Validity::Invalid(text.trim().to_owned())
        }
        Err(_) => Validity::Unavailable,
    }
}
