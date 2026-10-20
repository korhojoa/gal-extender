// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Command launcher, built into `libgal_loader.so`.
//!
//! The `launcher` feature reads `gal_launch.conf` beside the active loader
//! configuration, or beside the loader library when no configuration is open.
//! Blank lines and lines that start with `#` are ignored.
//!
//! - `once: <cmd>` runs once per boot after a successful exit.
//! - `daemon: <cmd>` or `<cmd>` starts a command only when it is not running.
//! - `check=<name> <cmd>` checks the process name through `pidin`.
//!
//! Runner commands use a shell background job. Child signal isolation is not guaranteed.
//! Enable this module only with a reviewed command configuration.

use core::ffi::{c_char, c_int};
use core::fmt::{self, Write as _};
use std::ffi::{CString, OsStr};
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;

use gal_feature_api::{config, location, FeatureModule};
use qnx_slog::log::Line;
use qnx_slog::sys::{SLOG_ERROR, SLOG_INFO};

/// The module the loader lists in `BUILTIN_FEATURES`.
pub const MODULE: FeatureModule = FeatureModule {
    name: "launcher",
    init,
};

/// The version in the banner: the single copy. Bump per change.
const VERSION: &str = "7-rs";

const LAUNCH_LIST_NAME: &str = "gal_launch.conf";
const SHMEM: &str = "/dev/shmem";
/// Most entries read from the list (`MAX_ENTRIES`).
const MAX_ENTRIES: usize = 32;
/// `fgets(line, MAX_CMD)`: a list line is read in chunks of 511 bytes.
const MAX_CMD: usize = 512;
/// Longest `check=` name kept (`char check[64]`).
const CHECK_MAX: usize = 63;
/// The `proc_running` command buffer (`char cmd[160]`).
const PIDIN_CMD_MAX: usize = 160;

fn log_at(sev: c_int, args: fmt::Arguments<'_>) {
    let mut msg: Line<512> = Line::new();
    let _ = msg.write_fmt(args);
    let mut line: Line<544> = Line::new();
    let _ = writeln!(line, "[GalLauncher] {}", msg.as_str());
    let _ = qnx_slog::log::text(sev, line.as_str());
}

macro_rules! lc_log {
    ($($a:tt)*) => { log_at(SLOG_INFO, format_args!($($a)*)) };
}
macro_rules! lc_loge {
    ($($a:tt)*) => { log_at(SLOG_ERROR, format_args!($($a)*)) };
}

/// Bytes printed as `%s` would print them (invalid UTF-8 as U+FFFD).
struct B<'a>(&'a [u8]);

impl fmt::Display for B<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.utf8_chunks() {
            f.write_str(c.valid())?;
            if !c.invalid().is_empty() {
                f.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

fn entr(n: usize) -> &'static str {
    if n == 1 {
        "y"
    } else {
        "ies"
    }
}

// ---------------------------------------------------------------------------
// libc
// ---------------------------------------------------------------------------

extern "C" {
    fn system(cmd: *const c_char) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
}

/// `system(cmd)`. -1 when the command holds a NUL (never: it is built from
/// C strings).
fn run_system(cmd: &[u8]) -> c_int {
    let Ok(c) = CString::new(cmd) else {
        return -1;
    };
    // SAFETY: a NUL-terminated command.
    unsafe { system(c.as_ptr()) }
}

/// `r != -1 && WIFEXITED(r) && WEXITSTATUS(r) == 0` (QNX sys/wait.h).
#[allow(clippy::verbose_bit_mask)] // the two macros, spelled as in sys/wait.h
fn exited_zero(r: c_int) -> bool {
    r != -1 && (r & 0xff) == 0 && ((r >> 8) & 0xff) == 0
}

fn path_of(p: &[u8]) -> &std::path::Path {
    std::path::Path::new(OsStr::from_bytes(p))
}

// ---------------------------------------------------------------------------
// Parsing (pure)
// ---------------------------------------------------------------------------

/// What one entry was found to need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    None,
    Launched,
    SkipRunning,
    SkipDone,
    Failed,
}

/// One command of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    cmd: Vec<u8>,
    /// Process name for the liveness check. empty: the recorded pid.
    check: Vec<u8>,
    is_once: bool,
    action: Action,
}

/// The text of each `fgets(buf, cap)` call over `data` in turn: up to and
/// including a newline, or `cap - 1` bytes, or the rest. then cut at the
/// first NUL (every later use is a C string).
fn fgets_chunks(data: &[u8], cap: usize) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let max = rest.len().min(cap - 1);
        let n = rest[..max]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(max, |i| i + 1);
        let (chunk, tail) = rest.split_at(n);
        rest = tail;
        Some(chunk.split(|&b| b == 0).next().unwrap_or(chunk))
    })
}

/// `trim`: leading blanks and tabs, trailing blanks, tabs, CR and LF.
fn trim(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(s.len());
    let s = &s[start..];
    let end = s
        .iter()
        .rposition(|&b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(0, |i| i + 1);
    &s[..end]
}

/// `valid_process_name`: non-empty, `[A-Za-z0-9_.-]` only (it goes into a
/// single-quoted shell word).
fn valid_process_name(s: &[u8]) -> bool {
    !s.is_empty()
        && s.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

/// `parse_list`'s loop over the list's bytes. `invalid` is called (to log)
/// for each `check=` name that fails [`valid_process_name`], in file order.
fn parse_list(data: &[u8], mut invalid: impl FnMut(&[u8])) -> Vec<Entry> {
    let mut out = Vec::new();
    for raw in fgets_chunks(data, MAX_CMD) {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let line = trim(raw);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let (is_once, cmd) = if let Some(c) = line.strip_prefix(b"once:") {
            (true, c)
        } else if let Some(c) = line.strip_prefix(b"daemon:") {
            (false, c)
        } else {
            (false, line)
        };
        let mut cmd = trim(cmd);
        let mut check: &[u8] = b"";
        if let Some(c) = cmd.strip_prefix(b"check=") {
            let k = c
                .iter()
                .take(CHECK_MAX)
                .position(|&b| b == b' ' || b == b'\t')
                .unwrap_or(c.len().min(CHECK_MAX));
            check = &c[..k];
            cmd = trim(&c[k..]);
            if !valid_process_name(check) {
                invalid(check);
                continue;
            }
        }
        if cmd.is_empty() {
            continue;
        }
        out.push(Entry {
            cmd: cmd.to_vec(),
            check: check.to_vec(),
            is_once,
            action: Action::None,
        });
    }
    out
}

/// `launch_list_path`: `gal_launch.conf` in the directory of the conf the
/// loader opened, else the default.
fn launch_list_path(conf: &[u8]) -> Vec<u8> {
    if let Some(slash) = conf.iter().rposition(|&b| b == b'/') {
        let dirlen = slash + 1;
        if dirlen < 256 - 20 {
            let mut p = conf[..dirlen].to_vec();
            p.extend_from_slice(b"gal_launch.conf");
            return p;
        }
    }
    launch_list_default()
}

fn launch_list_default() -> Vec<u8> {
    location::file(LAUNCH_LIST_NAME).into_bytes()
}

/// `atoi` (QNX: `(int)strtol(s, NULL, 10)`, long is 32 bits).
#[cfg(test)]
fn atoi(s: &[u8]) -> i32 {
    scan_int(s).unwrap_or(0)
}

/// `strtol` base 10 over `s`: None when no digit follows the white space
/// and sign (what `fscanf("%d")` treats as no match).
fn scan_int(s: &[u8]) -> Option<i32> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut v: i64 = 0;
    while let Some(d) = s.get(i).filter(|c| c.is_ascii_digit()) {
        v = (v * 10 + i64::from(d - b'0')).min(i64::from(i32::MAX) + 1);
        i += 1;
    }
    if i == start {
        return None;
    }
    let v = if neg { -v } else { v };
    i32::try_from(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).ok()
}

/// The shell runner body for one command entry.
fn runner_script(cmd: &[u8], done: Option<&str>) -> Vec<u8> {
    let mut s = Vec::with_capacity(cmd.len() + 200);
    s.extend_from_slice(cmd);
    s.extend_from_slice(b" >/dev/null 2>&1\n");
    s.extend_from_slice(b"rc=$?\n");
    if let Some(d) = done {
        s.extend_from_slice(format!("[ $rc -eq 0 ] && : > {d}\n").as_bytes());
    }
    s
}

struct Shm<'a> {
    dir: &'a str,
}

impl Shm<'_> {
    fn file(&self, id: usize, ext: &str) -> String {
        format!("{}/gal_launch_{}.{}", self.dir, id, ext)
    }

    /// `read_pid`: the first integer of the pid file, else 0.
    fn read_pid(&self, id: usize) -> i32 {
        std::fs::read(self.file(id, "pid"))
            .ok()
            .and_then(|b| scan_int(&b))
            .unwrap_or(0)
    }

    /// `pid_alive`: the recorded runner pid exists (`kill(pid, 0) == 0`).
    fn pid_alive(&self, id: usize) -> bool {
        let pid = self.read_pid(id);
        // SAFETY: signal 0 only checks the process.
        pid > 0 && unsafe { kill(pid, 0) } == 0
    }

    /// `done_exists`: the once-marker can be opened.
    fn done_exists(&self, id: usize) -> bool {
        std::fs::File::open(self.file(id, "done")).is_ok()
    }

    /// `daemon_running`: by name when the entry has one, else by pid.
    fn daemon_running(&self, e: &Entry, id: usize) -> bool {
        if e.check.is_empty() {
            self.pid_alive(id)
        } else {
            proc_running(&e.check)
        }
    }

    /// `launch_entry`: write the runner script, start it detached, record its
    /// pid.
    fn launch(&self, id: usize, cmd: &[u8], is_once: bool) -> Action {
        let sh = self.file(id, "sh");
        let pid = self.file(id, "pid");
        let done = self.file(id, "done");
        let script = runner_script(cmd, is_once.then_some(done.as_str()));
        match std::fs::File::create(&sh) {
            Ok(mut f) => {
                // Write the complete runner body before requesting launch.
                let _ = f.write_all(&script);
            }
            Err(e) => {
                lc_loge!(
                    "entry #{}: cannot write {} errno={} \u{2014} skipping",
                    id,
                    sh,
                    e.raw_os_error().unwrap_or(0)
                );
                return Action::Failed;
            }
        }
        let start = format!("sh {sh} </dev/null >/dev/null 2>&1 & echo $! > {pid}");
        let r = run_system(start.as_bytes());
        launch_result(id, cmd, is_once, r)
    }
}

fn launch_result(id: usize, cmd: &[u8], is_once: bool, result: c_int) -> Action {
    if result != 0 {
        lc_loge!("entry #{} launch failed status={}: {}", id, result, B(cmd));
        return Action::Failed;
    }
    lc_log!(
        "entry #{} shell launch accepted ({}): {}",
        id,
        if is_once { "once" } else { "daemon" },
        B(cmd)
    );
    Action::Launched
}

/// `proc_running`: `pidin | grep -qw '<name>'` exits 0.
fn proc_running(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut cmd = b"pidin 2>/dev/null | grep -qw '".to_vec();
    cmd.extend_from_slice(name);
    cmd.push(b'\'');
    cmd.truncate(PIDIN_CMD_MAX - 1);
    exited_zero(run_system(&cmd))
}

// ---------------------------------------------------------------------------
// init (the loader calls it once, on the constructor thread, panic-guarded)
// ---------------------------------------------------------------------------

/// Read the command list and launch entries not already completed or running.
fn run(shm: &Shm<'_>, list: &[u8]) {
    let Ok(mut f) = std::fs::File::open(path_of(list)) else {
        lc_log!("no launch list at {} (nothing to start)", B(list));
        return;
    };
    // A read error ends list processing.
    let mut data = Vec::new();
    let _ = f.read_to_end(&mut data);
    drop(f);
    let mut entries = parse_list(&data, |check| {
        lc_loge!(
            "invalid check= process name '{}' \u{2014} entry skipped",
            B(check)
        );
    });
    lc_log!(
        "parsed {} launch entr{} from {}",
        entries.len(),
        entr(entries.len()),
        B(list)
    );
    if entries.is_empty() {
        return;
    }

    for (i, e) in entries.iter_mut().enumerate() {
        if e.is_once && shm.done_exists(i) {
            e.action = Action::SkipDone;
            continue;
        }
        if !e.is_once && shm.daemon_running(e, i) {
            lc_log!(
                "entry #{} already running ({}) \u{2014} not relaunching: {}",
                i,
                if e.check.is_empty() {
                    B(b"by recorded pid")
                } else {
                    B(&e.check)
                },
                B(&e.cmd)
            );
            e.action = Action::SkipRunning;
            continue;
        }
        let a = shm.launch(i, &e.cmd, e.is_once);
        e.action = a;
    }
}

fn init() {
    lc_loge!("launcher v{}", VERSION);
    let list = launch_list_path(config::path().to_bytes());
    run(&Shm { dir: SHMEM }, &list);
}

#[cfg(test)]
mod tests;
