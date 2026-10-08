use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use std::fs;

use anyhow::{Context, Result, anyhow};
use regex::Regex;

use crate::compositor::{Compositor, CompositorId, LayerSurface, Ownership, SessionEnv};

const KILL_POLL_MS: u64 = 50;
const KILL_WAIT_MS: u64 = 500;
const VERIFY_POLL_MS: u64 = 250;
const VERIFY_TIMEOUT_MS: u64 = 10_000;
const LOG_TAIL_LINES: usize = 20;

/// A usable graphical session: the environment plus the compositor we will talk to.
///
/// The compositor is detected here rather than assumed, so a niri session is a
/// first-class target and not a failure.
pub struct Session {
    pub runtime_dir: PathBuf,
    pub wayland_display: Option<String>,
    pub compositor: Compositor,
    /// Kept so callers can pass it on without re-reading the environment.
    pub env: SessionEnv,
}

impl Session {
    pub fn compositor_id(&self) -> CompositorId {
        self.compositor.id()
    }
}

pub fn check_graphical_session() -> Result<Session> {
    let env = SessionEnv::from_process();
    for (key, value) in [
        ("XDG_RUNTIME_DIR", env.runtime_dir.to_str()),
        ("WAYLAND_DISPLAY", env.wayland_display.as_deref()),
    ] {
        if value.is_none_or(str::is_empty) {
            return Err(anyhow!(
                "not running inside a usable Wayland session: missing {key}; \
                 launch Rice Cooker from the desktop session you want to rice"
            ));
        }
    }
    let runtime = env.runtime_dir.clone();
    if !runtime.is_absolute() || !runtime.is_dir() {
        return Err(anyhow!(
            "XDG_RUNTIME_DIR is not an absolute directory: {runtime:?}"
        ));
    }
    let probe = runtime.join(format!(".rice-cooker-session-check-{}", std::process::id()));
    fs::create_dir(&probe)
        .with_context(|| format!("XDG_RUNTIME_DIR is not writable: {runtime:?}"))?;
    let _ = fs::remove_dir(&probe);

    let compositor = Compositor::detect(&env)?;
    Ok(Session {
        runtime_dir: runtime,
        wayland_display: env.wayland_display.clone(),
        compositor,
        env,
    })
}

pub fn kill_notif_daemons() -> Result<()> {
    for notifier in ["dunst", "mako", "swaync"] {
        pkill(&["-TERM", "-x", notifier])?;
    }
    Ok(())
}

/// Shell process names recognised when identifying "the shell that is running".
///
/// `noctalia` is in the list because that is what
/// `desktop/niri/settings/startup.nix` spawns on this desktop; matching only
/// `quickshell|qs` meant the running shell was never captured, never evicted and
/// never replayed.
///
/// `quickshell` covers upstream quickshell and the `noctalia-qs` fork through
/// [`normalize_shell_name`], which unwraps Nix's `.foo-wrapped` naming.
pub const DEFAULT_SHELL_NAMES: &[&str] = &["quickshell", "qs", "noctalia-qs", "noctalia"];

/// Reduce an argv0 or `/proc/<pid>/exe` path to the name to match on.
///
/// Handles three shapes seen in the wild:
/// - `/nix/store/…-quickshell-1.2/bin/quickshell` → `quickshell`
/// - `/nix/store/…/bin/.quickshell-wrapped` → `quickshell` (Nix `wrapProgram`)
/// - `qs` → `qs`
pub fn normalize_shell_name(raw: &str) -> String {
    let base = Path::new(raw)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.to_string());
    let base = base.strip_prefix('.').unwrap_or(&base);
    let base = base.strip_suffix("-wrapped").unwrap_or(base);
    base.to_string()
}

/// Set of shell names considered ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellMatcher {
    names: Vec<String>,
}

impl Default for ShellMatcher {
    fn default() -> Self {
        Self {
            names: DEFAULT_SHELL_NAMES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl ShellMatcher {
    /// Build from explicit names, normalising and de-duplicating. Duplicates are
    /// removed so callers can union catalog and default names without care.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out: Vec<String> = Vec::new();
        for name in names {
            let normalized = normalize_shell_name(name.as_ref());
            // Whitespace-only names are artifacts of splitting a config string,
            // never real process names.
            if !normalized.trim().is_empty() && !out.contains(&normalized) {
                out.push(normalized);
            }
        }
        Self { names: out }
    }

    /// Defaults plus the catalog's launch binaries, so a rice launched through a
    /// custom binary is still recognised as a shell.
    pub fn with_extra<I, S>(extra: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut all: Vec<String> = DEFAULT_SHELL_NAMES.iter().map(|s| s.to_string()).collect();
        all.extend(extra.into_iter().map(|s| s.as_ref().to_string()));
        Self::new(all)
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Human-readable form for error messages.
    pub fn label(&self) -> String {
        self.names.join("|")
    }

    /// Match on argv0 *or* the resolved executable, because a wrapper script's
    /// argv0 is the wrapper while its `exe` is the interpreter.
    pub fn matches(&self, argv: &[String], exe: Option<&Path>) -> bool {
        let from_argv = argv.first().map(|a| normalize_shell_name(a));
        let from_exe = exe.map(|p| normalize_shell_name(&p.to_string_lossy()));
        [from_argv, from_exe]
            .into_iter()
            .flatten()
            .any(|name| self.names.iter().any(|n| n == &name))
    }
}

/// Kill every shell process we recognise.
///
/// Signals pids discovered from `/proc` rather than using `pkill -x`, because
/// `-x` matches `comm`, which the kernel truncates to 15 characters — so
/// `.quickshell-wrapped` and store-path argv0 binaries never match.
pub fn kill_quickshell() -> Result<()> {
    kill_shells(&PathBuf::from(PROC_ROOT), &ShellMatcher::default())
}

/// Kill the known shells *and* whatever `extra` names.
///
/// A Nix rice's binary is a store path whose argv0 basename is the rice's own
/// name, not `quickshell`, so the default matcher alone would leave a previously
/// previewed shell running and leave two shells fighting for the same surfaces.
pub fn kill_quickshell_with(extra: &[String]) -> Result<()> {
    kill_shells(
        &PathBuf::from(PROC_ROOT),
        &ShellMatcher::with_extra(extra.iter().cloned()),
    )
}

pub fn kill_shells(proc_root: &Path, matcher: &ShellMatcher) -> Result<()> {
    let pids = matching_pids(proc_root, matcher)?;
    if pids.is_empty() {
        return Ok(());
    }
    signal(&pids, "-TERM")?;

    let deadline = Instant::now() + Duration::from_millis(KILL_WAIT_MS);
    while Instant::now() < deadline {
        if matching_pids(proc_root, matcher)?.is_empty() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(KILL_POLL_MS));
    }

    let survivors = matching_pids(proc_root, matcher)?;
    if survivors.is_empty() {
        return Ok(());
    }
    signal(&survivors, "-KILL")?;

    // A surviving SIGKILL means D-state, and quickshell's `--no-duplicate`
    // default would make the follow-up launch exit silently.
    thread::sleep(Duration::from_millis(KILL_POLL_MS));
    let survivors = matching_pids(proc_root, matcher)?;
    if !survivors.is_empty() {
        return Err(anyhow!(
            "shell process(es) {survivors:?} still running after SIGKILL (possibly D-state)"
        ));
    }
    Ok(())
}

fn signal(pids: &[u32], sig: &str) -> Result<()> {
    let status = Command::new("kill")
        .arg(sig)
        .args(pids.iter().map(|p| p.to_string()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning kill")?;
    // `kill` exits non-zero if any pid vanished between the scan and the signal;
    // the caller re-scans, so that race is not an error.
    let _ = status;
    Ok(())
}

/// Pids whose argv0 or executable name matches, scanned from `proc_root`.
pub fn matching_pids(proc_root: &Path, matcher: &ShellMatcher) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for pid in proc_pids(proc_root)? {
        let Some(proc_entry) = read_proc_entry(proc_root, pid)? else {
            continue;
        };
        if matcher.matches(&proc_entry.cmdline, proc_entry.exe.as_deref()) {
            out.push(pid);
        }
    }
    Ok(out)
}

pub fn rice_shell_alive(name: &str) -> Result<bool> {
    Ok(!pgrep(&["-xf", &format!("quickshell -c {name}")])?.is_empty())
}

/// quickshell resolves `<name>` against `$XDG_CONFIG_HOME/quickshell/<name>/shell.qml`
/// — the target of the symlink our install pipeline creates.
pub fn launch_detached_by_name(name: &str, log_file: &Path, cwd: &Path) -> Result<()> {
    let argv = vec!["quickshell".to_string(), "-c".to_string(), name.to_string()];
    launch_argv(&argv, cwd, log_file)
}

/// Relaunch from a persisted argv+cwd pair, regardless of `-p <path>` vs `-c <name>`.
pub fn launch_argv(argv: &[String], cwd: &Path, log_file: &Path) -> Result<()> {
    let (argv0, rest) = argv
        .split_first()
        .ok_or_else(|| anyhow!("empty argv; nothing to launch"))?;
    let log = fs::File::create(log_file)
        .with_context(|| format!("opening log {}", log_file.display()))?;
    let log_stdout = log
        .try_clone()
        .with_context(|| format!("cloning log handle {}", log_file.display()))?;
    // setsid's exit reflects spawn success only — `verify_by_name` checks child health.
    let status = Command::new("setsid")
        .arg("-f")
        .arg(argv0)
        .args(rest)
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log_stdout)
        .stderr(log)
        .status()
        .with_context(|| format!("spawning setsid {argv0}"))?;
    if !status.success() {
        return Err(anyhow!("setsid failed to spawn (exit {status})"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum VerifyResult {
    Ok,
    Dead { log_tail: String },
}

pub fn verify_by_name(name: &str, log_file: &Path) -> Result<VerifyResult> {
    let pat = format!("quickshell -c {name}");
    let deadline = Instant::now() + Duration::from_millis(VERIFY_TIMEOUT_MS);
    let mut hypr_ever_said_no = false;

    loop {
        thread::sleep(Duration::from_millis(VERIFY_POLL_MS));

        let pids = pgrep(&["-xf", &pat])?;
        let alive = !pids.is_empty();
        let log_contents = fs::read_to_string(log_file).unwrap_or_default();

        if !alive {
            return Ok(VerifyResult::Dead {
                log_tail: tail_lines_or_placeholder(&log_contents, name),
            });
        }
        // Not matching bare "ERROR:" — quickshell emits that for Qt deprecation
        // notices and other non-fatal runtime errors.
        if log_contents.contains("Failed to load configuration") {
            return Ok(VerifyResult::Dead {
                log_tail: tail_lines_or_placeholder(&log_contents, name),
            });
        }
        match hyprland_owns_layers(&pids) {
            Some(true) => return Ok(VerifyResult::Ok),
            Some(false) => hypr_ever_said_no = true,
            None => {}
        }

        if Instant::now() >= deadline {
            // Re-check liveness: `alive` above is up to VERIFY_POLL_MS stale.
            if pgrep(&["-xf", &pat])?.is_empty() {
                return Ok(VerifyResult::Dead {
                    log_tail: tail_lines_or_placeholder(&log_contents, name),
                });
            }
            // Non-Hyprland compositors leave hypr_ever_said_no false and fall
            // back to alive + log-clean = Ok.
            if hypr_ever_said_no {
                let base_tail = tail_lines_or_placeholder(&log_contents, name);
                return Ok(VerifyResult::Dead {
                    log_tail: format!(
                        "{base_tail}\n<rice-cooker: shell alive + log-clean but created 0 layer-shell surfaces in {VERIFY_TIMEOUT_MS}ms — likely a missing runtime dep (wallpaper path, dbus service, specific env)>"
                    ),
                });
            }
            return Ok(VerifyResult::Ok);
        }
    }
}

/// Layer-ownership evidence for a shell launched from an explicit argv.
///
/// Assembled by the caller so this module needs no catalog knowledge. `baseline`
/// must be a snapshot taken *after* eviction and *before* launch, so that a
/// surviving surface from the outgoing shell cannot be mistaken for the new one.
pub struct OwnershipProbe<'a> {
    pub compositor: &'a Compositor,
    /// Compiled from the catalog entry's `layer_namespaces`.
    pub namespaces: &'a [Regex],
    pub baseline: &'a [LayerSurface],
}

/// Verify a shell launched from an explicit argv — the Nix path, where the binary
/// is a store path rather than `quickshell -c <name>`.
///
/// Liveness comes from procfs argv/exe matching instead of a fixed `pgrep -xf`
/// pattern, because a store path can carry a Nix `.foo-wrapped` name. Ownership
/// comes from the compositor: on Hyprland via pid, on niri via layer namespace,
/// since niri reports no pid at all.
pub fn verify_argv(
    matcher: &ShellMatcher,
    proc_root: &Path,
    log_file: &Path,
    ownership: Option<&OwnershipProbe<'_>>,
) -> Result<VerifyResult> {
    let label = matcher.label();
    let deadline = Instant::now() + Duration::from_millis(VERIFY_TIMEOUT_MS);
    // True once the compositor answered and did not list our surfaces. Drives the
    // same "alive but opened nothing" diagnosis Hyprland already had.
    let mut ipc_ever_said_no = false;

    loop {
        thread::sleep(Duration::from_millis(VERIFY_POLL_MS));

        let pids = matching_pids(proc_root, matcher)?;
        let alive = !pids.is_empty();
        let log_contents = fs::read_to_string(log_file).unwrap_or_default();

        if !alive {
            return Ok(VerifyResult::Dead {
                log_tail: tail_lines_or_placeholder(&log_contents, &label),
            });
        }
        // Not matching bare "ERROR:" — quickshell emits that for Qt deprecation
        // notices and other non-fatal runtime errors.
        if log_contents.contains("Failed to load configuration") {
            return Ok(VerifyResult::Dead {
                log_tail: tail_lines_or_placeholder(&log_contents, &label),
            });
        }
        if let Some(probe) = ownership
            && let Some(snapshot) = probe.compositor.layers()
        {
            let own = Ownership {
                pids: &pids,
                namespaces: probe.namespaces,
                baseline: probe.baseline,
            };
            if crate::compositor::owns_layers(&snapshot, &own) {
                return Ok(VerifyResult::Ok);
            }
            ipc_ever_said_no = true;
        }

        if Instant::now() >= deadline {
            // Re-check liveness: `alive` above is up to VERIFY_POLL_MS stale.
            if matching_pids(proc_root, matcher)?.is_empty() {
                return Ok(VerifyResult::Dead {
                    log_tail: tail_lines_or_placeholder(&log_contents, &label),
                });
            }
            if ipc_ever_said_no {
                let base_tail = tail_lines_or_placeholder(&log_contents, &label);
                return Ok(VerifyResult::Dead {
                    log_tail: format!(
                        "{base_tail}\n<rice-cooker: shell alive + log-clean but created 0 matching \
                         layer-shell surfaces in {VERIFY_TIMEOUT_MS}ms — likely a missing runtime \
                         dep (wallpaper path, dbus service, specific env) or a namespace the \
                         catalog does not declare>"
                    ),
                });
            }
            // No ownership probe, or the compositor never answered: alive +
            // log-clean is the best available verdict.
            return Ok(VerifyResult::Ok);
        }
    }
}

pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Tolerates the trailing NUL Linux appends; invalid UTF-8 becomes U+FFFD.
fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let trimmed = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    trimmed
        .split(|&b| b == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

pub struct QuickshellProc {
    pub cmdline: Vec<String>,
    /// cwd preserved for relative `-p` paths in the original argv.
    pub cwd: Option<PathBuf>,
}

/// Where procfs is mounted. Injected in tests so `/proc` shapes can be faked.
pub const PROC_ROOT: &str = "/proc";

/// One process's identity as read from procfs.
#[derive(Debug, Clone)]
struct ProcEntry {
    cmdline: Vec<String>,
    cwd: Option<PathBuf>,
    exe: Option<PathBuf>,
}

fn proc_pids(proc_root: &Path) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(proc_root) {
        Ok(e) => e,
        // No procfs (non-Linux) means no processes to find, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", proc_root.display())),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() {
            out.push(pid);
        }
    }
    Ok(out)
}

fn read_proc_entry(proc_root: &Path, pid: u32) -> Result<Option<ProcEntry>> {
    // Skip races (process exited) and other users' entries (hidepid). Any other
    // error propagates — silently dropping it would mis-record our own
    // unreadable shell as "nothing was running".
    let bytes = match fs::read(proc_root.join(pid.to_string()).join("cmdline")) {
        Ok(b) => b,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(anyhow!("reading cmdline for pid {pid}: {e}")),
    };
    let cmdline = parse_cmdline(&bytes);
    if cmdline.is_empty() {
        return Ok(None);
    }
    let dir = proc_root.join(pid.to_string());
    Ok(Some(ProcEntry {
        cmdline,
        cwd: fs::read_link(dir.join("cwd")).ok(),
        exe: fs::read_link(dir.join("exe")).ok(),
    }))
}

/// Names of the running processes that match, deduplicated and sorted.
///
/// `matching_pids` answers "is any of these running"; this answers "which", so a
/// caller can report a conflict by name instead of just failing.
pub fn running_shell_names(proc_root: &Path, matcher: &ShellMatcher) -> Result<Vec<String>> {
    let mut found: Vec<String> = Vec::new();
    for pid in proc_pids(proc_root)? {
        let Some(entry) = read_proc_entry(proc_root, pid)? else {
            continue;
        };
        let name = entry
            .cmdline
            .first()
            .map(|a| normalize_shell_name(a))
            .or_else(|| entry.exe.as_deref().map(|e| normalize_shell_name(&e.to_string_lossy())))
            .unwrap_or_default();
        if matcher.matches(&entry.cmdline, entry.exe.as_deref()) && !found.contains(&name) {
            found.push(name);
        }
    }
    found.sort();
    Ok(found)
}

/// The first running shell matching `matcher`, for capture-before-install.
pub fn find_running_shell(
    proc_root: &Path,
    matcher: &ShellMatcher,
) -> Result<Option<QuickshellProc>> {
    for pid in proc_pids(proc_root)? {
        let Some(entry) = read_proc_entry(proc_root, pid)? else {
            continue;
        };
        if matcher.matches(&entry.cmdline, entry.exe.as_deref()) {
            return Ok(Some(QuickshellProc {
                cmdline: entry.cmdline,
                cwd: entry.cwd,
            }));
        }
    }
    Ok(None)
}

pub fn find_running_quickshell() -> Result<Option<QuickshellProc>> {
    find_running_shell(&PathBuf::from(PROC_ROOT), &ShellMatcher::default())
}

fn pkill(args: &[&str]) -> Result<()> {
    let status = Command::new("pkill")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning pkill")?;
    match status.code() {
        Some(0) | Some(1) => Ok(()),
        Some(c) => Err(anyhow!("pkill {:?} failed with exit code {}", args, c)),
        None => Err(anyhow!("pkill {:?} terminated by signal", args)),
    }
}

fn pgrep(args: &[&str]) -> Result<Vec<u32>> {
    let out = Command::new("pgrep")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .context("spawning pgrep")?;
    match out.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .collect()),
        Some(1) => Ok(Vec::new()),
        Some(c) => Err(anyhow!("pgrep {:?} failed with exit code {}", args, c)),
        None => Err(anyhow!("pgrep {:?} terminated by signal", args)),
    }
}

fn tail_lines_or_placeholder(log: &str, name: &str) -> String {
    if log.is_empty() {
        format!("<no log content for quickshell -c {name}>")
    } else {
        tail_lines(log, LOG_TAIL_LINES)
    }
}

/// Some(answer) if hyprctl responded; None on any failure. The `timeout` guard
/// keeps a wedged compositor from blocking past verify's deadline.
fn hyprland_owns_layers(pids: &[u32]) -> Option<bool> {
    let out = Command::new("timeout")
        .args(["--signal=KILL", "1", "hyprctl", "layers", "-j"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8(out.stdout).ok()?;
    let root: serde_json::Value = serde_json::from_str(&body).ok()?;
    // Shape: { "<monitor>": { "levels": { "0": [ {pid, ...}, ... ] } } }
    let root_obj = root.as_object()?;
    let pid_set: std::collections::HashSet<u32> = pids.iter().copied().collect();
    for monitor in root_obj.values() {
        let Some(levels) = monitor.get("levels").and_then(|v| v.as_object()) else {
            continue;
        };
        for layer_list in levels.values() {
            let Some(arr) = layer_list.as_array() else {
                continue;
            };
            for layer in arr {
                if let Some(pid) = layer.get("pid").and_then(|v| v.as_u64())
                    && pid_set.contains(&(pid as u32))
                {
                    return Some(true);
                }
            }
        }
    }
    Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One process to plant in the fixture procfs: `(pid, argv, exe target)`.
    type ProcFixture = (i32, Vec<String>, Option<String>);

    /// `proc!(42, ["/bin/x", "-c", "y"], None)` — owned values throughout so the
    /// fixture never depends on array-to-slice coercion.
    macro_rules! proc {
        ($pid:expr, [$($arg:literal),* $(,)?], $exe:expr) => {
            (
                $pid,
                vec![$($arg.to_string()),*],
                $exe.map(|e: &str| e.to_string()),
            )
        };
    }

    /// Build a fixture procfs with the given processes.
    fn fixture_proc(procs: Vec<ProcFixture>) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for (pid, argv, exe) in procs {
            let dir = t.path().join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            let mut cmdline = Vec::new();
            for arg in &argv {
                cmdline.extend_from_slice(arg.as_bytes());
                cmdline.push(0);
            }
            fs::write(dir.join("cmdline"), cmdline).unwrap();
            if let Some(exe) = exe {
                // The link target need not exist; read_link is what matters.
                std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
            }
        }
        t
    }

    #[test]
    fn tail_returns_last_n_lines() {
        assert_eq!(tail_lines("1\n2\n3\n4\n5", 2), "4\n5");
        assert_eq!(tail_lines("a\nb\nc", 10), "a\nb\nc");
    }

    #[test]
    fn parse_cmdline_handles_edge_cases() {
        assert!(parse_cmdline(b"").is_empty());
        assert_eq!(parse_cmdline(b"foo\0bar\0"), vec!["foo", "bar"]);
        assert_eq!(parse_cmdline(b"foo\0bar"), vec!["foo", "bar"]);
        assert_eq!(parse_cmdline(b"foo\0\0bar\0"), vec!["foo", "", "bar"]);
        let raw: &[u8] = b"\xff\0ok\0";
        let lossy = parse_cmdline(raw);
        assert!(lossy[0].contains('\u{FFFD}'));
        assert_eq!(lossy[1], "ok");
    }

    // ── shell identity ───────────────────────────────────────────────────────

    #[test]
    fn normalize_unwraps_store_paths_and_nix_wrappers() {
        assert_eq!(normalize_shell_name("qs"), "qs");
        assert_eq!(normalize_shell_name("/usr/bin/quickshell"), "quickshell");
        assert_eq!(
            normalize_shell_name("/nix/store/abc-quickshell-1.2/bin/quickshell"),
            "quickshell"
        );
        // `wrapProgram` produces a dot-prefixed `-wrapped` sibling.
        assert_eq!(
            normalize_shell_name("/nix/store/abc/bin/.quickshell-wrapped"),
            "quickshell"
        );
        assert_eq!(normalize_shell_name("/nix/store/x/bin/.noctalia-wrapped"), "noctalia");
        // A leading dot with no -wrapped suffix still loses the dot.
        assert_eq!(normalize_shell_name(".hidden"), "hidden");
        assert_eq!(normalize_shell_name(""), "");
    }

    #[test]
    fn default_matcher_recognises_noctalia() {
        // The regression: boreal's shell is `noctalia`, so matching only
        // `quickshell|qs` left it uncaptured, un-evicted and un-replayed.
        let matcher = ShellMatcher::default();
        assert!(matcher.matches(&["noctalia".to_string()], None));
        assert!(matcher.matches(&["/nix/store/x/bin/noctalia".to_string()], None));
        assert!(matcher.matches(&["quickshell".to_string()], None));
        assert!(matcher.matches(&["qs".to_string()], None));
        assert!(matcher.matches(&["noctalia-qs".to_string()], None));
    }

    #[test]
    fn unrelated_processes_do_not_match() {
        let matcher = ShellMatcher::default();
        for argv0 in ["waybar", "electron", "kitty", "notnoctalia"] {
            assert!(
                !matcher.matches(&[argv0.to_string()], None),
                "matched {argv0}"
            );
        }
    }

    #[test]
    fn matcher_falls_back_to_the_resolved_executable() {
        // A wrapper script's argv0 is the wrapper while its exe is the
        // interpreter, and vice versa; either side may carry the name.
        let matcher = ShellMatcher::default();
        assert!(matcher.matches(
            &["/bin/sh".to_string()],
            Some(Path::new("/nix/store/x/bin/.noctalia-wrapped"))
        ));
        assert!(!matcher.matches(
            &["/bin/sh".to_string()],
            Some(Path::new("/usr/bin/bash"))
        ));
    }

    #[test]
    fn new_normalises_and_de_duplicates() {
        let matcher = ShellMatcher::new(["noctalia", "/usr/bin/noctalia", "./noctalia"]);
        assert_eq!(matcher.names(), &["noctalia".to_string()]);
    }

    #[test]
    fn with_extra_keeps_the_defaults() {
        let matcher = ShellMatcher::with_extra(["amane"]);
        assert!(matcher.names().contains(&"amane".to_string()));
        assert!(matcher.names().contains(&"noctalia".to_string()));
        assert!(matcher.names().contains(&"quickshell".to_string()));
    }

    #[test]
    fn empty_names_are_dropped() {
        assert!(ShellMatcher::new(["", "   "]).names().is_empty());
    }

    // ── procfs scanning ──────────────────────────────────────────────────────

    #[test]
    fn matching_pids_finds_a_noctalia_process() {
        let proc = fixture_proc(vec![
            proc!(1, ["/usr/lib/systemd/systemd"], None),
            proc!(42, ["/nix/store/x/bin/noctalia"], None),
            proc!(43, ["waybar"], None),
        ]);
        assert_eq!(
            matching_pids(proc.path(), &ShellMatcher::default()).unwrap(),
            vec![42]
        );
    }

    #[test]
    fn matching_pids_finds_a_wrapped_shell_by_exe() {
        // pid 7: the wrapper itself is argv0.
        // pid 8: argv0 is a friendlier shim, so only the resolved exe names the
        // real shell. Both shapes have to be caught.
        let proc = fixture_proc(vec![
            proc!(7, ["/nix/store/y/bin/.quickshell-wrapped"], None),
            proc!(
                8,
                ["/home/u/.local/bin/rice-shell"],
                Some("/nix/store/z/bin/.noctalia-wrapped")
            ),
            proc!(9, ["/home/u/.local/bin/unrelated"], Some("/usr/bin/bash")),
        ]);
        let pids = matching_pids(proc.path(), &ShellMatcher::default()).unwrap();
        assert_eq!(pids, vec![7, 8], "pid 9 must not match");
    }

    #[test]
    fn missing_proc_root_is_empty_not_an_error() {
        let matcher = ShellMatcher::default();
        let missing = Path::new("/nonexistent-proc-root-for-test");
        assert!(matching_pids(missing, &matcher).unwrap().is_empty());
        assert!(find_running_shell(missing, &matcher).unwrap().is_none());
    }

    #[test]
    fn kill_shells_is_a_no_op_when_nothing_matches() {
        let proc = fixture_proc(vec![proc!(1, ["waybar"], None)]);
        // Must not invoke `kill` at all, and must report success.
        assert!(kill_shells(proc.path(), &ShellMatcher::default()).is_ok());
    }

    #[test]
    fn find_running_shell_returns_the_first_match_with_cwd() {
        let proc = fixture_proc(vec![
            proc!(5, ["waybar"], None),
            proc!(6, ["qs", "-c", "clock"], None),
        ]);
        let found = find_running_shell(proc.path(), &ShellMatcher::default())
            .unwrap()
            .expect("qs should be found");
        assert_eq!(found.cmdline, vec!["qs", "-c", "clock"]);
        assert!(found.cwd.is_none(), "fixture has no cwd link");
    }

    #[test]
    fn entries_without_cmdline_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        // A pid directory with no cmdline: a kernel thread or a race.
        fs::create_dir_all(t.path().join("9")).unwrap();
        assert!(matching_pids(t.path(), &ShellMatcher::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn non_numeric_proc_entries_are_ignored() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("self")).unwrap();
        assert!(matching_pids(t.path(), &ShellMatcher::default())
            .unwrap()
            .is_empty());
    }

    // ── verify_argv (the Nix path) ───────────────────────────────────────────

    #[test]
    fn verify_argv_reports_dead_when_nothing_matches() {
        let proc = fixture_proc(vec![proc!(1, ["waybar"], None)]);
        let log = tempfile::tempdir().unwrap();
        let log_file = log.path().join("last-run.log");
        fs::write(&log_file, "INFO: booting\n").unwrap();
        let matcher = ShellMatcher::new(["caelestia-shell"]);
        let result = verify_argv(&matcher, proc.path(), &log_file, None).unwrap();
        assert!(matches!(result, VerifyResult::Dead { .. }), "got {result:?}");
    }

    #[test]
    fn verify_argv_reports_dead_on_a_config_load_failure() {
        // A live process whose log shows the shell failed to load its config must
        // not be reported healthy, even though the pid is present.
        let proc = fixture_proc(vec![proc!(
            7,
            ["/nix/store/x/bin/caelestia-shell"],
            None
        )]);
        let log = tempfile::tempdir().unwrap();
        let log_file = log.path().join("last-run.log");
        fs::write(&log_file, "Failed to load configuration\n").unwrap();
        let matcher = ShellMatcher::new(["caelestia-shell"]);
        let result = verify_argv(&matcher, proc.path(), &log_file, None).unwrap();
        match result {
            VerifyResult::Dead { log_tail } => {
                assert!(log_tail.contains("Failed to load configuration"), "{log_tail}");
            }
            VerifyResult::Ok => panic!("a load failure must not verify as healthy"),
        }
    }

    #[test]
    fn running_shell_names_reports_which_ones_match() {
        let proc = fixture_proc(vec![
            proc!(1, ["waybar"], None),
            proc!(2, ["/usr/bin/eww"], None),
            proc!(3, ["kitty"], None),
        ]);
        let matcher = ShellMatcher::new(["waybar", "eww"]);
        assert_eq!(
            running_shell_names(proc.path(), &matcher).unwrap(),
            vec!["eww".to_string(), "waybar".to_string()]
        );
    }

    #[test]
    fn running_shell_names_is_empty_when_nothing_matches() {
        let proc = fixture_proc(vec![proc!(1, ["kitty"], None)]);
        let matcher = ShellMatcher::new(["waybar"]);
        assert!(running_shell_names(proc.path(), &matcher).unwrap().is_empty());
    }

    #[test]
    fn shell_matcher_label_is_readable() {
        assert_eq!(ShellMatcher::new(["qs", "noctalia"]).label(), "qs|noctalia");
    }

    #[test]
    fn session_reports_its_compositor_id() {
        use crate::compositor::CompositorId;
        let s = Session {
            runtime_dir: PathBuf::from("/run/user/1000"),
            wayland_display: Some("wayland-1".into()),
            compositor: Compositor::Niri {
                socket: PathBuf::from("/run/user/1000/niri.wayland-1.9.sock"),
            },
            env: SessionEnv {
                runtime_dir: PathBuf::from("/run/user/1000"),
                wayland_display: Some("wayland-1".into()),
                hyprland_signature: None,
                niri_socket: None,
                current_desktop: Some("niri".into()),
                proc_root: PathBuf::from("/proc"),
            },
        };
        assert_eq!(s.compositor_id(), CompositorId::Niri);
    }
}
