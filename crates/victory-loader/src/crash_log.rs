//! The crash log and the report format, shared by the Windows crash
//! reporter (`crash.rs`) and the Linux dispatcher (`linux::seh`).

/// Frames listed per report.
pub(crate) const MAX_FRAMES: usize = 40;

/// The file crash reports are appended to, besides being printed.
static LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Also appends every unhandled-exception report and panic to `file`, so a
/// crash can be read after the console is gone.
pub fn log_crashes_to(file: std::path::PathBuf) {
    if LOG.set(file).is_ok() {
        let print = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            print(info);
            append_to_log(&format!(
                "VIctory: panic on thread {:?}: {info}\n{}",
                std::thread::current().name().unwrap_or("?"),
                title_output()
            ));
        }));
    }
}

/// Appends a failure that is not an exception (the host GPU's) to the crash
/// log, with the title's last debug output, and prints it.
pub fn log_crash(text: &str) {
    let text = format!("{text}
{}", title_output());
    eprint!("{text}");
    append_to_log(&text);
}

fn append_to_log(text: &str) {
    use std::io::Write;
    let Some(file) = LOG.get() else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let opened = std::fs::OpenOptions::new().create(true).append(true).open(file);
    if let Ok(mut f) = opened {
        let _ = write!(f, "unix time {now}, process {}\n{text}\n", std::process::id());
    }
}

/// The title's last lines of debug output, kept for crash reports.
static SAID: std::sync::Mutex<std::collections::VecDeque<String>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
const SAID_LINES: usize = 8;

/// Keeps a line of the title's debug output for the next crash report.
pub fn note_title_output(line: &str) {
    if line.is_empty() {
        return;
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() == SAID_LINES {
        said.pop_front();
    }
    said.push_back(line.to_owned());
}

/// The kept lines, one per report line; nothing if another thread holds
/// them (it may be the one that faulted).
fn title_output() -> String {
    let Ok(said) = SAID.try_lock() else {
        return String::new();
    };
    said.iter().map(|l| format!("  title said: {l}\n")).collect()
}

/// MSVC's code for a C++ `throw`, and its magic first parameter.
pub(crate) const CPP_EXCEPTION: u32 = 0xE06D_7363;
pub(crate) const CPP_MAGIC: usize = 0x1993_0520;
const ACCESS_VIOLATION: u32 = 0xC000_0005;

/// What a C++ throw threw: the decorated name of its type, then its value
/// where that names the cause (an integer, e.g. an HRESULT; a standard
/// exception's message).
///
/// # Safety
/// `p` are the parameters of a live MSVC throw (`p[0]` the magic).
unsafe fn thrown(p: &[usize]) -> String {
    // SAFETY: the throw's ThrowInfo, its RVAs relative to `p[3]`: catchable
    // type array at +12 (count, then type RVAs), each type's descriptor at
    // +4, whose decorated name starts at +16; the object is at `p[1]`.
    unsafe {
        let rva = |at: usize| (at as *const u32).read_unaligned() as usize;
        let types = p[3] + rva(p[2] + 12);
        let name = |i: usize| {
            let descriptor = p[3] + rva(p[3] + rva(types + 4 + 4 * i) + 4);
            std::ffi::CStr::from_ptr((descriptor + 16) as *const std::ffi::c_char)
                .to_string_lossy()
        };
        let first = name(0);
        let value = match &*first {
            // int, unsigned, long (HRESULT), unsigned long
            ".H" | ".I" | ".J" | ".K" if p[1] != 0 => {
                format!(" = {:#010x}", (p[1] as *const u32).read_unaligned())
            }
            // std::exception keeps its message after the table pointer.
            _ if (0..rva(types)).any(|i| name(i) == ".?AVexception@std@@") => {
                let message = (p[1] as *const *const std::ffi::c_char).add(1).read();
                if message.is_null() {
                    String::new()
                } else {
                    format!(": {}", std::ffi::CStr::from_ptr(message).to_string_lossy())
                }
            }
            _ => String::new(),
        };
        format!("{first}{value}")
    }
}

/// A crash report being written: printed line by line (a fault while
/// unwinding must not lose the lines before it), and kept for the file.
#[derive(Default)]
pub(crate) struct Report(String);

impl Report {
    pub(crate) fn line(&mut self, l: String) {
        eprintln!("{l}");
        self.0.push_str(&l);
        self.0.push('\n');
    }

    /// The first line: what happened, where (`module+rva`), and what the
    /// exception's parameters say (an access, a C++ throw).
    pub(crate) fn header(&mut self, what: &str, code: u32, at: String, p: &[usize]) {
        let detail = if code == ACCESS_VIOLATION && p.len() >= 2 {
            let kind = match p[0] {
                0 => "read",
                1 => "write",
                _ => "execute",
            };
            format!(" ({kind} {:#x})", p[1])
        } else if code == CPP_EXCEPTION && p.len() >= 4 && p[0] == CPP_MAGIC {
            // SAFETY: a live MSVC throw.
            format!(" (C++ {} thrown)", unsafe { thrown(p) })
        } else {
            String::new()
        };
        self.line(format!("VIctory: {what} {code:#010x} at {at}{detail}"));
    }

    /// The integer registers, in x86 order (rax, rcx, rdx, rbx, rsp, rbp,
    /// rsi, rdi, r8..r15).
    pub(crate) fn registers(&mut self, r: &[u64; 16]) {
        self.line(format!(
            "  rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x} rsi {:#x} rdi {:#x} rsp {:#x} rbp {:#x}",
            r[0], r[3], r[1], r[2], r[6], r[7], r[4], r[5]
        ));
        self.line(format!(
            "  r8 {:#x} r9 {:#x} r10 {:#x} r11 {:#x} r12 {:#x} r13 {:#x} r14 {:#x} r15 {:#x}",
            r[8], r[9], r[10], r[11], r[12], r[13], r[14], r[15]
        ));
    }

    /// With `to_file`, the title's last output and the append to the crash log.
    pub(crate) fn finish(mut self, to_file: bool) {
        if to_file {
            let said = title_output();
            eprint!("{said}");
            self.0.push_str(&said);
            append_to_log(&self.0);
        }
    }
}
