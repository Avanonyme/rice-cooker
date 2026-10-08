//! Compositor abstraction: Hyprland and niri behind one interface.
//!
//! Three things differ between the two and are hidden here:
//!
//! 1. **Detection.** Hyprland exports `HYPRLAND_INSTANCE_SIGNATURE` and a
//!    per-instance socket under `$XDG_RUNTIME_DIR/hypr/<sig>/`. niri exports
//!    `NIRI_SOCKET`, but that variable is not reliable ([niri#2149]), so the
//!    socket filename is reconstructed from `WAYLAND_DISPLAY` and `/proc`.
//! 2. **Layer enumeration.** Hyprland is queried through the `hyprctl` binary
//!    (`hyprctl layers -j`); niri is queried over its IPC socket directly.
//! 3. **Ownership evidence.** Hyprland's layer list carries a `pid` per surface;
//!    niri's does not. So ownership is decided as
//!    *appeared since the baseline* AND (*pid matches* OR *namespace matches*).
//!    On niri only the namespace half can ever be satisfied.
//!
//! [niri#2149]: https://github.com/YaLTeR/niri/issues/2149

pub mod hyprland;
pub mod niri;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// IPC budget. Matches the deadline the old `hyprctl` path used
/// (`timeout --signal=KILL 1 hyprctl layers -j`).
pub const IPC_TIMEOUT: Duration = Duration::from_secs(1);

/// Applied when a catalog entry declares no `layer_namespaces`.
///
/// Hyprland needs no default because its pid evidence is sufficient; niri does,
/// because namespace is the only evidence it can offer. `quickshell` is the
/// argv0 basename for both upstream quickshell and the `noctalia-qs` fork, and
/// it is what `import Quickshell` renders as by default.
pub const DEFAULT_NIRI_NAMESPACE: &str = "^quickshell";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompositorId {
    Hyprland,
    Niri,
}

impl CompositorId {
    pub const ALL: [CompositorId; 2] = [CompositorId::Hyprland, CompositorId::Niri];

    pub fn as_str(self) -> &'static str {
        match self {
            CompositorId::Hyprland => "hyprland",
            CompositorId::Niri => "niri",
        }
    }
}

impl std::fmt::Display for CompositorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One layer-shell surface, normalised across compositors.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LayerSurface {
    pub namespace: String,
    pub output: String,
    pub layer: String,
    /// Hyprland reports the owning pid. niri does not, and this is always `None`.
    pub pid: Option<u32>,
}

impl LayerSurface {
    /// Identity used for the baseline multiset diff. Deliberately excludes `pid`:
    /// a compositor is free to respawn a surface with a new pid, and on niri
    /// there is no pid at all, so including it would make the diff
    /// platform-dependent.
    fn key(&self) -> (&str, &str, &str) {
        (&self.namespace, &self.output, &self.layer)
    }
}

/// Environment the compositor is detected from. Every field is injected rather
/// than read from `std::env` at the call site, so tests can build a session
/// without touching process globals.
#[derive(Debug, Clone)]
pub struct SessionEnv {
    pub runtime_dir: PathBuf,
    pub wayland_display: Option<String>,
    pub hyprland_signature: Option<String>,
    pub niri_socket: Option<String>,
    pub current_desktop: Option<String>,
    /// Explicit compositor override from `RICE_COOKER_COMPOSITOR` (the Nix
    /// module writes it). `None` means detect as before.
    pub compositor_override: Option<String>,
    /// `/proc` on Linux; a fixture directory in tests.
    pub proc_root: PathBuf,
}

impl SessionEnv {
    pub fn from_process() -> Self {
        let non_empty = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        Self {
            runtime_dir: non_empty("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/run/user/0")),
            wayland_display: non_empty("WAYLAND_DISPLAY"),
            hyprland_signature: non_empty("HYPRLAND_INSTANCE_SIGNATURE"),
            niri_socket: non_empty("NIRI_SOCKET"),
            current_desktop: non_empty("XDG_CURRENT_DESKTOP"),
            compositor_override: non_empty("RICE_COOKER_COMPOSITOR"),
            proc_root: PathBuf::from("/proc"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compositor {
    Hyprland { signature: String },
    Niri { socket: PathBuf },
}

impl Compositor {
    /// Hyprland first: a niri-nested session is not a thing, but a Hyprland
    /// session started from a niri one would inherit `NIRI_SOCKET`, so
    /// signature-before-socket is the safe order.
    pub fn detect(env: &SessionEnv) -> Result<Self> {
        if let Some(raw) = env.compositor_override.as_deref() {
            return detect_override(env, raw);
        }

        let hypr_sig = env.hyprland_signature.as_deref();
        if let Some(sig) = hypr_sig
            && hyprland::instance_socket(env, sig).is_some()
        {
            return Ok(Compositor::Hyprland {
                signature: sig.to_string(),
            });
        }
        if let Some(socket) = niri::resolve_socket(env)? {
            return Ok(Compositor::Niri { socket });
        }
        match hypr_sig {
            Some(sig) => anyhow::bail!(
                "Hyprland instance {sig} is set but its IPC socket is missing from {}",
                env.runtime_dir.display()
            ),
            None => anyhow::bail!(
                "no supported compositor: set HYPRLAND_INSTANCE_SIGNATURE (Hyprland) \
                 or run under a niri session with a reachable IPC socket"
            ),
        }
    }

    pub fn id(&self) -> CompositorId {
        match self {
            Compositor::Hyprland { .. } => CompositorId::Hyprland,
            Compositor::Niri { .. } => CompositorId::Niri,
        }
    }

    /// `None` means the query failed or timed out — never "no surfaces".
    /// Callers must keep that distinction: an IPC failure is not evidence that
    /// the shell failed to open a layer.
    pub fn layers(&self) -> Option<Vec<LayerSurface>> {
        match self {
            Compositor::Hyprland { signature } => hyprland::layers(signature),
            Compositor::Niri { socket } => niri::layers(socket),
        }
    }

    /// Whether a surface counts as ours, given the pids we launched and the
    /// namespaces the rice declared.
    pub fn ownership<'a>(
        &self,
        pids: &'a [u32],
        declared: &'a [Regex],
        baseline: &'a [LayerSurface],
    ) -> Ownership<'a> {
        Ownership {
            pids,
            namespaces: declared,
            baseline,
        }
    }
}

/// Explicit override from `RICE_COOKER_COMPOSITOR`. Case-insensitive on the
/// name, and it never falls back to the other compositor: naming a compositor
/// whose socket is not reachable is an error that names the missing socket.
fn detect_override(env: &SessionEnv, raw: &str) -> Result<Compositor> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "hyprland" => {
            let Some(sig) = env.hyprland_signature.as_deref() else {
                anyhow::bail!(
                    "RICE_COOKER_COMPOSITOR=hyprland but HYPRLAND_INSTANCE_SIGNATURE is unset; \
                     expected a live socket at {}/hypr/<signature>/.socket.sock",
                    env.runtime_dir.display()
                );
            };
            if hyprland::instance_socket(env, sig).is_none() {
                anyhow::bail!(
                    "RICE_COOKER_COMPOSITOR=hyprland but the Hyprland instance socket \
                     {}/hypr/{sig}/.socket.sock is not reachable",
                    env.runtime_dir.display()
                );
            }
            Ok(Compositor::Hyprland {
                signature: sig.to_string(),
            })
        }
        "niri" => match niri::resolve_socket(env) {
            Ok(Some(socket)) => Ok(Compositor::Niri { socket }),
            Ok(None) => anyhow::bail!(
                "RICE_COOKER_COMPOSITOR=niri but no niri IPC socket is reachable \
                 (looked for niri.<WAYLAND_DISPLAY>.<pid>.sock in {})",
                env.runtime_dir.display()
            ),
            Err(e) => anyhow::bail!("RICE_COOKER_COMPOSITOR=niri: {e:#}"),
        },
        other => anyhow::bail!(
            "unknown RICE_COOKER_COMPOSITOR value {other:?}; expected `hyprland` or `niri`"
        ),
    }
}

/// Everything needed to decide whether a layer surface belongs to the shell we
/// just launched.
pub struct Ownership<'a> {
    pub pids: &'a [u32],
    /// Patterns from the catalog entry's `layer_namespaces`.
    pub namespaces: &'a [Regex],
    /// Snapshot taken after eviction and before launch.
    pub baseline: &'a [LayerSurface],
}

/// Pure decision so it can be unit-tested without a compositor.
///
/// A surface is ours when it was not present in the baseline *and* either its
/// pid is one we launched, or its namespace matches a declared pattern. On niri
/// `pid` is always `None`, so only the namespace half can match — which is why
/// `DEFAULT_NIRI_NAMESPACE` exists.
pub fn owns_layers(snapshot: &[LayerSurface], ownership: &Ownership<'_>) -> bool {
    for surface in new_surfaces(snapshot, ownership.baseline) {
        let pid_match = surface
            .pid
            .is_some_and(|pid| ownership.pids.contains(&pid));
        let ns_match = ownership
            .namespaces
            .iter()
            .any(|re| re.is_match(&surface.namespace));
        if pid_match || ns_match {
            return true;
        }
    }
    false
}

/// Surfaces present in `snapshot` but not accounted for by `baseline`.
///
/// Multiset, not set: a surface counts as pre-existing only as many times as
/// that exact (namespace, output, layer) triple appears in the baseline. This
/// stops a leftover surface from the shell we just evicted being read as
/// evidence that the new shell came up.
fn new_surfaces<'s>(snapshot: &'s [LayerSurface], baseline: &[LayerSurface]) -> Vec<&'s LayerSurface> {
    let mut remaining: Vec<&LayerSurface> = baseline.iter().collect();
    let mut new = Vec::new();
    for surface in snapshot {
        match remaining.iter().position(|c| c.key() == surface.key()) {
            Some(idx) => {
                remaining.swap_remove(idx);
            }
            None => new.push(surface),
        }
    }
    new
}

/// Count of surfaces the baseline does not account for. `owns_layers` only needs
/// the boolean, but tests and diagnostics want the multiplicity.
pub fn count_new_surfaces(snapshot: &[LayerSurface], baseline: &[LayerSurface]) -> usize {
    new_surfaces(snapshot, baseline).len()
}

/// Compile a catalog entry's `layer_namespaces`, falling back to the compositor's
/// default so an entry that declares nothing still verifies on niri.
pub fn compile_namespaces(
    id: CompositorId,
    declared: &[String],
) -> Result<Vec<Regex>> {
    let patterns: Vec<&str> = if declared.is_empty() {
        match id {
            CompositorId::Hyprland => Vec::new(),
            CompositorId::Niri => vec![DEFAULT_NIRI_NAMESPACE],
        }
    } else {
        declared.iter().map(String::as_str).collect()
    };
    patterns
        .into_iter()
        .map(|p| {
            Regex::new(p)
                .map_err(|e| anyhow::anyhow!("invalid layer namespace pattern {p:?}: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(ns: &str, pid: Option<u32>) -> LayerSurface {
        LayerSurface {
            namespace: ns.to_string(),
            output: "eDP-1".to_string(),
            layer: "top".to_string(),
            pid,
        }
    }

    fn re(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap()
    }

    #[test]
    fn new_surface_with_matching_pid_is_ours() {
        let baseline = vec![surface("waybar", Some(10))];
        let snapshot = vec![surface("waybar", Some(10)), surface("quickshell", Some(42))];
        let own = Ownership {
            pids: &[42],
            namespaces: &[],
            baseline: &baseline,
        };
        assert!(owns_layers(&snapshot, &own));
    }

    #[test]
    fn preexisting_surface_is_never_ours_even_with_matching_pid() {
        // The shell we evicted had this pid; its surviving surface must not be
        // read as the new shell having come up.
        let baseline = vec![surface("quickshell", Some(42))];
        let snapshot = baseline.clone();
        let own = Ownership {
            pids: &[42],
            namespaces: &[],
            baseline: &baseline,
        };
        assert!(!owns_layers(&snapshot, &own));
    }

    #[test]
    fn multiset_baseline_does_not_hide_a_second_identical_surface() {
        // One `quickshell` surface pre-existed; two exist now. The extra one is new.
        let baseline = vec![surface("quickshell", None)];
        let snapshot = vec![surface("quickshell", None), surface("quickshell", None)];
        let own = Ownership {
            pids: &[],
            namespaces: &[re("^quickshell")],
            baseline: &baseline,
        };
        assert!(owns_layers(&snapshot, &own));
        assert_eq!(count_new_surfaces(&snapshot, &baseline), 1);
    }

    #[test]
    fn niri_path_matches_on_namespace_alone() {
        // niri never reports a pid; namespace is the only evidence available.
        let snapshot = vec![surface("noctalia-bar", None)];
        let own = Ownership {
            pids: &[],
            namespaces: &[re("^noctalia-")],
            baseline: &[],
        };
        assert!(owns_layers(&snapshot, &own));
    }

    #[test]
    fn niri_path_fails_when_namespace_matches_nothing() {
        let snapshot = vec![surface("some-other-shell", None)];
        let own = Ownership {
            pids: &[],
            namespaces: &[re("^noctalia-")],
            baseline: &[],
        };
        assert!(!owns_layers(&snapshot, &own));
    }

    #[test]
    fn unrelated_new_surface_is_not_ours() {
        let snapshot = vec![surface("notify-osd", Some(99))];
        let own = Ownership {
            pids: &[42],
            namespaces: &[re("^quickshell")],
            baseline: &[],
        };
        assert!(!owns_layers(&snapshot, &own));
    }

    #[test]
    fn empty_snapshot_is_not_ownership() {
        let own = Ownership {
            pids: &[42],
            namespaces: &[re("^quickshell")],
            baseline: &[],
        };
        assert!(!owns_layers(&[], &own));
    }

    #[test]
    fn output_and_layer_participate_in_the_baseline_key() {
        let baseline = vec![LayerSurface {
            namespace: "quickshell".into(),
            output: "eDP-1".into(),
            layer: "top".into(),
            pid: None,
        }];
        let moved = vec![LayerSurface {
            namespace: "quickshell".into(),
            output: "DP-1".into(),
            layer: "top".into(),
            pid: None,
        }];
        // Same namespace, different monitor: a new surface.
        assert_eq!(count_new_surfaces(&moved, &baseline), 1);
    }

    #[test]
    fn hyprland_default_has_no_namespace_requirement() {
        assert!(compile_namespaces(CompositorId::Hyprland, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn niri_default_requires_the_quickshell_namespace() {
        let compiled = compile_namespaces(CompositorId::Niri, &[]).unwrap();
        assert_eq!(compiled.len(), 1);
        assert!(compiled[0].is_match("quickshell"));
    }

    #[test]
    fn declared_namespaces_override_the_default() {
        let compiled =
            compile_namespaces(CompositorId::Niri, &["^noctalia-".to_string()]).unwrap();
        assert!(compiled[0].is_match("noctalia-overview"));
        assert!(!compiled[0].is_match("quickshell"));
    }

    #[test]
    fn invalid_namespace_pattern_is_an_error_not_a_silent_skip() {
        let err = compile_namespaces(CompositorId::Niri, &["[unclosed".to_string()]).unwrap_err();
        assert!(err.to_string().contains("invalid layer namespace"));
    }

    #[test]
    fn compositor_id_round_trips_through_serde() {
        for (id, wire) in [
            (CompositorId::Hyprland, "\"hyprland\""),
            (CompositorId::Niri, "\"niri\""),
        ] {
            assert_eq!(serde_json::to_string(&id).unwrap(), wire);
            let back: CompositorId = serde_json::from_str(wire).unwrap();
            assert_eq!(back, id);
            assert_eq!(id.as_str(), wire.trim_matches('"'));
        }
    }

    // ── RICE_COOKER_COMPOSITOR override ──────────────────────────────────────

    fn session_env(
        root: &std::path::Path,
        hyprland_signature: Option<&str>,
        niri_socket: Option<&str>,
        compositor_override: Option<&str>,
    ) -> SessionEnv {
        SessionEnv {
            runtime_dir: root.to_path_buf(),
            wayland_display: Some("wayland-1".to_string()),
            hyprland_signature: hyprland_signature.map(str::to_string),
            niri_socket: niri_socket.map(str::to_string),
            current_desktop: None,
            compositor_override: compositor_override.map(str::to_string),
            proc_root: root.join("proc"),
        }
    }

    fn touch_hyprland_socket(root: &std::path::Path, signature: &str) {
        let dir = root.join("hypr").join(signature);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".socket.sock"), b"").unwrap();
    }

    #[test]
    fn override_niri_is_case_insensitive() {
        use std::os::unix::net::UnixListener;
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("niri.wayland-1.7.sock");
        let _listener = UnixListener::bind(&sock).unwrap();

        for raw in ["niri", "Niri", "NIRI"] {
            let env = session_env(
                t.path(),
                None,
                Some(&sock.to_string_lossy()),
                Some(raw),
            );
            assert_eq!(
                Compositor::detect(&env).unwrap(),
                Compositor::Niri { socket: sock.clone() }
            );
        }
    }

    #[test]
    fn override_hyprland_is_case_insensitive_and_uses_the_live_socket() {
        let t = tempfile::tempdir().unwrap();
        let sig = "abc123";
        touch_hyprland_socket(t.path(), sig);
        let env = session_env(t.path(), Some(sig), None, Some("HyPrLaNd"));
        assert_eq!(
            Compositor::detect(&env).unwrap(),
            Compositor::Hyprland { signature: sig.to_string() }
        );
    }

    #[test]
    fn override_hyprland_missing_socket_names_the_socket() {
        let t = tempfile::tempdir().unwrap();
        let sig = "abc123";
        let env = session_env(t.path(), Some(sig), None, Some("hyprland"));
        let err = Compositor::detect(&env).unwrap_err().to_string();
        assert!(err.contains("hyprland"), "got: {err}");
        assert!(err.contains(".socket.sock"), "got: {err}");
        assert!(err.contains(sig), "got: {err}");
    }

    #[test]
    fn override_niri_does_not_fall_back_to_hyprland() {
        let t = tempfile::tempdir().unwrap();
        // A live Hyprland socket is present, so a fallback would succeed; the
        // override must still fail because it names niri, whose socket is absent.
        touch_hyprland_socket(t.path(), "sig");
        let env = session_env(t.path(), Some("sig"), None, Some("niri"));
        let err = Compositor::detect(&env).unwrap_err().to_string();
        assert!(err.contains("niri"), "got: {err}");
        assert!(err.contains("sock"), "got: {err}");
    }

    #[test]
    fn override_niri_missing_socket_names_the_socket() {
        let t = tempfile::tempdir().unwrap();
        let env = session_env(t.path(), None, None, Some("niri"));
        let err = Compositor::detect(&env).unwrap_err().to_string();
        assert!(err.contains("niri"), "got: {err}");
        assert!(err.contains("sock"), "got: {err}");
    }

    #[test]
    fn override_hyprland_does_not_fall_back_to_niri() {
        use std::os::unix::net::UnixListener;
        let t = tempfile::tempdir().unwrap();
        // A live niri socket is present, so a fallback would succeed; the
        // override must still fail because it names hyprland without a
        // signature, so there is no Hyprland socket to reach.
        let sock = t.path().join("niri.wayland-1.7.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        let env = session_env(
            t.path(),
            None,
            Some(&sock.to_string_lossy()),
            Some("hyprland"),
        );
        let err = Compositor::detect(&env).unwrap_err().to_string();
        assert!(err.contains("hyprland"), "got: {err}");
        assert!(err.contains(".socket.sock"), "got: {err}");
    }

    #[test]
    fn unknown_override_value_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let env = session_env(t.path(), None, None, Some("sway"));
        let err = Compositor::detect(&env).unwrap_err().to_string();
        assert!(err.contains("unknown RICE_COOKER_COMPOSITOR value"), "got: {err}");
    }

    #[test]
    fn unset_override_still_detects_hyprland() {
        let t = tempfile::tempdir().unwrap();
        let sig = "abc123";
        touch_hyprland_socket(t.path(), sig);
        let env = session_env(t.path(), Some(sig), None, None);
        assert_eq!(
            Compositor::detect(&env).unwrap(),
            Compositor::Hyprland { signature: sig.to_string() }
        );
    }

    #[test]
    fn unset_override_still_detects_niri() {
        use std::os::unix::net::UnixListener;
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("niri.wayland-1.7.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        let env = session_env(
            t.path(),
            None,
            Some(&sock.to_string_lossy()),
            None,
        );
        assert_eq!(
            Compositor::detect(&env).unwrap(),
            Compositor::Niri { socket: sock }
        );
    }
}
