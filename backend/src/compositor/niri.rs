//! niri specifics: IPC socket discovery and direct socket protocol.
//!
//! niri's socket filename is built by `IpcServer::start` in niri as
//! `format!("niri.{wayland_socket_name}.{}.sock", process::id())`, where
//! `wayland_socket_name` is the `WAYLAND_DISPLAY` value. `$NIRI_SOCKET` points
//! at the same file, but it is not reliably exported ([niri#2149]), so this
//! module reconstructs the name from `WAYLAND_DISPLAY` and `/proc` instead of
//! trusting the variable.
//!
//! Requests are sent over the socket directly rather than shelling out to
//! `niri msg`, for two reasons: no dependency on the `niri` binary being on
//! `PATH`, and no CLI/compositor version skew — `niri msg` refuses to talk to a
//! compositor older than itself.
//!
//! [niri#2149]: https://github.com/YaLTeR/niri/issues/2149

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{IPC_TIMEOUT, LayerSurface, SessionEnv};

/// Name of the environment variable holding the niri IPC socket path.
/// Mirrors `niri_ipc::socket::SOCKET_PATH_ENV`.
pub const SOCKET_PATH_ENV: &str = "NIRI_SOCKET";

const PREFIX: &str = "niri.";
const SUFFIX: &str = ".sock";

/// `niri.<WAYLAND_DISPLAY>.<pid>.sock`
pub fn socket_filename(wayland_display: &str, pid: u32) -> String {
    format!("{PREFIX}{wayland_display}.{pid}{SUFFIX}")
}

/// Inverse of [`socket_filename`]. Returns `(wayland_display, pid)`.
///
/// The split is on the *last* `.` before the suffix, so a `WAYLAND_DISPLAY`
/// containing dots (`wayland-1.2`) still parses correctly.
pub fn parse_socket_filename(name: &str) -> Option<(&str, u32)> {
    let rest = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    let (display, pid) = rest.rsplit_once('.')?;
    if display.is_empty() {
        return None;
    }
    Some((display, pid.parse().ok()?))
}

fn is_live_pid(env: &SessionEnv, pid: u32) -> bool {
    // `/proc` absent (non-Linux or an injected fixture root) means liveness
    // cannot be established from the filesystem; do not reject on that basis.
    if !env.proc_root.is_dir() {
        return true;
    }
    env.proc_root.join(pid.to_string()).exists()
}

/// `Path::is_socket` is still nightly-only, so go through the metadata.
/// `symlink_metadata` on purpose: a symlink pointing at a socket is not a socket
/// we can connect to.
fn is_socket_path(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|md| md.file_type().is_socket())
        .unwrap_or(false)
}

/// Locate the niri IPC socket.
///
/// Returns `Ok(None)` when this is simply not a niri session. Returns `Err`
/// when it *is* a niri session but no usable socket could be found — that
/// distinction is what lets the caller print an actionable message instead of
/// "unsupported compositor".
pub fn resolve_socket(env: &SessionEnv) -> Result<Option<PathBuf>> {
    if let Some(raw) = env.niri_socket.as_deref() {
        let path = PathBuf::from(raw);
        if is_socket_path(&path) {
            return Ok(Some(path));
        }
        // Set but unusable: fall through to reconstruction rather than failing
        // outright, since a stale variable is the documented failure mode.
        eprintln!(
            "rice-cooker: warn: {SOCKET_PATH_ENV}={} is not a socket; falling back to $XDG_RUNTIME_DIR scan",
            path.display()
        );
    }

    let mut candidates = scan_runtime_dir(env)?;
    if candidates.is_empty() {
        if is_niri_desktop(env) {
            bail!(
                "niri session detected but no IPC socket found in {} \
                 (looked for niri.<WAYLAND_DISPLAY>.<pid>.sock; \
                 NIRI_SOCKET is unset or stale)",
                env.runtime_dir.display()
            );
        }
        return Ok(None);
    }

    // Prefer a socket that answers, then an exact WAYLAND_DISPLAY match, then
    // the newest pid. Ranking is stable so repeated calls pick the same socket.
    candidates.sort_by(|a, b| {
        b.responsive
            .cmp(&a.responsive)
            .then(b.exact_display.cmp(&a.exact_display))
            .then(b.pid.cmp(&a.pid))
    });
    Ok(Some(candidates[0].path.clone()))
}

struct Candidate {
    path: PathBuf,
    pid: u32,
    exact_display: bool,
    responsive: bool,
}

fn scan_runtime_dir(env: &SessionEnv) -> Result<Vec<Candidate>> {
    let entries = match std::fs::read_dir(&env.runtime_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", env.runtime_dir.display()));
        }
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((display, pid)) = parse_socket_filename(name) else {
            continue;
        };
        let path = entry.path();
        if !is_socket_path(&path) || !is_live_pid(env, pid) {
            continue;
        }
        out.push(Candidate {
            exact_display: env.wayland_display.as_deref() == Some(display),
            responsive: probe(&path),
            path,
            pid,
        });
    }
    Ok(out)
}

fn is_niri_desktop(env: &SessionEnv) -> bool {
    env.current_desktop
        .as_deref()
        .is_some_and(|d| d.to_ascii_lowercase().contains("niri"))
}

/// One blocking request/response round trip. `None` on any failure: connect
/// refused, timeout, or unparseable reply.
pub fn request(socket: &Path, request: &str) -> Option<String> {
    let stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(IPC_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IPC_TIMEOUT)).ok()?;

    let mut writer = stream.try_clone().ok()?;
    writer.write_all(request.as_bytes()).ok()?;
    writer.write_all(b"\n").ok()?;
    writer.flush().ok()?;
    // Half-close the write side so a compositor that waits for EOF before
    // answering cannot deadlock against our read.
    let _ = writer.shutdown(Shutdown::Write);

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) if line.trim().is_empty() => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

/// Cheap liveness probe: `Version` is the smallest request niri supports.
pub fn probe(socket: &Path) -> bool {
    request(socket, "\"Version\"").is_some_and(|line| line.contains("\"Ok\""))
}

/// Query the live layer list. `None` on failure or timeout, never "empty".
pub fn layers(socket: &Path) -> Option<Vec<LayerSurface>> {
    let line = request(socket, "\"Layers\"")?;
    match parse_layers_reply(&line) {
        Ok(layers) => Some(layers),
        Err(e) => {
            eprintln!("rice-cooker: warn: could not parse niri `Layers` reply: {e:#}");
            None
        }
    }
}

#[derive(serde::Deserialize)]
struct RawLayer {
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    layer: String,
}

/// Parse a `Reply` line.
///
/// Wire shape: `{"Ok":{"Layers":[{namespace,output,layer,...}]}}` or
/// `{"Err":"..."}`. Parsed via `serde_json::Value` on purpose: a compositor
/// newer than this binary may add fields (or reply variants), and
/// `deny_unknown_fields` / exhaustive enums would turn that into a hard failure
/// on a working desktop.
pub fn parse_layers_reply(body: &str) -> Result<Vec<LayerSurface>> {
    let value: serde_json::Value =
        serde_json::from_str(body.trim()).context("parsing niri reply")?;
    if let Some(err) = value.get("Err") {
        bail!("niri returned an error for Layers: {err}");
    }
    let payload = value
        .get("Ok")
        .ok_or_else(|| anyhow::anyhow!("niri reply had neither Ok nor Err: {body}"))?;
    let raw = payload
        .get("Layers")
        .ok_or_else(|| anyhow::anyhow!("expected a Layers reply, got: {payload}"))?;
    let parsed: Vec<RawLayer> = serde_json::from_value(raw.clone())
        .context("parsing niri layer surfaces")?;
    Ok(parsed
        .into_iter()
        .map(|l| LayerSurface {
            namespace: l.namespace,
            output: l.output,
            layer: l.layer,
            // niri's LayerSurface has no pid field. This is the whole reason
            // ownership needs a namespace-based path.
            pid: None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::os::unix::net::UnixListener;

    fn temp_env(root: &Path) -> SessionEnv {
        SessionEnv {
            runtime_dir: root.to_path_buf(),
            wayland_display: Some("wayland-1".to_string()),
            hyprland_signature: None,
            niri_socket: None,
            current_desktop: Some("niri".to_string()),
            compositor_override: None,
            proc_root: root.join("proc"),
        }
    }

    fn touch_socket(path: &Path) -> UnixListener {
        UnixListener::bind(path).unwrap_or_else(|e| panic!("bind {}: {e}", path.display()))
    }

    #[test]
    fn socket_filename_round_trips() {
        let name = socket_filename("wayland-1", 4242);
        assert_eq!(name, "niri.wayland-1.4242.sock");
        assert_eq!(parse_socket_filename(&name), Some(("wayland-1", 4242)));
    }

    #[test]
    fn socket_filename_parses_dotted_wayland_display() {
        // WAYLAND_DISPLAY can itself contain dots; the pid is the last segment.
        assert_eq!(
            parse_socket_filename("niri.wayland-1.2.999.sock"),
            Some(("wayland-1.2", 999))
        );
    }

    #[test]
    fn non_niri_socket_names_are_rejected() {
        for bad in [
            "wayland-1",           // the Wayland socket itself
            "niri.wayland-1.sock", // no pid
            "niri..5.sock",        // empty display
            "niri.wayland-1.x.sock", // non-numeric pid
            "niri.wayland-1.5.socket",
            ".sock",
            "",
        ] {
            assert_eq!(parse_socket_filename(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn resolves_via_niri_socket_env_when_present() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("niri.wayland-1.7.sock");
        let _listener = touch_socket(&sock);
        let mut env = temp_env(t.path());
        env.niri_socket = Some(sock.to_string_lossy().into_owned());
        assert_eq!(resolve_socket(&env).unwrap(), Some(sock));
    }

    #[test]
    fn stale_niri_socket_env_falls_back_to_scan() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join(socket_filename("wayland-1", 8));
        let _listener = touch_socket(&real);
        let stale = t.path().join("niri.wayland-9.1.sock"); // never bound
        std::fs::write(&stale, b"stale").unwrap();

        let mut env = temp_env(t.path());
        env.niri_socket = Some(stale.to_string_lossy().into_owned());
        assert_eq!(resolve_socket(&env).unwrap(), Some(real));
    }

    #[test]
    fn exact_wayland_display_match_wins_over_higher_pid() {
        let t = tempfile::tempdir().unwrap();
        let other = t.path().join(socket_filename("wayland-2", 9999));
        let ours = t.path().join(socket_filename("wayland-1", 10));
        let _a = touch_socket(&other);
        let _b = touch_socket(&ours);

        let env = temp_env(t.path());
        assert_eq!(resolve_socket(&env).unwrap(), Some(ours));
    }

    #[test]
    fn dead_pid_candidates_are_skipped() {
        let t = tempfile::tempdir().unwrap();
        let proc_root = t.path().join("proc");
        std::fs::create_dir_all(proc_root.join("12")).unwrap(); // 12 alive
        let dead = t.path().join(socket_filename("wayland-1", 11));
        let live = t.path().join(socket_filename("wayland-1", 12));
        let _a = touch_socket(&dead);
        let _b = touch_socket(&live);

        let env = temp_env(t.path());
        assert_eq!(resolve_socket(&env).unwrap(), Some(live));
    }

    #[test]
    fn niri_desktop_without_a_socket_is_an_actionable_error() {
        let t = tempfile::tempdir().unwrap();
        let env = temp_env(t.path());
        let err = resolve_socket(&env).unwrap_err().to_string();
        assert!(err.contains("niri session detected"), "got: {err}");
        assert!(err.contains("NIRI_SOCKET"), "got: {err}");
    }

    #[test]
    fn non_niri_desktop_without_a_socket_is_not_an_error() {
        let t = tempfile::tempdir().unwrap();
        let mut env = temp_env(t.path());
        env.current_desktop = Some("Hyprland".to_string());
        assert_eq!(resolve_socket(&env).unwrap(), None);
    }

    #[test]
    fn missing_runtime_dir_is_not_an_error() {
        let t = tempfile::tempdir().unwrap();
        let mut env = temp_env(&t.path().join("does-not-exist"));
        env.current_desktop = None;
        assert_eq!(resolve_socket(&env).unwrap(), None);
    }

    #[test]
    fn parses_a_layers_reply_and_drops_pid() {
        let body = r#"{"Ok":{"Layers":[
            {"namespace":"noctalia-bar","output":"eDP-1","layer":"Top","keyboard_interactivity":"None"},
            {"namespace":"noctalia-overview","output":"eDP-1","layer":"Overlay","keyboard_interactivity":"Exclusive"}
        ]}}"#;
        let layers = parse_layers_reply(body).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].namespace, "noctalia-bar");
        assert_eq!(layers[0].output, "eDP-1");
        assert_eq!(layers[0].layer, "Top");
        assert_eq!(layers[0].pid, None, "niri never reports a pid");
    }

    #[test]
    fn tolerates_unknown_fields_from_a_newer_compositor() {
        let body = r#"{"Ok":{"Layers":[
            {"namespace":"x","output":"eDP-1","layer":"Top","brand_new_field":true}
        ]}}"#;
        assert_eq!(parse_layers_reply(body).unwrap().len(), 1);
    }

    #[test]
    fn tolerates_unknown_reply_variants_gracefully() {
        // Wrong variant: an error, but a message that names what was received.
        let err = parse_layers_reply(r#"{"Ok":"Handled"}"#).unwrap_err().to_string();
        assert!(err.contains("Layers"), "got: {err}");
        let err = parse_layers_reply(r#"{"Ok":{"Windows":[]}}"#).unwrap_err().to_string();
        assert!(err.contains("Layers"), "got: {err}");
    }

    #[test]
    fn surfaces_an_error_reply() {
        let err = parse_layers_reply(r#"{"Err":"unknown request"}"#).unwrap_err().to_string();
        assert!(err.contains("unknown request"), "got: {err}");
    }

    #[test]
    fn empty_layers_reply_is_zero_layers_not_a_failure() {
        assert!(parse_layers_reply(r#"{"Ok":{"Layers":[]}}"#).unwrap().is_empty());
    }

    #[test]
    fn malformed_reply_is_an_error() {
        assert!(parse_layers_reply("garbage").is_err());
        assert!(parse_layers_reply("{}").is_err());
    }

    #[test]
    fn request_round_trips_against_a_fake_compositor() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("niri.wayland-1.5.sock");
        let listener = touch_socket(&sock);

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "\"Layers\"");
            stream.write_all(b"{\"Ok\":{\"Layers\":[]}}\n").unwrap();
            stream.flush().unwrap();
            line
        });

        let reply = request(&sock, "\"Layers\"").unwrap();
        assert!(reply.contains("Ok"));
        assert_eq!(server.join().unwrap().trim(), "\"Layers\"");
    }

    #[test]
    fn unresponsive_socket_yields_none_not_a_hang() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("niri.wayland-1.6.sock");
        // Listener that accepts and never replies.
        let listener = touch_socket(&sock);
        let handle = std::thread::spawn(move || {
            let _conn = listener.accept();
            std::thread::sleep(Duration::from_secs(3));
        });
        assert_eq!(request(&sock, "\"Layers\""), None);
        assert!(!probe(&sock));
        drop(handle);
    }

    #[test]
    fn probe_reports_false_for_a_nonexistent_socket() {
        assert!(!probe(Path::new("/nonexistent/niri.sock")));
    }
}
