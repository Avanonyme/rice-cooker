//! Platform detection, and the Nix-side realize primitives.
//!
//! Two things live here:
//!
//! 1. [`PlatformId`] — which package manager backs the install. Detected from
//!    the filesystem, with an explicit override, never guessed from `PATH`
//!    alone.
//! 2. The Nix realize path — building a rice flake into a store path, deriving
//!    the launch argv from the catalog, and emitting the two-line module snippet
//!    that is what "install" means on a declarative system.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::catalog::{LaunchKind, RiceEntry};
use crate::paths::Paths;
use crate::process;

/// Shells that fight a rice for the same layer surfaces. Mirrors the list the
/// Electron main process used to hardcode, plus `ironbar`.
pub const CONFLICTING_SHELLS: &[&str] = &["waybar", "ags", "astal", "eww", "yambar", "ironbar"];

/// Machine-readable environment report.
///
/// Exists so the UI does not reimplement platform and compositor detection in
/// TypeScript — it did, and got niri wrong.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvReport {
    pub platform: Option<&'static str>,
    pub compositor: Option<&'static str>,
    pub session_type: Option<String>,
    pub supported: bool,
    /// Why `supported` is false, most actionable first.
    pub reasons: Vec<String>,
    pub conflicting_shells: Vec<String>,
}

pub fn env_report() -> EnvReport {
    let mut reasons = Vec::new();

    let platform = match detect() {
        Ok(p) => Some(p),
        Err(e) => {
            reasons.push(format!("{e:#}"));
            None
        }
    };

    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    if session_type.as_deref() != Some("wayland") {
        reasons.push(format!(
            "not a Wayland session (XDG_SESSION_TYPE={})",
            session_type.as_deref().unwrap_or("<unset>")
        ));
    }

    let compositor = match process::check_graphical_session() {
        Ok(session) => Some(session.compositor_id().as_str()),
        Err(e) => {
            reasons.push(format!("{e:#}"));
            None
        }
    };

    let conflicting_shells = conflicting_shells();
    if !conflicting_shells.is_empty() {
        reasons.push(format!(
            "another shell is already running: {}",
            conflicting_shells.join(", ")
        ));
    }

    EnvReport {
        platform: platform.map(PlatformId::as_str),
        compositor,
        session_type,
        supported: reasons.is_empty(),
        reasons,
        conflicting_shells,
    }
}

/// Which of the rival shells are running. Scans procfs, so this covers the
/// current user's processes and any other user's the caller can read.
pub fn conflicting_shells() -> Vec<String> {
    let matcher = process::ShellMatcher::new(CONFLICTING_SHELLS);
    process::running_shell_names(std::path::Path::new(process::PROC_ROOT), &matcher)
        .unwrap_or_default()
}

/// Environment variable that forces a platform, mainly for tests and for a host
/// that carries both package managers.
pub const PLATFORM_ENV: &str = "RICE_COOKER_PLATFORM";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformId {
    /// Arch: `paru`/`yay` + `pacman`, symlink into `$XDG_CONFIG_HOME`. The
    /// original behaviour, untouched.
    Arch,
    /// Nix: build a flake, launch from the store, and treat "install" as
    /// emitting configuration rather than mutating state.
    Nix,
}

impl PlatformId {
    pub fn as_str(self) -> &'static str {
        match self {
            PlatformId::Arch => "arch",
            PlatformId::Nix => "nix",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "arch" | "pacman" => Ok(PlatformId::Arch),
            "nix" | "nixos" => Ok(PlatformId::Nix),
            other => bail!("unknown {PLATFORM_ENV} value {other:?}; expected `arch` or `nix`"),
        }
    }
}

pub fn detect() -> Result<PlatformId> {
    let over = std::env::var(PLATFORM_ENV).ok();
    detect_in(Path::new("/"), over.as_deref())
}

/// Split out so the marker logic is testable against a fixture root.
pub fn detect_in(root: &Path, override_flag: Option<&str>) -> Result<PlatformId> {
    if let Some(raw) = override_flag {
        // An explicit override that cannot be parsed is a user error. Falling
        // through to the markers would silently pick a package manager the user
        // did not ask for, and install with the wrong tool.
        return PlatformId::parse(raw);
    }
    // /etc/NIXOS before /etc/arch-release: a NixOS host that has an
    // arch-release left behind by a container image is still NixOS.
    if root.join("etc/NIXOS").exists() {
        return Ok(PlatformId::Nix);
    }
    if root.join("etc/arch-release").exists() {
        return Ok(PlatformId::Arch);
    }
    // No marker either way (a bare container, a non-NixOS host using nix).
    Ok(if which("nix").is_some() && which("pacman").is_none() {
        PlatformId::Nix
    } else {
        PlatformId::Arch
    })
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Build a rice flake and return its store path.
///
/// `--no-link` on purpose: no `result` symlink is left in the caller's cwd, so a
/// preview leaves nothing behind but store content.
pub fn build_store_path(flake_ref: &str, package_attr: &str) -> Result<PathBuf> {
    ensure!(
        !flake_ref.starts_with('-'),
        "refusing flake reference starting with '-': {flake_ref:?}"
    );
    ensure!(
        !package_attr.starts_with('-'),
        "refusing package attribute starting with '-': {package_attr:?}"
    );
    let target = format!("{flake_ref}#{package_attr}");
    eprintln!("build_store_path: nix build {target}");

    // stderr is inherited, not captured: nix's progress bar and build logs are
    // the only feedback during a multi-minute compile, and `output()` would
    // swallow them so a compiling quickshell looks like a hang.
    let mut child = Command::new("nix")
        .args([
            "build",
            "--no-link",
            "--print-out-paths",
            "--no-write-lock-file",
        ])
        .arg(&target)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning nix build")?;

    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .context("reading nix build stdout")?;
    }
    let status = child.wait().context("waiting for nix build")?;
    if !status.success() {
        bail!("nix build {target} failed (exit {:?})", status.code());
    }

    let stdout = stdout.as_str();
    let path = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("nix build {target} produced no output path; expected --print-out-paths")
        })?;
    ensure!(
        path.starts_with("/nix/store/"),
        "nix build {target} returned something that is not a store path: {path:?}"
    );
    Ok(PathBuf::from(path))
}

/// Derive the launch argv from the catalog entry, resolved against the store path.
///
/// A bare binary name is rewritten to `<store>/bin/<name>` when present. That
/// matters because a Nix-packaged shell is usually a *wrapper* carrying its own
/// `-p <config-dir>` flag, and picking up whatever `qs` is on `PATH` would run a
/// different shell than the one just built.
pub fn launch_argv(entry: &RiceEntry, name: &str, store: &Path) -> Vec<String> {
    let base = match &entry.launch {
        Some(launch) => match launch.kind {
            LaunchKind::Argv => {
                if launch.argv.is_empty() {
                    vec![name.to_string()]
                } else {
                    launch.argv.clone()
                }
            }
            LaunchKind::Quickshell => vec![
                launch.bin.clone().unwrap_or_else(|| "quickshell".to_string()),
                "-c".to_string(),
                launch
                    .config
                    .clone()
                    .unwrap_or_else(|| name.to_string()),
            ],
        },
        None => vec!["quickshell".to_string(), "-c".to_string(), name.to_string()],
    };
    resolve_against_store(base, store)
}

fn resolve_against_store(mut argv: Vec<String>, store: &Path) -> Vec<String> {
    let Some(first) = argv.first_mut() else {
        return argv;
    };
    // Already an explicit path (or something the shell should resolve itself).
    if first.contains('/') {
        return argv;
    }
    let candidate = store.join("bin").join(&*first);
    if candidate.is_file() {
        *first = candidate.to_string_lossy().into_owned();
    }
    argv
}

/// The config a user adopts for a durable install.
///
/// On Nix, "install" is necessarily an act of configuration — the tool cannot
/// mutate a declarative system — so the install artifact is this snippet, and
/// the only durable state rice-cooker keeps is the path it wrote it to.
pub fn install_snippet(name: &str) -> String {
    format!(
        "# rice-cooker: adopt this into your Home Manager configuration.\n\
         #\n\
         # `enable` is what forces this shell over whatever your own config\n\
         # already starts, and `shell` selects from the flake's built-in list.\n\
         #\n\
         # The rice's module has to be imported by you: a module cannot choose its\n\
         # imports from configuration values, and `programs.rice-cooker` asserts\n\
         # that the option below exists rather than importing it for you.\n\
         imports = [ inputs.{name}.homeManagerModules.default ];\n\
         \n\
         programs.rice-cooker = {{\n\
         \x20 enable = true;\n\
         \x20 shell = \"{name}\";\n\
         \x20 rices.{name} = inputs.{name};\n\
         }};\n"
    )
}

/// Write the install snippet, returning where it landed.
pub fn write_install_snippet(paths: &Paths, name: &str) -> Result<PathBuf> {
    let dir = paths.data_home.join("install-snippets");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{name}.nix"));
    std::fs::write(&path, install_snippet(name))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{LaunchDecl, RiceEntry};

    fn entry_with_launch(launch: Option<LaunchDecl>) -> RiceEntry {
        RiceEntry {
            display_name: "X".into(),
            creator_name: "x".into(),
            repo: "https://x".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            symlink_src: None,
            symlink_dst: None,
            package_managed: false,
            preview_deps: vec![],
            install_deps: vec![],
            interactive: false,
            compositors: vec![crate::compositor::CompositorId::Niri],
            layer_namespaces: vec!["^caelestia-".into()],
            launch,
            nix: None,
        }
    }

    fn store_with_bin(bins: Vec<&str>) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("bin")).unwrap();
        for bin in bins {
            std::fs::write(t.path().join("bin").join(bin), b"#!/bin/sh\n").unwrap();
        }
        t
    }

    #[test]
    fn explicit_platform_override_wins() {
        // Even with the arch marker present, the override decides.
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("etc")).unwrap();
        std::fs::write(t.path().join("etc/arch-release"), b"").unwrap();
        assert_eq!(detect_in(t.path(), Some("nix")).unwrap(), PlatformId::Nix);
        assert_eq!(detect_in(t.path(), Some("arch")).unwrap(), PlatformId::Arch);
    }

    #[test]
    fn nixos_marker_takes_priority_over_a_stray_arch_release() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("etc")).unwrap();
        std::fs::write(t.path().join("etc/arch-release"), b"").unwrap();
        std::fs::write(t.path().join("etc/NIXOS"), b"").unwrap();
        assert_eq!(detect_in(t.path(), None).unwrap(), PlatformId::Nix);
    }

    #[test]
    fn arch_marker_alone_selects_arch() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("etc")).unwrap();
        std::fs::write(t.path().join("etc/arch-release"), b"").unwrap();
        // No env mutation needed: a marker alone decides, before any PATH probe.
        assert_eq!(detect_in(t.path(), None).unwrap(), PlatformId::Arch);
    }

    #[test]
    fn unknown_platform_override_value_is_reported() {
        let err = PlatformId::parse("fedora").unwrap_err().to_string();
        assert!(err.contains("unknown"), "got: {err}");
        // An unparseable override must not silently become Arch — not even on a
        // host with no marker of its own, which is what this fixture root is.
        let t = tempfile::tempdir().unwrap();
        let err = detect_in(t.path(), Some("fedora")).unwrap_err().to_string();
        assert!(err.contains("unknown"), "got: {err}");
    }

    #[test]
    fn no_override_and_no_marker_still_resolves() {
        // The fallback must stay infallible: only an explicit bad value errors.
        let t = tempfile::tempdir().unwrap();
        assert!(detect_in(t.path(), None).is_ok());
    }

    #[test]
    fn platform_names_round_trip() {
        for id in [PlatformId::Arch, PlatformId::Nix] {
            assert_eq!(PlatformId::parse(id.as_str()).unwrap(), id);
        }
    }

    #[test]
    fn argv_launch_resolves_a_bare_binary_into_the_store() {
        let store = store_with_bin(vec!["caelestia-shell"]);
        let entry = entry_with_launch(Some(LaunchDecl {
            kind: LaunchKind::Argv,
            bin: None,
            config: None,
            argv: vec!["caelestia-shell".into()],
        }));
        let argv = launch_argv(&entry, "niri-caelestia", store.path());
        assert_eq!(
            argv,
            vec![store
                .path()
                .join("bin/caelestia-shell")
                .to_string_lossy()
                .into_owned()]
        );
    }

    #[test]
    fn argv_launch_keeps_a_binary_the_store_does_not_have() {
        // `noctalia` comes from the user's profile, not from the rice's store path.
        let store = store_with_bin(vec!["caelestia-shell"]);
        let entry = entry_with_launch(Some(LaunchDecl {
            kind: LaunchKind::Argv,
            bin: None,
            config: None,
            argv: vec!["noctalia".into()],
        }));
        assert_eq!(launch_argv(&entry, "noctalia", store.path()), vec!["noctalia"]);
    }

    #[test]
    fn argv_launch_leaves_an_explicit_path_alone() {
        let store = store_with_bin(vec!["caelestia-shell"]);
        let entry = entry_with_launch(Some(LaunchDecl {
            kind: LaunchKind::Argv,
            bin: None,
            config: None,
            argv: vec!["/usr/bin/foo".into()],
        }));
        assert_eq!(
            launch_argv(&entry, "x", store.path()),
            vec!["/usr/bin/foo"]
        );
    }

    #[test]
    fn absent_launch_defaults_to_quickshell_dash_c() {
        let store = store_with_bin(vec!["quickshell"]);
        let entry = entry_with_launch(None);
        assert_eq!(
            launch_argv(&entry, "dms", store.path()),
            vec![
                store
                    .path()
                    .join("bin/quickshell")
                    .to_string_lossy()
                    .into_owned(),
                "-c".into(),
                "dms".into()
            ]
        );
    }

    #[test]
    fn quickshell_launch_honours_bin_and_config_overrides() {
        let store = store_with_bin(vec![]);
        let entry = entry_with_launch(Some(LaunchDecl {
            kind: LaunchKind::Quickshell,
            bin: Some("noctalia-qs".into()),
            config: Some("noctalia".into()),
            argv: vec![],
        }));
        assert_eq!(
            launch_argv(&entry, "noctalia", store.path()),
            vec!["noctalia-qs", "-c", "noctalia"]
        );
    }

    #[test]
    fn install_snippet_is_adoptable_as_is() {
        let snippet = install_snippet("niri-caelestia");
        assert!(snippet.contains("programs.rice-cooker"));
        assert!(snippet.contains("enable = true"));
        assert!(snippet.contains("shell = \"niri-caelestia\""));
        // Without this line the module's own assertion would fail on the snippet
        // it just told the user to paste.
        assert!(snippet.contains("rices.niri-caelestia = inputs.niri-caelestia"));
        assert!(snippet.contains("inputs.niri-caelestia.homeManagerModules.default"));
    }

    #[test]
    fn write_install_snippet_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().to_path_buf();
        let paths = Paths::at_roots(home.clone(), home.join("cache"), home.join("data"));
        let first = write_install_snippet(&paths, "dms").unwrap();
        let second = write_install_snippet(&paths, "dms").unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with(home.join("data")));
        assert!(std::fs::read_to_string(&first).unwrap().contains("dms"));
    }

    #[test]
    fn build_store_path_refuses_flag_injection() {
        let err = build_store_path("-evil", "default").unwrap_err().to_string();
        assert!(err.contains("refusing flake reference"), "got: {err}");
        let err = build_store_path("github:x/y", "-rf").unwrap_err().to_string();
        assert!(err.contains("refusing package attribute"), "got: {err}");
    }

}
