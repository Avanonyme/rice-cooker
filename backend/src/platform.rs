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

pub use crate::catalog::PreviewMode;
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

/// Is `bin` on `PATH`? Exposed so preflight can report a missing shell rather
/// than failing later with something opaque.
pub fn which(bin: &str) -> Option<PathBuf> {
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

/// Fetch a rice's tree at a pinned revision and return its store path.
///
/// `builtins.fetchGit` rather than `nix build`: a configuration-only rice has no
/// derivation to build, and its tree still has to land in the store. Impure, and
/// deliberately so — the revision is pinned by the catalog, so the result is
/// still deterministic.
pub fn fetch_source(repo: &str, rev: &str) -> Result<PathBuf> {
    for (label, value) in [("url", repo), ("rev", rev)] {
        ensure!(
            !value.is_empty(),
            "fetch_source: {label} is empty"
        );
        ensure!(
            !value.contains(['"', '\'', ';', '$', '`', '\\', '\n', '\r']),
            "fetch_source: refusing {label} containing a Nix-significant character: {value:?}"
        );
    }
    let expr = format!("builtins.fetchGit {{ url = \"{repo}\"; rev = \"{rev}\"; }}");
    eprintln!("fetch_source: nix eval --impure {expr}");

    let mut child = Command::new("nix")
        .args(["eval", "--raw", "--impure", "--expr", &expr])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning nix eval")?;
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .context("reading nix eval stdout")?;
    }
    let status = child.wait().context("waiting for nix eval")?;
    if !status.success() {
        bail!("fetching {repo}@{rev} failed (exit {:?})", status.code());
    }
    let path = stdout.trim();
    ensure!(
        path.starts_with("/nix/store/"),
        "fetching {repo}@{rev} returned something that is not a store path: {path:?}"
    );
    Ok(PathBuf::from(path))
}

/// Resolve a `nix.runtime` installable to a store path.
///
/// `nixpkgs#quickshell` is a binary-cache hit, so declaring a runtime does not
/// mean compiling one.
pub fn resolve_runtime(spec: &str) -> Result<PathBuf> {
    let (flake, attr) = match spec.split_once('#') {
        Some((flake, attr)) => (flake, attr),
        None => (spec, "default"),
    };
    ensure!(!flake.is_empty(), "nix.runtime names no flake: {spec:?}");
    build_store_path(flake, attr)
}

/// Launch argv for a rice previewed from a fetched tree.
///
/// `quickshell -p <dir>` — the tree's own config directory, not a `-c <name>`
/// lookup, because nothing was installed under `$XDG_CONFIG_HOME`.
///
/// `runtime` is the shell's store path when the catalog declares one. That is the
/// point of declaring it: otherwise this depends on the user's system already
/// having the shell, which for a quickshell rice is a guess.
pub fn source_launch_argv(
    entry: &RiceEntry,
    tree: &Path,
    runtime: Option<&Path>,
) -> Result<Vec<String>> {
    let src = entry
        .symlink_src
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("previewing from a tree needs symlink_src"))?;
    let bin = entry
        .launch
        .as_ref()
        .and_then(|l| l.bin.clone())
        .unwrap_or_else(|| "quickshell".to_string());
    let argv0 = match runtime {
        Some(store) => {
            let candidate = store.join("bin").join(&bin);
            ensure!(
                candidate.is_file(),
                "the declared runtime has no bin/{bin}: {}",
                store.display()
            );
            candidate.to_string_lossy().into_owned()
        }
        None => bin,
    };
    Ok(vec![
        argv0,
        "-p".to_string(),
        tree.join(src).to_string_lossy().into_owned(),
    ])
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

/// Which compositor's modules to import, when that can be known.
///
/// `RICE_COOKER_COMPOSITOR` — written by `programs.rice-cooker` — wins, so a
/// declarative choice is honoured. Otherwise detection is used, and a headless
/// install simply emits the compositor-independent modules.
pub fn compositor_hint() -> Option<crate::compositor::CompositorId> {
    let env = crate::compositor::SessionEnv::from_process();
    if let Some(raw) = env.compositor_override.as_deref() {
        return match raw.trim().to_ascii_lowercase().as_str() {
            "niri" => Some(crate::compositor::CompositorId::Niri),
            "hyprland" => Some(crate::compositor::CompositorId::Hyprland),
            _ => None,
        };
    }
    crate::compositor::Compositor::detect(&env)
        .ok()
        .map(|compositor| compositor.id())
}

/// The config a user adopts for a durable install.
///
/// On Nix, "install" is necessarily an act of configuration — the tool cannot
/// mutate a declarative system — so the install artifact is this snippet, and the
/// only durable state rice-cooker keeps is the path it wrote it to.
///
/// The compositor matters here: a rice may need an extra module on niri (dms's
/// `homeModules.niri` extends its own namespace), and that is known at this point
/// without any rebuild.
pub fn install_snippet(
    entry: &RiceEntry,
    name: &str,
    compositor: Option<crate::compositor::CompositorId>,
) -> Result<String> {
    let nix = entry
        .nix
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{name}: no [nix] block, so there is nothing to emit"))?;

    let mut out = String::from(
        "# rice-cooker: adopt this into your Home Manager configuration.\n\
         #\n\
         # `enable` is what forces this shell over whatever your own config already\n\
         # starts, and `shell` selects from the flake's built-in list.\n",
    );

    let modules = nix.modules(compositor);
    if !modules.is_empty() {
        out.push_str(
            "#\n\
             # The rice's module has to be imported by you: a module cannot choose its\n\
             # imports from configuration values, and `programs.rice-cooker` asserts\n\
             # that the namespace below is enabled rather than importing it for you.\n\
             imports = [\n",
        );
        for module in &modules {
            out.push_str(&format!("  inputs.{name}.{module}\n"));
        }
        out.push_str("];\n\n");
    }

    out.push_str(&format!(
        "programs.rice-cooker = {{\n\
         \x20 enable = true;\n\
         \x20 shell = \"{name}\";\n\
         \x20 rices.{name} = inputs.{name};\n\
         }};\n"
    ));

    // A rice whose configuration is files has to have them deployed, or the
    // snippet would record a shell selection without installing the shell.
    if let Some((key, source)) = home_file_stanza(entry, name) {
        out.push_str(
            "\n# This rice is configuration, so it is deployed as files rather than\n\
             # read from a module's options.\n",
        );
        out.push_str(&format!("{key}.source = {source};\n"));
    }

    if let Some(namespace) = nix.hm_namespace.as_deref() {
        // The namespace, plus the conventional `enable`. `hm_config` is merged
        // beneath the same namespace, so the two cannot disagree.
        out.push_str(&format!("\n{namespace}.enable = true;\n"));
        if let Some(settings) = &nix.hm_config {
            for (key, value) in settings {
                out.push_str(&format!(
                    "{namespace}.{key} = {};\n",
                    toml_to_nix(value)
                ));
            }
        }
    }

    Ok(out)
}

/// The `xdg.configFile` / `home.file` stanza for a rice's dotfiles, if it has any.
///
/// Returns `(attribute path, source expression)`. The destination is mapped from
/// the catalog's `symlink_dst`, which is the same path Arch symlinks — so the two
/// platforms cannot disagree about where a rice's configuration lives.
fn home_file_stanza(entry: &RiceEntry, name: &str) -> Option<(String, String)> {
    let (src, dst) = entry.symlink()?;
    let rel = dst.strip_prefix("~/")?;
    let (attribute, rel) = match rel.strip_prefix(".config/") {
        Some(rest) => ("xdg.configFile", rest),
        None => ("home.file", rel),
    };
    let source = if src == "." {
        format!("inputs.{name}")
    } else {
        format!("inputs.{name} + \"/{src}\"")
    };
    // `{:?}` quotes and escapes it, which is what a Nix attribute name needs.
    Some((format!("{attribute}.{rel:?}"), source))
}

/// Render a TOML value as a Nix literal.
///
/// Only through this function does catalog data reach the snippet, and only
/// scalars, arrays and tables are representable — never a string that is
/// interpolated as Nix *syntax*.
fn toml_to_nix(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => format!("{:?}", s),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => format!("{:?}", d.to_string()),
        toml::Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(toml_to_nix).collect();
            format!("[ {} ]", inner.join(" "))
        }
        toml::Value::Table(table) => {
            let inner: Vec<String> = table
                .iter()
                .map(|(k, v)| format!("{k} = {};", toml_to_nix(v)))
                .collect();
            format!("{{ {} }}", inner.join(" "))
        }
    }
}

/// Remove an emitted snippet. Idempotent: a missing file is success, so revert
/// can be retried.
pub fn remove_install_snippet(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Write the install snippet, returning where it landed.
pub fn write_install_snippet(
    paths: &Paths,
    entry: &RiceEntry,
    name: &str,
    compositor: Option<crate::compositor::CompositorId>,
) -> Result<PathBuf> {
    let dir = paths.data_home.join("install-snippets");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{name}.nix"));
    let body = install_snippet(entry, name, compositor)?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

// ── compositor compatibility (the T2 probe) ──────────────────────────────────

/// Bindings found in a rice's configuration tree, counted per *file*.
///
/// Per file rather than per occurrence: one file importing `Quickshell.Hyprland`
/// twelve times is one file that will fail to load on niri, not twelve.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BindingCounts {
    pub hyprland: usize,
    pub niri: usize,
    /// Files examined, so a zero can be told from "nothing was scanned".
    pub files_scanned: usize,
}

/// Substrings that mean a rice's configuration depends on a compositor's API.
const HYPRLAND_SIGNALS: &[&str] = &["Quickshell.Hyprland", "Hyprland.", "hyprctl"];
const NIRI_SIGNALS: &[&str] = &["niri msg", "NIRI_SOCKET", "Quickshell.Niri"];

const SCAN_EXTENSIONS: &[&str] = &[
    "qml", "js", "mjs", "ts", "sh", "fish", "bash", "py", "lua", "toml", "json", "conf", "kdl",
];
const SCAN_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const SCAN_MAX_FILES: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatVerdict {
    /// Bindings for the declared compositor, and none for the other.
    Supported,
    /// No compositor-specific bindings at all. It renders, because layer shells go
    /// through the compositor-agnostic `zwlr_layer_shell_v1`; nothing in it is
    /// compositor-bound either way.
    Neutral,
    /// Bindings for the *other* compositor and none for this one: the declaration
    /// is wrong, or the rice cannot work here.
    Contradicts,
    /// Bindings for both.
    Mixed,
}

/// Walk a rice's configuration tree and count compositor bindings.
///
/// Point this at the rice's own config directory — `<repo>/<symlink_src>` — not at
/// the whole repository. A dotfiles repo commonly carries `config/hypr/...` for
/// Hyprland-side theming, and counting that would report a rice as
/// Hyprland-bound when its quickshell tree has no Hyprland reference at all.
pub fn scan_compositor_bindings(root: &Path) -> Result<BindingCounts> {
    ensure!(root.is_dir(), "not a directory: {}", root.display());
    let mut counts = BindingCounts::default();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A symlink farm is not an error; skip what cannot be read.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if counts.files_scanned >= SCAN_MAX_FILES {
                return Ok(counts);
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let relevant = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| SCAN_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
            if !relevant {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > SCAN_MAX_FILE_BYTES {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&path) else {
                // Not UTF-8, so not configuration we can read as text.
                continue;
            };
            counts.files_scanned += 1;
            if HYPRLAND_SIGNALS.iter().any(|sig| body.contains(sig)) {
                counts.hyprland += 1;
            }
            if NIRI_SIGNALS.iter().any(|sig| body.contains(sig)) {
                counts.niri += 1;
            }
        }
    }
    Ok(counts)
}

/// What the evidence says about a declared compositor.
pub fn compat_verdict(declared: crate::compositor::CompositorId, counts: &BindingCounts) -> CompatVerdict {
    use crate::compositor::CompositorId;
    let (this, other) = match declared {
        CompositorId::Hyprland => (counts.hyprland, counts.niri),
        CompositorId::Niri => (counts.niri, counts.hyprland),
    };
    match (this > 0, other > 0) {
        (true, false) => CompatVerdict::Supported,
        (false, false) => CompatVerdict::Neutral,
        (false, true) => CompatVerdict::Contradicts,
        (true, true) => CompatVerdict::Mixed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{LaunchDecl, PreviewMode, RiceEntry};
    use crate::compositor::CompositorId;

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

    fn nix_entry(build: &str, module: &str, namespace: Option<&str>) -> RiceEntry {
        let mut entry = entry_with_launch(None);
        entry.nix = Some(crate::catalog::NixDecl {
            build: Some(build.to_string()),
            flake: None,
            module: Some(module.to_string()),
            runtime: None,
            module_niri: None,
            hm_namespace: namespace.map(str::to_string),
            hm_config: None,
            packages: vec![],
            follows_nixpkgs: false,
            mutable_config: false,
            preview: None,
            system_module: None,
            system_module_required: false,
        });
        entry
    }

    #[test]
    fn install_snippet_is_adoptable_as_is() {
        let entry = nix_entry("default", "homeModules.default", Some("programs.caelestia"));
        let snippet = install_snippet(&entry, "niri-caelestia", None).unwrap();
        assert!(snippet.contains("programs.rice-cooker"));
        assert!(snippet.contains("enable = true"));
        assert!(snippet.contains("shell = \"niri-caelestia\""));
        // Without these the module's own assertion would fail on the snippet it
        // just told the user to paste.
        assert!(snippet.contains("rices.niri-caelestia = inputs.niri-caelestia"));
        assert!(snippet.contains("inputs.niri-caelestia.homeModules.default"));
        // The namespace plus the conventional enable, not a hardcoded option path.
        assert!(snippet.contains("programs.caelestia.enable = true;"));
    }

    #[test]
    fn install_snippet_selects_the_niri_module_only_on_niri() {
        let mut entry = nix_entry("default", "homeModules.default", Some("programs.dms"));
        entry.nix.as_mut().unwrap().module_niri = Some("homeModules.niri".to_string());
        entry.nix.as_mut().unwrap().hm_config = Some(
            "bar = \"top\"\n"
                .parse::<toml::Table>()
                .unwrap(),
        );

        let on_hypr = install_snippet(&entry, "dms", Some(crate::compositor::CompositorId::Hyprland)).unwrap();
        assert!(!on_hypr.contains("homeModules.niri"), "{on_hypr}");

        let on_niri = install_snippet(&entry, "dms", Some(crate::compositor::CompositorId::Niri)).unwrap();
        assert!(on_niri.contains("inputs.dms.homeModules.niri"), "{on_niri}");
        // hm_config is nested under the namespace, not repeated with it.
        assert!(on_niri.contains("programs.dms.bar = \"top\";"), "{on_niri}");
        assert!(!on_niri.contains("programs.dms.programs"), "{on_niri}");
    }

    #[test]
    fn install_snippet_deploys_dotfiles_for_a_configuration_only_rice() {
        let mut entry = entry_with_launch(None);
        entry.symlink_src = Some("configs/quickshell".into());
        entry.symlink_dst = Some("~/.config/quickshell/retro".into());
        entry.nix = Some(crate::catalog::NixDecl {
            build: None,
            flake: None,
            module: None,
            runtime: Some("nixpkgs#quickshell".into()),
            module_niri: None,
            hm_namespace: None,
            hm_config: None,
            packages: vec![],
            follows_nixpkgs: false,
            mutable_config: false,
            preview: None,
            system_module: None,
            system_module_required: false,
        });
        assert_eq!(entry.preview_mode(), PreviewMode::QuickshellSource);

        let snippet = install_snippet(&entry, "retro", None).unwrap();
        assert!(
            snippet.contains("xdg.configFile.\"quickshell/retro\".source = inputs.retro + \"/configs/quickshell\";"),
            "{snippet}"
        );
    }

    #[test]
    fn a_destination_outside_dot_config_uses_home_file() {
        let mut entry = entry_with_launch(None);
        entry.symlink_src = Some(".".into());
        entry.symlink_dst = Some("~/wallpapers".into());
        let (key, source) = home_file_stanza(&entry, "x").unwrap();
        assert_eq!(key, "home.file.\"wallpapers\"");
        assert_eq!(source, "inputs.x");
    }

    #[test]
    fn install_snippet_without_a_namespace_omits_the_enable_line() {
        let entry = nix_entry("default", "homeModules.default", None);
        let snippet = install_snippet(&entry, "x", None).unwrap();
        assert!(snippet.contains("programs.rice-cooker"));
        assert!(!snippet.contains(".enable = true;\n\nprograms"), "{snippet}");
    }

    #[test]
    fn write_install_snippet_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().to_path_buf();
        let paths = Paths::at_roots(home.clone(), home.join("cache"), home.join("data"));
        let entry = nix_entry("default", "homeModules.default", None);
        let first = write_install_snippet(&paths, &entry, "dms", None).unwrap();
        let second = write_install_snippet(&paths, &entry, "dms", None).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with(home.join("data")));
        assert!(std::fs::read_to_string(&first).unwrap().contains("dms"));
    }

    /// A tree with a rice directory and, separately, Hyprland-side theming.
    fn fixture_tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let rice = t.path().join("quickshell");
        std::fs::create_dir_all(rice.join("bar")).unwrap();
        std::fs::write(rice.join("shell.qml"), "import Quickshell\n").unwrap();
        std::fs::write(
            rice.join("bar/Workspaces.qml"),
            "import Quickshell.Hyprland\nHyprland.workspaces\n",
        )
        .unwrap();
        // Outside the rice: Hyprland-side theming, not the rice.
        let theme = t.path().join("config/hypr/themes/base/scripts");
        std::fs::create_dir_all(&theme).unwrap();
        std::fs::write(theme.join("apply.sh"), "hyprctl keyword general:border 1\n").unwrap();
        t
    }

    #[test]
    fn scan_counts_bindings_per_file_within_the_rice_only() {
        let t = fixture_tree();
        let counts = scan_compositor_bindings(&t.path().join("quickshell")).unwrap();
        assert_eq!(counts.hyprland, 1, "one file imports Quickshell.Hyprland");
        assert_eq!(counts.niri, 0);
        assert_eq!(counts.files_scanned, 2);

        // Scanning the whole repo would see the theme script and misreport it.
        let whole = scan_compositor_bindings(t.path()).unwrap();
        assert_eq!(whole.hyprland, 2);
    }

    #[test]
    fn scan_ignores_binaries_unreadable_and_irrelevant_files() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("logo.png"), [0xff, 0xfe, 0xfd]).unwrap();
        std::fs::write(t.path().join("notes.txt"), "hyprctl everywhere").unwrap();
        std::fs::write(t.path().join("shell.qml"), "import Quickshell").unwrap();
        let counts = scan_compositor_bindings(t.path()).unwrap();
        assert_eq!(counts.hyprland, 0, ".txt is not a config extension");
        assert_eq!(counts.files_scanned, 1);
    }

    #[test]
    fn a_rice_with_no_compositor_bindings_is_neutral() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("shell.qml"), "import Quickshell\n").unwrap();
        let counts = scan_compositor_bindings(t.path()).unwrap();
        // Neutral, not Contradicts: quickshell renders through the
        // compositor-agnostic zwlr_layer_shell_v1, so nothing here is bound.
        assert_eq!(compat_verdict(CompositorId::Niri, &counts), CompatVerdict::Neutral);
        assert_eq!(compat_verdict(CompositorId::Hyprland, &counts), CompatVerdict::Neutral);
    }

    #[test]
    fn verdict_reads_the_bindings_the_right_way_round() {
        let hyprland_only = BindingCounts { hyprland: 3, niri: 0, files_scanned: 3 };
        assert_eq!(
            compat_verdict(CompositorId::Hyprland, &hyprland_only),
            CompatVerdict::Supported
        );
        assert_eq!(
            compat_verdict(CompositorId::Niri, &hyprland_only),
            CompatVerdict::Contradicts
        );

        let niri_only = BindingCounts { hyprland: 0, niri: 2, files_scanned: 2 };
        assert_eq!(
            compat_verdict(CompositorId::Niri, &niri_only),
            CompatVerdict::Supported
        );
        assert_eq!(
            compat_verdict(CompositorId::Hyprland, &niri_only),
            CompatVerdict::Contradicts
        );

        let both = BindingCounts { hyprland: 1, niri: 1, files_scanned: 2 };
        assert_eq!(
            compat_verdict(CompositorId::Niri, &both),
            CompatVerdict::Mixed
        );
    }

    #[test]
    fn scanning_a_missing_directory_is_an_error_not_a_zero() {
        let err = scan_compositor_bindings(Path::new("/nonexistent-rice-tree"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn fetch_source_refuses_nix_significant_characters() {
        for bad in [
            ("https://x/\"y", "rev"),
            ("https://x/y; rm -rf /", "rev"),
            ("https://x/y", "0123\"456"),
            ("https://x/y", "$(whoami)"),
        ] {
            let err = fetch_source(bad.0, bad.1).unwrap_err().to_string();
            assert!(err.contains("refusing"), "{bad:?}: got {err}");
        }
        let err = fetch_source("", "abc").unwrap_err().to_string();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn source_launch_points_quickshell_at_the_tree() {
        let store = store_with_bin(vec![]);
        let mut entry = entry_with_launch(None);
        entry.symlink_src = Some("configs/quickshell".into());
        let argv = source_launch_argv(&entry, store.path(), None).unwrap();
        assert_eq!(argv[0], "quickshell");
        assert_eq!(argv[1], "-p");
        assert_eq!(
            argv[2],
            store
                .path()
                .join("configs/quickshell")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn source_launch_without_a_source_dir_is_an_error() {
        let store = store_with_bin(vec![]);
        let mut entry = entry_with_launch(None);
        entry.symlink_src = None;
        let err = source_launch_argv(&entry, store.path(), None).unwrap_err().to_string();
        assert!(err.contains("symlink_src"), "got: {err}");
    }

    #[test]
    fn the_bundled_dms_entry_emits_its_niri_module_only_on_niri() {
        // A cross-check of the whole chain: catalog data, the compositor, and the
        // snippet the user is told to paste.
        let catalog = crate::catalog::Catalog::parse(include_str!("../catalog.toml")).unwrap();
        let entry = catalog.get("dms").expect("dms is in the bundled catalog");

        let on_niri = install_snippet(entry, "dms", Some(crate::compositor::CompositorId::Niri))
            .unwrap();
        assert!(
            on_niri.contains("inputs.dms.homeModules.niri"),
            "the niri module is required on niri:\n{on_niri}"
        );
        assert!(
            on_niri.contains("programs.dank-material-shell.enable = true;"),
            "the namespace plus the filled enable:\n{on_niri}"
        );

        let on_hyprland =
            install_snippet(entry, "dms", Some(crate::compositor::CompositorId::Hyprland)).unwrap();
        assert!(
            !on_hyprland.contains("homeModules.niri"),
            "the niri module must not be imported on Hyprland:\n{on_hyprland}"
        );
    }

    #[test]
    fn source_launch_uses_the_declared_runtime_when_there_is_one() {
        let store = store_with_bin(vec!["quickshell"]);
        let mut entry = entry_with_launch(None);
        entry.symlink_src = Some(".".into());
        let argv = source_launch_argv(&entry, store.path(), Some(store.path())).unwrap();
        assert_eq!(
            argv[0],
            store.path().join("bin/quickshell").to_string_lossy().into_owned()
        );
    }

    #[test]
    fn a_runtime_without_the_declared_binary_is_an_error() {
        let store = store_with_bin(vec![]);
        let mut entry = entry_with_launch(None);
        entry.symlink_src = Some(".".into());
        let err = source_launch_argv(&entry, store.path(), Some(store.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no bin/quickshell"), "got: {err}");
    }

    #[test]
    fn runtime_spec_splits_into_a_flake_and_an_attribute() {
        // Only the parsing is unit-testable without invoking nix; the wrong-attribute
        // path is caught by the `ensure` in `source_launch_argv`.
        assert_eq!("nixpkgs#quickshell".split_once('#').unwrap(), ("nixpkgs", "quickshell"));
        let err = resolve_runtime("#quickshell").unwrap_err().to_string();
        assert!(err.contains("names no flake"), "got: {err}");
    }

    #[test]
    fn remove_install_snippet_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().to_path_buf();
        let paths = Paths::at_roots(home.clone(), home.join("cache"), home.join("data"));
        let entry = nix_entry("default", "homeModules.default", None);
        let path = write_install_snippet(&paths, &entry, "dms", None).unwrap();
        assert!(path.exists());
        remove_install_snippet(&path).unwrap();
        assert!(!path.exists());
        // Retrying must not fail: revert is allowed to run twice.
        remove_install_snippet(&path).unwrap();
    }

    #[test]
    fn build_store_path_refuses_flag_injection() {
        let err = build_store_path("-evil", "default").unwrap_err().to_string();
        assert!(err.contains("refusing flake reference"), "got: {err}");
        let err = build_store_path("github:x/y", "-rf").unwrap_err().to_string();
        assert!(err.contains("refusing package attribute"), "got: {err}");
    }

}
