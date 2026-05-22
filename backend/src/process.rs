use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use std::{env, fs};

use anyhow::{Context, Result, anyhow};

const KILL_POLL_MS: u64 = 50;
const KILL_WAIT_MS: u64 = 500;
const VERIFY_POLL_MS: u64 = 250;
const VERIFY_TIMEOUT_MS: u64 = 10_000;
const LOG_TAIL_LINES: usize = 20;

pub fn check_graphical_session() -> Result<()> {
    let var = |key| env::var(key).ok().filter(|v| !v.is_empty());
    for key in [
        "XDG_RUNTIME_DIR",
        "WAYLAND_DISPLAY",
        "HYPRLAND_INSTANCE_SIGNATURE",
    ] {
        if var(key).is_none() {
            return Err(anyhow!(
                "not running inside a usable Hyprland session: missing {key}; \
                 launch Rice Cooker from the Hyprland user session you want to rice"
            ));
        }
    }
    let runtime = PathBuf::from(var("XDG_RUNTIME_DIR").expect("checked above"));
    if !runtime.is_absolute() || !runtime.is_dir() {
        return Err(anyhow!(
            "XDG_RUNTIME_DIR is not an absolute directory: {runtime:?}"
        ));
    }
    let probe = runtime.join(format!(".rice-cooker-session-check-{}", std::process::id()));
    fs::create_dir(&probe)
        .with_context(|| format!("XDG_RUNTIME_DIR is not writable: {runtime:?}"))?;
    let _ = fs::remove_dir(&probe);
    Ok(())
}

pub fn kill_notif_daemons() -> Result<()> {
    for notifier in ["dunst", "mako", "swaync"] {
        pkill(&["-TERM", "-x", notifier])?;
    }
    Ok(())
}

pub fn kill_quickshell() -> Result<()> {
    pkill(&["-TERM", "-x", "quickshell|qs"])?;

    let deadline = Instant::now() + Duration::from_millis(KILL_WAIT_MS);
    while Instant::now() < deadline {
        if pgrep(&["-x", "quickshell|qs"])?.is_empty() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(KILL_POLL_MS));
    }

    pkill(&["-KILL", "-x", "quickshell|qs"])?;
    // `quickshell --no-duplicate` is the default, so a follow-up launch
    // would silently exit if a prior qs survived SIGKILL. Verify it's gone.
    thread::sleep(Duration::from_millis(KILL_POLL_MS));
    if !pgrep(&["-x", "quickshell|qs"])?.is_empty() {
        return Err(anyhow!(
            "quickshell still running after SIGKILL (possibly D-state)"
        ));
    }
    Ok(())
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

pub fn find_running_quickshell() -> Result<Option<QuickshellProc>> {
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        // Skip races (process exited) and other users' entries (hidepid). Any
        // other error propagates — silently dropping it would mis-record our
        // own unreadable qs as "nothing was running".
        let bytes = match fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(b) => b,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(e) => return Err(anyhow!("reading /proc/{pid}/cmdline: {e}")),
        };
        let argv = parse_cmdline(&bytes);
        if argv.is_empty() {
            continue;
        }
        let argv0_basename = Path::new(&argv[0])
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if matches!(argv0_basename.as_str(), "quickshell" | "qs") {
            let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok();
            return Ok(Some(QuickshellProc { cmdline: argv, cwd }));
        }
    }
    Ok(None)
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
        let lossy = parse_cmdline(b"\xff\0ok\0");
        assert!(lossy[0].contains('\u{FFFD}'));
        assert_eq!(lossy[1], "ok");
    }
}
