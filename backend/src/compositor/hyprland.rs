//! Hyprland specifics: instance socket discovery and `hyprctl layers -j` parsing.
//!
//! Hyprland is the compositor whose layer list carries a `pid`, which is what
//! the original implementation relied on exclusively. That path is preserved
//! here as one of two ways to establish ownership (see [`super::owns_layers`]).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{IPC_TIMEOUT, LayerSurface, SessionEnv};

/// `$XDG_RUNTIME_DIR/hypr/<signature>/.socket.sock`, if it exists. Presence is
/// the only reliable way to tell a live instance from a stale env var.
pub fn instance_socket(env: &SessionEnv, signature: &str) -> Option<PathBuf> {
    let path = env.runtime_dir.join("hypr").join(signature).join(".socket.sock");
    path.exists().then_some(path)
}

/// Query the live layer list. `None` on any failure — including the IPC timeout —
/// because a failed query is not evidence that a shell opened no layers.
pub fn layers(signature: &str) -> Option<Vec<LayerSurface>> {
    let raw = query_layers_json(signature)?;
    // A parse failure after a successful exec is an IPC-shape surprise, not an
    // empty compositor; surface it on stderr rather than silently reporting "none".
    match parse_layers(&raw) {
        Ok(layers) => Some(layers),
        Err(e) => {
            eprintln!("rice-cooker: warn: could not parse `hyprctl layers -j`: {e:#}");
            None
        }
    }
}

fn query_layers_json(signature: &str) -> Option<String> {
    // `timeout` bounds a wedged compositor. When it is absent (minimal images,
    // test sandboxes) fall back to a bare `hyprctl` so the call still works.
    let mut cmd = if which("timeout") {
        let mut c = Command::new("timeout");
        let secs = format!("{}", IPC_TIMEOUT.as_secs().max(1));
        c.args(["--signal=KILL", &secs, "hyprctl"]);
        c
    } else {
        Command::new("hyprctl")
    };
    cmd.args(["layers", "-j"])
        .env("HYPRLAND_INSTANCE_SIGNATURE", signature)
        .stdin(Stdio::null())
        .stderr(Stdio::null());

    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn which(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

#[derive(Deserialize)]
struct RawLayer {
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    pid: Option<u32>,
}

#[derive(Deserialize)]
struct RawLevels {
    #[serde(default)]
    levels: std::collections::BTreeMap<String, Vec<RawLayer>>,
}

/// Parse `hyprctl layers -j`.
///
/// Shape: `{ "<monitor>": { "levels": { "0": [ { namespace, pid, ... } ] } } }`.
/// The monitor name and the layer level both become part of a surface's
/// baseline identity, so both are preserved.
pub fn parse_layers(body: &str) -> Result<Vec<LayerSurface>> {
    let monitors: std::collections::BTreeMap<String, RawLevels> =
        serde_json::from_str(body).context("parsing hyprctl layers JSON")?;
    let mut out = Vec::new();
    for (monitor, raw) in monitors {
        for (level, surfaces) in raw.levels {
            for surface in surfaces {
                out.push(LayerSurface {
                    namespace: surface.namespace,
                    output: monitor.clone(),
                    layer: level.clone(),
                    pid: surface.pid,
                });
            }
        }
    }
    Ok(out)
}

/// A `Path` that is a live Hyprland signature socket, for diagnostics.
pub fn signature_from_socket(path: &Path) -> Option<String> {
    path.parent()
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_the_documented_shape() {
        let body = r#"{
            "eDP-1": {
              "levels": {
                "0": [{"address":"a","namespace":"waybar","pid":10}],
                "2": [{"address":"b","namespace":"quickshell","pid":42}]
              }
            },
            "DP-1": { "levels": { "0": [] } }
        }"#;
        let mut layers = parse_layers(body).unwrap();
        layers.sort_by(|a, b| a.namespace.cmp(&b.namespace));
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].namespace, "quickshell");
        assert_eq!(layers[0].output, "eDP-1");
        assert_eq!(layers[0].layer, "2");
        assert_eq!(layers[0].pid, Some(42));
        assert_eq!(layers[1].namespace, "waybar");
        assert_eq!(layers[1].layer, "0");
    }

    #[test]
    fn missing_pid_field_is_none_not_an_error() {
        let body = r#"{"eDP-1":{"levels":{"0":[{"namespace":"x"}]}}}"#;
        let layers = parse_layers(body).unwrap();
        assert_eq!(layers[0].pid, None);
    }

    #[test]
    fn empty_object_is_no_layers() {
        assert!(parse_layers("{}").unwrap().is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_layers("not json").is_err());
    }

    #[test]
    fn socket_path_is_derived_from_signature() {
        let env = SessionEnv {
            runtime_dir: PathBuf::from("/run/user/1000"),
            wayland_display: None,
            hyprland_signature: None,
            niri_socket: None,
            current_desktop: None,
            proc_root: PathBuf::from("/proc"),
        };
        assert_eq!(
            instance_socket(&env, "abc123"),
            None,
            "absent socket must not be reported as present"
        );
        assert_eq!(
            signature_from_socket(Path::new("/run/user/1000/hypr/abc123/.socket.sock")),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn ipc_timeout_is_one_second() {
        assert_eq!(IPC_TIMEOUT, Duration::from_secs(1));
    }
}
