//! Rice catalog — single `catalog.toml` file keyed by rice name.
//!
//! Two shapes are accepted, distinguished by whether an entry carries a `[nix]`
//! table:
//!
//! - **Arch entries** (v1): a pinned commit to clone and a symlink into
//!   `$XDG_CONFIG_HOME`, plus `install_deps`/`preview_deps` for paru/yay.
//! - **Nix entries** (v2): a flake reference and one of three shapes —
//!   `module` (flake exports `homeManagerModules.*`), `package` (flake exports
//!   `packages.<system>.*`) or `dotfiles` (flake re-exports a tree).
//!
//! Every v2 field is defaulted, so the v1 entries in the shipped catalog parse
//! unchanged and `compositors` defaults to `["hyprland"]`, preserving v1
//! semantics exactly.
//!
//! Parsing has two modes. [`Catalog::parse`] is strict and is what CI and the
//! bundled catalog use. [`Catalog::parse_lenient`] is for a catalog fetched at
//! runtime: an entry this binary cannot understand is skipped with a warning
//! rather than taking the whole catalog down.

use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail, ensure};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::compositor::CompositorId;

/// Schema this binary writes and understands. A remote catalog declaring a
/// higher number is refused rather than half-parsed.
pub const CATALOG_SCHEMA: u32 = 2;

/// Reserved table holding catalog-wide metadata. Not a rice.
pub const RESERVED_TABLE: &str = "_catalog";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    /// Any malformed or unknown entry fails the whole parse. CI and the bundled
    /// catalog.
    Strict,
    /// A malformed or unknown entry is skipped with a warning. Use for anything
    /// fetched at runtime.
    Lenient,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// From `[_catalog].schema`. Defaults to [`CATALOG_SCHEMA`] when absent.
    #[serde(skip)]
    pub schema: u32,
    #[serde(flatten)]
    pub rices: IndexMap<String, RiceEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiceEntry {
    pub display_name: String,
    pub creator_name: String,
    pub repo: String,
    /// Ref passed to `git checkout --detach`. For `[nix]` entries this must be a
    /// full 40-hex revision or a non-hex ref name; see `validate_nix`.
    pub commit: String,
    /// Source path installed by the symlink step, relative to the clone dir.
    /// Required for Arch entries, and used by Nix as the declaration of which
    /// directory holds a rice's configuration files.
    #[serde(default)]
    pub symlink_src: Option<String>,
    /// Symlink destination, `~`-expanded. Must stay under `$HOME`.
    /// Required for Arch entries, and used by Nix as the declaration of which
    /// directory holds a rice's configuration files.
    #[serde(default)]
    pub symlink_dst: Option<String>,
    /// True when the package installs a Quickshell config discoverable by `-c`.
    #[serde(default)]
    pub package_managed: bool,
    /// Minimal deps needed even for dependency-light preview. Arch only.
    #[serde(default)]
    pub preview_deps: Vec<String>,
    /// Full install deps. Empty means install is unavailable. Arch only.
    #[serde(default)]
    pub install_deps: Vec<String>,
    /// Reserved for future interactive-installer support. Set to true ⇒
    /// install refuses (see `docs/issues/interactive-installs.md`).
    #[serde(default)]
    pub interactive: bool,

    // ── v2 ────────────────────────────────────────────────────────────────────
    /// Compositors this rice works on. Defaults to Hyprland-only, which is
    /// exactly the v1 assumption, so existing entries are unchanged in meaning.
    #[serde(default = "default_compositors")]
    pub compositors: Vec<CompositorId>,
    /// Regexes matched against layer-shell surface namespaces when deciding
    /// whether this rice actually came up. Required to verify on niri, whose
    /// layer list carries no pid. Empty means "use the compositor default".
    #[serde(default)]
    pub layer_namespaces: Vec<String>,
    /// How to start the rice. Absent means `quickshell -c <name>`.
    #[serde(default)]
    pub launch: Option<LaunchDecl>,
    /// Present ⇒ this is a Nix entry.
    #[serde(default)]
    pub nix: Option<NixDecl>,
}

fn default_compositors() -> Vec<CompositorId> {
    vec![CompositorId::Hyprland]
}

/// How the rice's shell is started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchDecl {
    pub kind: LaunchKind,
    /// `kind = "quickshell"`: the binary to run. Defaults to `quickshell`.
    #[serde(default)]
    pub bin: Option<String>,
    /// `kind = "quickshell"`: the `-c` argument. Defaults to the catalog key.
    #[serde(default)]
    pub config: Option<String>,
    /// `kind = "argv"`: the full argv, `argv[0]` included.
    #[serde(default)]
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchKind {
    /// `bin -c config`
    Quickshell,
    /// An explicit argv.
    Argv,
}

/// How a rice can be *previewed* on Nix.
///
/// Deliberately separate from how the rice is *installed* (`build`, `module` and
/// the dotfiles pair).
/// The two are independent: noctalia is a `module` rice that has to be built
/// (`preview = "package"`), while a quickshell dotfiles rice is a `dotfiles`
/// install that previews by fetching a tree. Deriving one from the other is what
/// made "fetch the source and run `quickshell -p`" look like a general answer
/// when it only covers the quickshell family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewMode {
    /// Build the flake and run `<store>/bin/<bin>` — anything with a runnable
    /// artifact, compiled shells included.
    Package,
    /// Fetch the tree and run the launcher against the fetched source. Only valid
    /// for a shell that can run from a config directory, i.e. quickshell.
    QuickshellSource,
    /// No runnable artifact: the rice exists only as configuration, so it can be
    /// installed but never previewed without a rebuild. Declaring this is the
    /// point — it makes the limitation explicit instead of a runtime failure.
    Unsupported,
}

/// Nix-side declaration for a rice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NixDecl {
    /// Attribute under `packages.<system>` to build, e.g. `"default"`.
    ///
    /// Presence means this rice has a **runnable artifact**. Absence is normal and
    /// not a defect: a rice that is pure quickshell configuration has no program
    /// of its own to build.
    #[serde(default)]
    pub build: Option<String>,
    /// Flake reference. Defaults to `<repo>/<commit>`.
    #[serde(default)]
    pub flake: Option<String>,
    /// `homeManagerModules.<this>` attribute path, e.g. `"homeModules.default"`.
    ///
    /// A module is one way to supply both the program and its configuration. It is
    /// independent of `build`: a rice may have either, both, or neither.
    #[serde(default)]
    pub module: Option<String>,
    /// This rice's own enable option, as a dotted path, e.g.
    /// `programs.caelestia.enable`.
    ///
    /// A module cannot choose its imports from configuration values, so the user
    /// has to import the rice's module themselves. This lets
    /// `programs.rice-cooker` *verify* they did, instead of silently forcing a
    /// shell whose module was never imported.
    #[serde(default)]
    pub hm_option: Option<String>,
    /// Extra nixpkgs attribute paths to add to `home.packages`.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Whether this flake's `nixpkgs` input should follow ours. Idiomatic, but
    /// opt-in: following a flake that expects its own nixpkgs can break it.
    #[serde(default)]
    pub follows_nixpkgs: bool,
    /// Rice writes into its own config directory at runtime, so a read-only
    /// store path will not do; activation stages a writable copy.
    #[serde(default)]
    pub mutable_config: bool,
    /// How to preview this rice. Defaults are derived from `shape` where that is
    /// unambiguous, so an entry that can be previewed differently must say so.
    #[serde(default)]
    pub preview: Option<PreviewMode>,
    /// Informational: the flake's NixOS module, if any. Never applied — see
    /// `system_module_required`.
    #[serde(default)]
    pub system_module: Option<String>,
    /// True when the rice cannot function without its NixOS module. Such an
    /// entry is reported as unsupported rather than silently activated
    /// incompletely, because applying it requires a system rebuild.
    #[serde(default)]
    pub system_module_required: bool,
    /// Nix module configuration, as data. Converted to JSON and merged into the
    /// generated module — never interpolated as Nix source text.
    #[serde(default)]
    pub hm_config: Option<toml::Table>,
}

impl NixDecl {
    /// The preview mode, defaulted from `shape` where derivable.
    ///
    /// `module` defaults to `Unsupported` because having a Home Manager module
    /// says nothing about whether there is a binary to run — but a NixOS
    /// activation *does* need one, and `install` does not. So it must be declared.
    pub fn preview_mode(&self, entry: &RiceEntry) -> PreviewMode {
        if let Some(preview) = self.preview {
            return preview;
        }
        // A buildable artifact is the only thing that can be launched from a
        // store path.
        if self.build.is_some() {
            return PreviewMode::Package;
        }
        // Otherwise the rice is configuration, so it can be previewed only if
        // `symlink_src` names a directory a shell can be pointed at.
        if entry.symlink_src.is_some() {
            return PreviewMode::QuickshellSource;
        }
        PreviewMode::Unsupported
    }

    /// Whether any Nix realization is declared at all.
    ///
    /// Deliberately not a check on `build`: a configuration-only rice is still
    /// installable, it just cannot be previewed.
    pub fn is_declared(&self, entry: &RiceEntry) -> bool {
        self.build.is_some() || self.module.is_some() || entry.symlink_src.is_some()
    }

    /// The flake reference to lock, defaulting to `<repo>/<commit>`.
    pub fn flake_ref(&self, repo: &str, commit: &str) -> String {
        match self.flake.as_deref() {
            Some(explicit) if !explicit.is_empty() => explicit.to_string(),
            _ => format!("{}", format_args!("{repo}/{commit}")),
        }
    }

    /// The `packages.<system>` attribute, when the rice has one to build.
    pub fn build_attr(&self) -> Option<&str> {
        self.build.as_deref()
    }
}

impl RiceEntry {
    /// How this entry can be previewed on Nix.
    ///
    /// `Unsupported` for a non-Nix entry, which is honest: it has no Nix
    /// realization to preview.
    pub fn preview_mode(&self) -> PreviewMode {
        self.nix
            .as_ref()
            .map(|nix| nix.preview_mode(self))
            .unwrap_or(PreviewMode::Unsupported)
    }

    /// Whether this entry can be installed at all, per platform.
    pub fn is_nix(&self) -> bool {
        self.nix.is_some()
    }

    /// Whether `install` can do anything meaningful here.
    ///
    /// Arch: there must be dependency work to do (upstream PR #16's rule). Nix:
    /// there must be a `[nix]` block, because on a declarative system `install`
    /// means emitting the configuration to adopt rather than mutating state.
    pub fn install_is_supported(&self, platform: crate::platform::PlatformId) -> bool {
        match platform {
            crate::platform::PlatformId::Arch => !self.install_deps.is_empty(),
            // A `[nix]` block that declares nothing is not installable; asking
            // `is_some` alone would report an empty block as supported.
            crate::platform::PlatformId::Nix => {
                self.nix.as_ref().is_some_and(|nix| nix.is_declared(self))
            }
        }
    }

    pub fn supports(&self, id: CompositorId) -> bool {
        self.compositors.contains(&id)
    }

    /// The symlink pair to install, when this entry has one.
    ///
    /// `None` for a Nix entry that declares no dotfiles, which links nothing into
    /// `$XDG_CONFIG_HOME`. Validation guarantees the two fields are present or
    /// absent together, so this never yields a half pair.
    pub fn symlink(&self) -> Option<(&str, &str)> {
        match (self.symlink_src.as_deref(), self.symlink_dst.as_deref()) {
            (Some(src), Some(dst)) => Some((src, dst)),
            _ => None,
        }
    }

    /// Whether the install step should create a symlink at all. `package_managed`
    /// entries install their own config discovery, so nothing is linked for them.
    pub fn links_into_config(&self) -> bool {
        !self.package_managed && self.symlink().is_some()
    }
}

impl Catalog {
    /// Strict parse. Backwards compatible with v1 catalogues.
    pub fn parse(s: &str) -> Result<Self> {
        Self::parse_mode(s, ParseMode::Strict)
    }

    /// Lenient parse for catalogues fetched at runtime.
    pub fn parse_lenient(s: &str) -> Result<Self> {
        Self::parse_mode(s, ParseMode::Lenient)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        Self::from_file_mode(path, ParseMode::Strict)
    }

    pub fn from_file_mode(path: &Path, mode: ParseMode) -> Result<Self> {
        let body = fs::read_to_string(path)
            .with_context(|| format!("reading catalog {}", path.display()))?;
        Self::parse_mode(&body, mode)
            .with_context(|| format!("parsing catalog {}", path.display()))
    }

    pub fn parse_mode(s: &str, mode: ParseMode) -> Result<Self> {
        // `#[serde(flatten)]` means every top-level table would otherwise be read
        // as a rice, so the metadata table is lifted out before deserialising.
        let mut root: toml::Table = toml::from_str(s).context("parsing catalog.toml")?;
        let schema = extract_schema(&mut root)?;
        ensure!(
            schema <= CATALOG_SCHEMA,
            "catalog schema {schema} is newer than this tool supports ({CATALOG_SCHEMA}); \
             refusing to parse it"
        );

        let mut rices: IndexMap<String, RiceEntry> = IndexMap::new();
        for (name, value) in root {
            validate_name(&name)?;
            let entry: RiceEntry = match value.try_into() {
                Ok(entry) => entry,
                Err(e) => match mode {
                    // `to_string()` on a context-wrapped anyhow error prints only
                    // the context, so the serde cause is folded into the message
                    // rather than attached as a source.
                    ParseMode::Strict => {
                        return Err(anyhow::anyhow!("invalid catalog entry {name:?}: {e}"));
                    }
                    ParseMode::Lenient => {
                        eprintln!(
                            "rice-cooker: warn: skipping catalog entry {name:?}: {e}"
                        );
                        continue;
                    }
                },
            };
            match validate_entry(&name, &entry) {
                Ok(()) => {}
                Err(e) => match mode {
                    ParseMode::Strict => {
                        return Err(anyhow::anyhow!("invalid catalog entry {name:?}: {e:#}"));
                    }
                    ParseMode::Lenient => {
                        eprintln!("rice-cooker: warn: skipping catalog entry {name:?}: {e:#}");
                        continue;
                    }
                },
            }
            rices.insert(name, entry);
        }

        Ok(Catalog { schema, rices })
    }

    pub fn get(&self, name: &str) -> Option<&RiceEntry> {
        self.rices.get(name)
    }

    /// Entries this tool can actually act on. `unsupported_reason` is reported
    /// per entry rather than removing it, so the UI can explain the gap.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &RiceEntry)> {
        self.rices.iter()
    }
}

fn extract_schema(root: &mut toml::Table) -> Result<u32> {
    let Some(meta) = root.remove(RESERVED_TABLE) else {
        return Ok(CATALOG_SCHEMA);
    };
    let table = meta
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("[{RESERVED_TABLE}] must be a table"))?;
    for key in table.keys() {
        ensure!(
            key == "schema",
            "[{RESERVED_TABLE}] has unknown key {key:?}; only `schema` is defined"
        );
    }
    match table.get("schema") {
        None => Ok(CATALOG_SCHEMA),
        Some(value) => {
            let raw = value
                .as_integer()
                .ok_or_else(|| anyhow::anyhow!("[{RESERVED_TABLE}].schema must be an integer"))?;
            let raw = u32::try_from(raw)
                .map_err(|_| anyhow::anyhow!("[{RESERVED_TABLE}].schema is out of range: {raw}"))?;
            Ok(raw)
        }
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        // Leading `_` is reserved for metadata tables such as `_catalog`.
        || name.starts_with('_')
        || name.starts_with('-')
        || name.chars().any(|c| matches!(c, '/' | '\\' | '\0'));
    ensure!(!bad, "invalid rice name {name:?}");
    Ok(())
}

fn validate_entry(name: &str, entry: &RiceEntry) -> Result<()> {
    ensure!(!entry.display_name.is_empty(), "{name}: display_name is empty");
    ensure!(!entry.creator_name.is_empty(), "{name}: creator_name is empty");
    ensure!(!entry.repo.is_empty(), "{name}: repo is empty");
    ensure!(!entry.commit.is_empty(), "{name}: commit is empty");
    ensure!(
        !entry.interactive,
        "{name}: interactive = true is not supported in v1 (see docs/issues/interactive-installs.md)"
    );
    ensure!(
        !entry.compositors.is_empty(),
        "{name}: compositors must not be empty; omit the key to default to [\"hyprland\"]"
    );

    validate_launch(name, entry)?;
    validate_nix(name, entry)?;

    // The symlink fields are Arch-side data and the `[nix]` shape is Nix-side
    // data: they are orthogonal, and a rice may need both. noctalia is exactly
    // that — symlinked into `quickshell/` on Arch, imported as a module on Nix.
    // Only `dotfiles` *requires* them, because that shape realizes by linking.
    // Arch realizes by symlink, so it needs the pair. A Nix entry may use the
    // symlink pair as its dotfiles declaration, or a build, or a module, in any
    // combination — `NixDecl::is_declared` is what requires it to say *something*.
    let needs_symlink = entry.nix.is_none();
    match (&entry.symlink_src, &entry.symlink_dst) {
        (None, None) if !needs_symlink => {}
        (None, _) | (_, None) => bail!(
            "{name}: symlink_src and symlink_dst must both be set or both omitted"
        ),
        (Some(src), Some(dst)) => {
            validate_symlink_src(name, src)?;
            validate_symlink_dst(name, dst)?;
        }
    }

    // Previewing from a fetched tree needs a directory inside it to point at.
    // Checked here rather than with the other `[nix]` rules because it depends on
    // the symlink pair above having been settled.
    if let Some(nix) = &entry.nix
        && nix.preview_mode(entry) == PreviewMode::QuickshellSource
        && entry.symlink_src.is_none()
    {
        bail!(
            "{name}: preview = \"quickshell-source\" requires symlink_src, which names the \
             config directory inside the fetched tree"
        );
    }
    if let Some(nix) = &entry.nix {
        ensure!(
            nix.is_declared(entry),
            "{name}: the [nix] block declares nothing to install — give it build, module \
             or symlink_src"
        );
    }

    Ok(())
}

fn validate_symlink_src(name: &str, raw: &str) -> Result<()> {
    ensure!(!raw.is_empty(), "{name}: symlink_src is required");
    // symlink_src gets Path::join'd onto clone_dir; absolute paths would
    // escape clone_dir outright and `..` would escape at dereference time.
    let src = Path::new(raw);
    ensure!(
        !src.is_absolute(),
        "{name}: symlink_src must be relative to the clone dir, got {raw:?}"
    );
    ensure!(
        !src.components().any(|c| matches!(c, Component::ParentDir)),
        "{name}: symlink_src must not contain .. segments, got {raw:?}"
    );
    Ok(())
}

fn validate_symlink_dst(name: &str, dst: &str) -> Result<()> {
    ensure!(!dst.is_empty(), "{name}: symlink_dst is required");
    ensure!(
        dst.starts_with("~/"),
        "{name}: symlink_dst must be under $HOME (start with `~/`), got {dst:?}"
    );
    ensure!(
        dst != "~/",
        "{name}: symlink_dst cannot be $HOME itself: {dst:?}"
    );
    ensure!(
        !Path::new(dst)
            .components()
            .any(|c| matches!(c, Component::ParentDir)),
        "{name}: symlink_dst must not contain .. components: {dst:?}"
    );
    Ok(())
}

fn validate_launch(name: &str, entry: &RiceEntry) -> Result<()> {
    let Some(launch) = &entry.launch else {
        return Ok(());
    };
    if launch.kind == LaunchKind::Argv {
        ensure!(
            !launch.argv.is_empty(),
            "{name}: launch.kind = \"argv\" requires a non-empty launch.argv"
        );
        ensure!(
            !launch.argv.iter().any(|a| a.is_empty()),
            "{name}: launch.argv must not contain empty arguments"
        );
    }
    Ok(())
}

fn validate_nix(name: &str, entry: &RiceEntry) -> Result<()> {
    let Some(nix) = &entry.nix else {
        return Ok(());
    };

    // A Nix flake reference with a short SHA resolves non-deterministically as
    // the log grows, so only a full revision or a named ref is accepted.
    if is_hex(&entry.commit) {
        ensure!(
            entry.commit.len() == 40,
            "{name}: nix entries need a full 40-character revision, got a {}-character \
             abbreviated revision {:?}",
            entry.commit.len(),
            entry.commit
        );
    }

    let flake = nix.flake_ref(&entry.repo, &entry.commit);
    ensure!(
        !flake.starts_with('-'),
        "{name}: flake reference must not start with '-': {flake:?}"
    );
    ensure!(
        !flake.contains(['"', '\'', ';', '$', '`', '\n', '\r']),
        "{name}: flake reference contains a character that cannot appear in a flake URL: {flake:?}"
    );

    if let Some(build) = nix.build.as_deref() {
        ensure_attr_path(name, "nix.build", build)?;
    }
    if let Some(module) = nix.module.as_deref() {
        ensure_attr_path(name, "nix.module", module)?;
    }

    for package in &nix.packages {
        ensure_attr_path(name, "nix.packages", package)?;
    }

    if let Some(system_module) = nix.system_module.as_deref() {
        ensure_attr_path(name, "nix.system_module", system_module)?;
    }

    if let Some(hm_option) = nix.hm_option.as_deref() {
        ensure_attr_path(name, "nix.hm_option", hm_option)?;
    }

    ensure!(
        !(nix.system_module_required && nix.system_module.is_none()),
        "{name}: system_module_required = true requires nix.system_module to name the module"
    );

    if let Some(hm_config) = &nix.hm_config {
        for key in hm_config.keys() {
            ensure_attr_path(name, "nix.hm_config", key)?;
        }
    }

    Ok(())
}

/// Attribute paths are dotted Nix identifiers. This is the only place catalog
/// data reaches the generated Nix module as a path, so it is validated strictly
/// rather than escaped.
fn ensure_attr_path(name: &str, field: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{name}: {field} is empty");
    let ok = value.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    });
    ensure!(
        ok,
        "{name}: {field} must be a dotted attribute path of [A-Za-z0-9_-] segments, got {value:?}"
    );
    Ok(())
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [dms]
        display_name = "DMS"
        creator_name = "AvengeMedia"
        repo = "https://x/dms"
        commit = "0123456789abcdef0123456789abcdef01234567"
        symlink_src = "."
        symlink_dst = "~/.config/quickshell/dms"
    "#;

    const NIX_NOCTALIA: &str = r#"
        [noctalia]
        display_name = "Noctalia"
        creator_name = "noctalia-dev"
        repo = "https://github.com/noctalia-dev/noctalia-shell"
        commit = "d7b68652e79bce5813dc4fea7e51636a5da3e1b7"
        compositors = ["hyprland", "niri"]
        layer_namespaces = ["^noctalia-"]

        [noctalia.launch]
        kind = "argv"
        argv = ["noctalia"]

        [noctalia.nix]
        build = "default"
        module = "homeModules.default"
        system_module = "nixosModules.default"
        packages = ["cliphist", "wl-clipboard"]
        follows_nixpkgs = true

        [noctalia.nix.hm_config.programs.noctalia]
        enable = true
    "#;

    #[test]
    fn parses_minimal() {
        let c = Catalog::parse(MINIMAL).unwrap();
        let e = c.get("dms").unwrap();
        assert_eq!(e.display_name, "DMS");
        assert_eq!(e.creator_name, "AvengeMedia");
        assert!(e.preview_deps.is_empty());
        assert!(e.install_deps.is_empty());
        assert!(!e.package_managed);
        assert!(!e.interactive);
    }

    #[test]
    fn v1_entries_default_to_hyprland_only() {
        // Preserves v1 meaning exactly: nothing starts claiming niri support.
        let c = Catalog::parse(MINIMAL).unwrap();
        let e = c.get("dms").unwrap();
        assert_eq!(e.compositors, vec![CompositorId::Hyprland]);
        assert!(e.supports(CompositorId::Hyprland));
        assert!(!e.supports(CompositorId::Niri));
        assert!(e.layer_namespaces.is_empty());
        assert!(e.launch.is_none());
        assert!(e.nix.is_none());
        assert!(!e.is_nix());
    }

    #[test]
    fn absent_metadata_table_means_current_schema() {
        let c = Catalog::parse(MINIMAL).unwrap();
        assert_eq!(c.schema, CATALOG_SCHEMA);
    }

    #[test]
    fn metadata_table_is_not_parsed_as_a_rice() {
        let body = format!("[_catalog]\nschema = 2\n{MINIMAL}");
        let c = Catalog::parse(&body).unwrap();
        assert_eq!(c.schema, 2);
        assert_eq!(c.rices.len(), 1);
        assert!(c.get(RESERVED_TABLE).is_none());
    }

    #[test]
    fn newer_schema_is_refused() {
        let body = format!("[_catalog]\nschema = 3\n{MINIMAL}");
        let err = Catalog::parse(&body).unwrap_err().to_string();
        assert!(err.contains("newer than this tool supports"), "got: {err}");
        assert!(Catalog::parse_lenient(&body).is_err(), "refused in lenient mode too");
    }

    #[test]
    fn unknown_metadata_key_is_rejected() {
        let body = format!("[_catalog]\nschema = 2\noops = 1\n{MINIMAL}");
        let err = Catalog::parse(&body).unwrap_err().to_string();
        assert!(err.contains("unknown key"), "got: {err}");
    }

    #[test]
    fn reserved_name_prefix_is_rejected() {
        for bad in ["_catalog", "_anything"] {
            assert!(validate_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rejects_interactive_true() {
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            symlink_src = "."
            symlink_dst = "~/.config/quickshell/x"
            interactive = true
        "#;
        let err = Catalog::parse(t).unwrap_err().to_string();
        assert!(err.contains("interactive"), "got: {err}");
    }

    #[test]
    fn rejects_symlink_dst_outside_home() {
        for bad in ["/etc/x", "/usr/share/x", "/", "~", "~/../escape"] {
            let t = format!(
                r#"
                [x]
                display_name = "X"
                creator_name = "x"
                repo = "https://x"
                commit = "0123456789abcdef0123456789abcdef01234567"
                symlink_src = "."
                symlink_dst = "{bad}"
                "#
            );
            assert!(Catalog::parse(&t).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn refuses_missing_required_fields() {
        for body in [
            r#"[x]
               display_name = ""
               creator_name = "x"
               repo = "https://x"
               commit = "0123456789abcdef0123456789abcdef01234567"
               symlink_src = "."
               symlink_dst = "~/.config/x""#,
            r#"[x]
               display_name = "X"
               creator_name = ""
               repo = "https://x"
               commit = "0123456789abcdef0123456789abcdef01234567"
               symlink_src = "."
               symlink_dst = "~/.config/x""#,
            r#"[x]
               display_name = "X"
               creator_name = "x"
               repo = ""
               commit = "0123456789abcdef0123456789abcdef01234567"
               symlink_src = "."
               symlink_dst = "~/.config/x""#,
            r#"[x]
               display_name = "X"
               creator_name = "x"
               repo = "https://x"
               commit = "0123456789abcdef0123456789abcdef01234567"
               symlink_src = ""
               symlink_dst = "~/.config/x""#,
            r#"[x]
               display_name = "X"
               creator_name = "x"
               repo = "https://x"
               commit = "0123456789abcdef0123456789abcdef01234567"
               symlink_src = "."
               symlink_dst = """#,
        ] {
            assert!(Catalog::parse(body).is_err(), "accepted: {body}");
        }
    }

    #[test]
    fn round_trips_with_deps() {
        let t = r#"
            [caelestia]
            display_name = "Caelestia"
            creator_name = "soramenew"
            repo = "https://github.com/caelestia-dots/shell"
            commit = "efc08759ceaeddc2c571d868c623995270ac365d"
            symlink_src = "."
            symlink_dst = "~/.config/quickshell/caelestia"
            package_managed = true
            install_deps = ["quickshell-git", "caelestia-shell"]
            preview_deps = ["quickshell-git", "caelestia-shell"]
        "#;
        let c = Catalog::parse(t).unwrap();
        let e = c.get("caelestia").unwrap();
        assert!(e.package_managed);
        assert_eq!(e.install_deps, vec!["quickshell-git", "caelestia-shell"]);
        assert_eq!(e.preview_deps, vec!["quickshell-git", "caelestia-shell"]);
    }

    // ── v2 ────────────────────────────────────────────────────────────────────

    #[test]
    fn parses_a_module_rice_with_launch_and_nix() {
        let c = Catalog::parse(NIX_NOCTALIA).unwrap();
        let e = c.get("noctalia").unwrap();
        assert_eq!(e.compositors, vec![CompositorId::Hyprland, CompositorId::Niri]);
        assert_eq!(e.layer_namespaces, vec!["^noctalia-"]);
        assert!(e.is_nix());

        let launch = e.launch.as_ref().unwrap();
        assert_eq!(launch.kind, LaunchKind::Argv);
        assert_eq!(launch.argv, vec!["noctalia"]);

        let nix = e.nix.as_ref().unwrap();
        assert_eq!(nix.module.as_deref(), Some("homeModules.default"));
        assert_eq!(nix.system_module.as_deref(), Some("nixosModules.default"));
        assert!(!nix.system_module_required);
        assert!(nix.follows_nixpkgs);
        assert!(!nix.mutable_config);
        assert_eq!(nix.packages, vec!["cliphist", "wl-clipboard"]);
        assert_eq!(nix.build_attr(), Some("default"));
        let hm = nix.hm_config.as_ref().unwrap();
        assert!(hm.contains_key("programs"));

        // A module rice places its own config; the Arch-era symlink fields are
        // not applicable and are rejected (see `module_rice_may_not_declare_symlinks`).
        assert!(e.symlink_src.is_none());
        assert!(!e.links_into_config());
    }

    #[test]
    fn flake_ref_defaults_to_repo_slash_commit() {
        let c = Catalog::parse(NIX_NOCTALIA).unwrap();
        let e = c.get("noctalia").unwrap();
        let nix = e.nix.as_ref().unwrap();
        assert_eq!(
            nix.flake_ref(&e.repo, &e.commit),
            format!("{}/{}", e.repo, e.commit)
        );
    }

    #[test]
    fn explicit_flake_ref_wins() {
        let t = r#"
            [amane]
            display_name = "Amane"
            creator_name = "MystiaFin"
            repo = "https://github.com/MystiaFin/amane"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [amane.launch]
            kind = "argv"
            argv = ["amane"]
            [amane.nix]
            build = "default"
            flake = "github:MystiaFin/amane"
        "#;
        let c = Catalog::parse(t).unwrap();
        let e = c.get("amane").unwrap();
        let nix = e.nix.as_ref().unwrap();
        assert_eq!(nix.flake_ref(&e.repo, &e.commit), "github:MystiaFin/amane");
        assert_eq!(nix.build_attr(), Some("default"));
        assert_eq!(nix.build_attr(), Some("default"));
        assert!(e.symlink_src.is_none(), "package rice links nothing");
    }

    #[test]
    fn a_nix_block_must_declare_something() {
        // Previously this was phrased as "dotfiles needs symlink_src". Now the
        // rule is the one that actually matters: a `[nix]` block that declares
        // neither a build, a module nor dotfiles installs nothing.
        for empty in ["", "preview = \"package\""] {
            let t = format!(
                r#"
                [dots]
                display_name = "Dots"
                creator_name = "x"
                repo = "https://x"
                commit = "0123456789abcdef0123456789abcdef01234567"
                [dots.nix]
                {empty}
                "#
            );
            let err = Catalog::parse(&t).unwrap_err().to_string();
            assert!(err.contains("declares nothing"), "got: {err}");
        }

        // Any one of the three is enough.
        for one in [
            "build = \"default\"",
            "module = \"homeModules.default\"",
        ] {
            let t = format!(
                r#"
                [dots]
                display_name = "Dots"
                creator_name = "x"
                repo = "https://x"
                commit = "0123456789abcdef0123456789abcdef01234567"
                [dots.nix]
                {one}
                "#
            );
            assert!(Catalog::parse(&t).is_ok(), "rejected {one}");
        }
        // Dotfiles alone count too.
        let t = r#"
            [dots]
            display_name = "Dots"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            symlink_src = "."
            symlink_dst = "~/.config/quickshell/dots"
            [dots.nix]
        "#;
        assert!(Catalog::parse(t).is_ok());
    }

    #[test]
    fn the_dotfiles_pair_is_orthogonal_to_build_and_module() {
        // A rice can be symlinked into place on Arch *and* built *and* imported as
        // a module. noctalia is all three at once, and refusing any combination
        // would exclude it.
        for (label, extra) in [
            ("build", "build = \"default\"\n"),
            ("module", "module = \"homeModules.default\"\n"),
            (
                "both",
                "build = \"default\"\nmodule = \"homeModules.default\"\n",
            ),
        ] {
            let t = format!(
                r#"
                [p]
                display_name = "P"
                creator_name = "x"
                repo = "https://x"
                commit = "0123456789abcdef0123456789abcdef01234567"
                symlink_src = "."
                symlink_dst = "~/.config/x"
                [p.nix]
                {extra}
                "#
            );
            let c = Catalog::parse(&t).unwrap_or_else(|e| panic!("{label}: {e:#}"));
            let entry = c.get("p").unwrap();
            assert!(entry.links_into_config(), "{label}: the symlink must survive");
            assert_eq!(entry.symlink(), Some((".", "~/.config/x")));
            // A build outranks dotfiles for previewing, and that precedence is
            // the one that matters: a compiled shell cannot run from a tree.
            let expected = if label == "module" {
                PreviewMode::QuickshellSource
            } else {
                PreviewMode::Package
            };
            assert_eq!(entry.preview_mode(), expected, "{label}");
        }
    }

    #[test]
    fn a_half_declared_symlink_pair_is_still_refused() {
        let t = r#"
            [p]
            display_name = "P"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            symlink_src = "."
            [p.nix]
            module = "homeModules.default"
        "#;
        let err = Catalog::parse(t).unwrap_err().to_string();
        assert!(err.contains("both be set or both omitted"), "got: {err}");
    }

    #[test]
    fn short_sha_is_rejected_for_nix_entries() {
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "d7b68652e79b"
            [x.nix]
            build = "default"
        "#;
        let err = Catalog::parse(t).unwrap_err().to_string();
        assert!(err.contains("full 40-character revision"), "got: {err}");
    }

    #[test]
    fn branch_ref_is_allowed_for_nix_entries() {
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "stable"
            [x.nix]
            build = "default"
        "#;
        assert!(Catalog::parse(t).is_ok());
    }

    #[test]
    fn a_module_without_a_build_installs_but_cannot_be_previewed() {
        // Having a Home Manager module says nothing about there being a binary to
        // run, which is exactly why `preview` is its own declaration.
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [x.nix]
            module = "homeModules.default"
        "#;
        let c = Catalog::parse(t).unwrap();
        let e = c.get("x").unwrap();
        assert_eq!(e.preview_mode(), PreviewMode::Unsupported);
        assert!(e.install_is_supported(crate::platform::PlatformId::Nix));
    }

    #[test]
    fn preview_mode_is_derived_from_what_is_declared() {
        let base = |extra: &str| {
            format!(
                r#"
                [x]
                display_name = "X"
                creator_name = "x"
                repo = "https://x"
                commit = "0123456789abcdef0123456789abcdef01234567"
                symlink_src = "."
                symlink_dst = "~/.config/quickshell/x"
                [x.nix]
                {extra}
                "#
            )
        };
        let mode = |extra: &str| {
            Catalog::parse(&base(extra))
                .unwrap()
                .get("x")
                .unwrap()
                .preview_mode()
        };
        // A build outranks dotfiles: a compiled shell cannot run from a tree.
        assert_eq!(mode("build = \"default\""), PreviewMode::Package);
        // Dotfiles alone means configuration for a shell that already exists.
        assert_eq!(mode(""), PreviewMode::QuickshellSource);
        // A module plus dotfiles previews from the fetched tree...
        assert_eq!(
            mode("module = \"homeModules.default\""),
            PreviewMode::QuickshellSource
        );
        // ...but a module with neither a build nor dotfiles has nothing to run.
        let bare = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [x.nix]
            module = "homeModules.default"
        "#;
        assert_eq!(
            Catalog::parse(bare).unwrap().get("x").unwrap().preview_mode(),
            PreviewMode::Unsupported
        );
    }

    #[test]
    fn a_module_rice_can_declare_a_package_preview() {
        // noctalia: a `module` install whose shell is C++ and must be built.
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [x.nix]
            module = "homeModules.default"
            preview = "package"
        "#;
        let c = Catalog::parse(t).unwrap();
        assert_eq!(
            c.get("x").unwrap().preview_mode(),
            PreviewMode::Package
        );
    }

    #[test]
    fn quickshell_source_requires_a_config_directory() {
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [x.nix]
            module = "homeModules.default"
            preview = "quickshell-source"
        "#;
        let err = Catalog::parse(t).unwrap_err().to_string();
        assert!(err.contains("requires symlink_src"), "got: {err}");
    }

    #[test]
    fn an_unsupported_preview_is_declarable() {
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [x.nix]
            module = "homeModules.default"
            preview = "unsupported"
        "#;
        let c = Catalog::parse(t).unwrap();
        assert_eq!(
            c.get("x").unwrap().preview_mode(),
            PreviewMode::Unsupported
        );
    }

    #[test]
    fn hm_option_is_parsed_and_validated() {
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [x.nix]
            build = "default"
            hm_option = "programs.caelestia.enable"
        "#;
        let c = Catalog::parse(t).unwrap();
        assert_eq!(
            c.get("x").unwrap().nix.as_ref().unwrap().hm_option.as_deref(),
            Some("programs.caelestia.enable")
        );

        // An injections-shaped path must be refused like any other attr path.
        let bad = t.replace("programs.caelestia.enable", r"programs.${evil}");
        assert!(Catalog::parse(&bad).is_err());
    }

    #[test]
    fn injection_attempts_in_attr_paths_are_rejected() {
        for field in [
            "module = \"homeModules.default\\nrm -rf /\")",
            "module = \"homeModules.${evil}\"",
            "module = \"homeModules.\"",
            "module = \"\"",
            "module = \"home Modules.default\"",
            "module = \"homeModules.default;builtins.abort\"",
        ] {
            let t = format!(
                r#"
                [x]
                display_name = "X"
                creator_name = "x"
                repo = "https://x"
                commit = "0123456789abcdef0123456789abcdef01234567"
                [x.nix]
                module = "homeModules.default"
                {field}
                "#
            );
            assert!(Catalog::parse(&t).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn injection_attempts_in_flake_refs_are_rejected() {
        for flake in [
            "github:x/y\"; evil = \"1",
            "github:x/y;rm -rf /",
            "$(whoami)",
            "-evil",
        ] {
            let t = format!(
                r#"
                [x]
                display_name = "X"
                creator_name = "x"
                repo = "https://x"
                commit = "0123456789abcdef0123456789abcdef01234567"
                [x.nix]
                build = "default"
                flake = "{flake}"
                "#
            );
            assert!(Catalog::parse(&t).is_err(), "accepted {flake:?}");
        }
    }

    #[test]
    fn system_module_required_needs_a_module() {
        let t = r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            [x.nix]
            build = "default"
            system_module_required = true
        "#;
        let err = Catalog::parse(t).unwrap_err().to_string();
        assert!(err.contains("system_module_required"), "got: {err}");
    }

    #[test]
    fn empty_compositor_list_is_rejected() {
        let t = format!("[x]\ncompositors = []\n{MINIMAL_ENTRY}");
        assert!(Catalog::parse(&t).is_err());
    }

    #[test]
    fn unknown_compositor_name_is_rejected() {
        let t = format!("[x]\ncompositors = [\"sway\"]\n{MINIMAL_ENTRY}");
        assert!(Catalog::parse(&t).is_err());
    }

    #[test]
    fn argv_launch_requires_a_non_empty_argv() {
        // The entry keys must precede the `[x.launch]` table header, or they
        // would be read as keys of `launch` instead of of the entry.
        let t = format!("[x]\n{MINIMAL_ENTRY}\n[x.launch]\nkind = \"argv\"\n");
        let err = Catalog::parse(&t).unwrap_err().to_string();
        assert!(err.contains("non-empty launch.argv"), "got: {err}");
    }

    #[test]
    fn unknown_field_is_fatal_in_strict_and_skipped_in_lenient() {
        let body = r#"
            [good]
            display_name = "Good"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            symlink_src = "."
            symlink_dst = "~/.config/x"

            [typo]
            display_name = "Typo"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            symlink_src = "."
            symlink_dst = "~/.config/y"
            fulture_field = true
        "#;
        assert!(
            Catalog::parse(body).is_err(),
            "strict must reject a typo so CI catches it"
        );

        let lenient = Catalog::parse_lenient(body).unwrap();
        assert_eq!(lenient.rices.len(), 1);
        assert!(lenient.get("good").is_some());
        assert!(lenient.get("typo").is_none(), "bad entry skipped, not nulled");
    }

    #[test]
    fn lenient_mode_does_not_paper_over_a_syntactically_broken_catalog() {
        assert!(Catalog::parse_lenient("[x").is_err());
    }

    #[test]
    fn schema_is_preserved_through_a_lenient_parse() {
        let body = format!("[_catalog]\nschema = 2\n{NIX_NOCTALIA}");
        let c = Catalog::parse_lenient(&body).unwrap();
        assert_eq!(c.schema, 2);
    }

    const MINIMAL_ENTRY: &str = r#"
        display_name = "X"
        creator_name = "x"
        repo = "https://x"
        commit = "0123456789abcdef0123456789abcdef01234567"
        symlink_src = "."
        symlink_dst = "~/.config/x"
    "#;

    #[test]
    fn a_nix_rice_that_cannot_be_previewed_must_say_so() {
        // Deriving `Unsupported` silently is how "most rices don't work" turns
        // into a runtime mystery. Every Nix entry either declares something that
        // can be previewed, or states the limitation with `preview`.
        let c = Catalog::parse(include_str!("../catalog.toml")).unwrap();
        for (name, entry) in c.entries() {
            if !entry.is_nix() {
                continue;
            }
            assert!(
                entry.install_is_supported(crate::platform::PlatformId::Nix),
                "{name}: a [nix] block must declare build, module or dotfiles"
            );
            let declared = entry
                .nix
                .as_ref()
                .and_then(|nix| nix.preview)
                .is_some();
            if !declared && entry.preview_mode() == PreviewMode::Unsupported {
                panic!(
                    "{name}: preview would be Unsupported; declare `preview = \"unsupported\"` \
                     so the limitation is stated rather than inferred"
                );
            }
        }
    }

    #[test]
    fn the_bundled_catalog_parses_strictly() {
        // The shipped catalog is what CI gates on; a regression here is a
        // release blocker, not a runtime warning.
        let bundled = include_str!("../catalog.toml");
        let c = Catalog::parse(bundled).expect("bundled catalog.toml must parse strictly");
        assert!(!c.rices.is_empty());
    }
}
