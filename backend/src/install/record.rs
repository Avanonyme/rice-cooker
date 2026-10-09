//! Install record: the JSON file at
//! `~/.local/share/rice-cooker/installs/<name>.json` + the `current.json`
//! pointer to the active rice.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::paths::Paths;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRecord {
    pub schema_version: u32,
    pub name: String,
    pub commit: String,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<PathBuf>,
    /// Present when the rice was realized from a Nix flake.
    ///
    /// Mutually exclusive with `pacman_diff` and the symlink pair: on Nix there
    /// are no packages to diff and nothing to link, so undo is simply "stop the
    /// shell and replay what was running before".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nix: Option<NixInstall>,
    pub pacman_diff: PacmanDiff,
}

/// What a Nix-realized rice leaves behind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NixInstall {
    /// The built store path the shell was launched from.
    ///
    /// Absent for an *emitted* install: `install` on Nix produces configuration
    /// and deliberately builds nothing, so there is no store path yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_path: Option<PathBuf>,
    /// The exact argv used, so a relaunch or a revert does not have to re-derive
    /// it from a catalog that may have changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch_argv: Vec<String>,
    /// Where the adopt-this-config snippet was written, set by `install` on Nix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PacmanDiff {
    #[serde(default)]
    pub added_explicit: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingDeps {
    pub name: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<PathBuf>,
    pub pre_all: Vec<String>,
    pub pre_explicit: Vec<String>,
}

impl InstallRecord {
    pub fn now_rfc3339() -> String {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .expect("RFC3339 formatting of OffsetDateTime::now_utc cannot fail")
    }
}

pub fn save_record(path: &Path, r: &InstallRecord) -> Result<()> {
    let body = serde_json::to_string_pretty(r).context("serializing install record")?;
    crate::paths::write_atomic(path, body.as_bytes(), true)
}

pub fn save_pending_deps(paths: &Paths, pending: &PendingDeps) -> Result<()> {
    let body = serde_json::to_string_pretty(pending).context("serializing pending deps")?;
    crate::paths::write_atomic(&paths.pending_deps_json(), body.as_bytes(), true)
}

pub fn load_pending_deps(paths: &Paths) -> Result<Option<PendingDeps>> {
    let path = paths.pending_deps_json();
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&body)
        .with_context(|| format!("parsing pending deps at {}", path.display()))
        .map(Some)
}

pub fn clear_pending_deps(paths: &Paths) -> Result<()> {
    match fs::remove_file(paths.pending_deps_json()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("removing pending-deps.json"),
    }
}

pub fn load_record(path: &Path) -> Result<InstallRecord> {
    let body = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let r: InstallRecord = serde_json::from_str(&body)
        .with_context(|| format!("parsing install record at {}", path.display()))?;
    if r.schema_version != SCHEMA_VERSION {
        return Err(anyhow::anyhow!(
            "install record at {} is schema_version {}, tool supports {}",
            path.display(),
            r.schema_version,
            SCHEMA_VERSION
        ));
    }
    Ok(r)
}

pub fn write_current(paths: &Paths, name: &str) -> Result<()> {
    let body = serde_json::json!({ "name": name }).to_string();
    crate::paths::write_atomic(&paths.current_json(), body.as_bytes(), true)
}

pub fn read_current(paths: &Paths) -> Result<Option<String>> {
    let s = match fs::read_to_string(paths.current_json()) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading current.json"),
    };
    #[derive(Deserialize)]
    struct Cur {
        name: String,
    }
    serde_json::from_str::<Cur>(&s)
        .map(|c| Some(c.name))
        .context("parsing current.json")
}

pub fn clear_current(paths: &Paths) -> Result<()> {
    match fs::remove_file(paths.current_json()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("removing current.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn tmp_paths() -> (tempfile::TempDir, Paths) {
        let t = tempdir().unwrap();
        let home = t.path().to_path_buf();
        let cache = t.path().join("cache");
        let data = t.path().join("data");
        let p = Paths::at_roots(home, cache, data);
        p.ensure_rices().unwrap();
        p.ensure_installs().unwrap();
        (t, p)
    }

    fn sample() -> InstallRecord {
        InstallRecord {
            schema_version: SCHEMA_VERSION,
            name: "dms".into(),
            commit: "abc123".into(),
            installed_at: InstallRecord::now_rfc3339(),
            symlink_path: Some(PathBuf::from("/home/x/.config/quickshell/dms")),
            symlink_target: Some(PathBuf::from("/home/x/.cache/rice-cooker/rices/dms")),
            nix: None,
            pacman_diff: PacmanDiff {
                added_explicit: vec!["caelestia-shell-git".into()],
                removed: Vec::new(),
            },
        }
    }

    #[test]
    fn record_round_trips_through_json() {
        let (_t, p) = tmp_paths();
        let r = sample();
        let path = p.record_json(&r.name).unwrap();
        save_record(&path, &r).unwrap();
        let back = load_record(&path).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn current_json_roundtrip() {
        let (_t, p) = tmp_paths();
        assert_eq!(read_current(&p).unwrap(), None);
        write_current(&p, "dms").unwrap();
        assert_eq!(read_current(&p).unwrap().as_deref(), Some("dms"));
        clear_current(&p).unwrap();
        assert_eq!(read_current(&p).unwrap(), None);
        clear_current(&p).unwrap();
    }

    #[test]
    fn pending_deps_roundtrip() {
        let (_t, p) = tmp_paths();
        let pending = PendingDeps {
            name: "dms".into(),
            commit: "abc123".into(),
            symlink_path: Some(PathBuf::from("/home/x/.config/quickshell/dms")),
            symlink_target: Some(PathBuf::from("/home/x/.cache/rice-cooker/rices/dms")),
            pre_all: vec!["quickshell".into()],
            pre_explicit: vec!["quickshell".into()],
        };
        assert_eq!(load_pending_deps(&p).unwrap(), None);
        save_pending_deps(&p, &pending).unwrap();
        assert_eq!(load_pending_deps(&p).unwrap(), Some(pending));
        clear_pending_deps(&p).unwrap();
        assert_eq!(load_pending_deps(&p).unwrap(), None);
    }

    #[test]
    fn load_rejects_future_schema_version() {
        let (_t, p) = tmp_paths();
        let path = p.record_json("x").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"schema_version":99,"name":"x","commit":"a","installed_at":"","symlink_path":"/","symlink_target":"/","pacman_diff":{}}"#,
        )
        .unwrap();
        assert!(load_record(&path).is_err());
    }

    #[test]
    fn nix_install_round_trips() {
        let (_t, p) = tmp_paths();
        let mut r = sample();
        r.nix = Some(NixInstall {
            store_path: Some(PathBuf::from("/nix/store/abc-caelestia-shell-1.0.0")),
            launch_argv: vec!["/nix/store/abc-caelestia-shell-1.0.0/bin/caelestia-shell".into()],
            snippet_path: None,
        });
        let path = p.record_json(&r.name).unwrap();
        save_record(&path, &r).unwrap();
        assert_eq!(load_record(&path).unwrap(), r);
    }

    #[test]
    fn a_record_without_a_nix_field_still_loads() {
        // Records written before the field existed must keep parsing, so a
        // downgrade stays readable.
        let (_t, p) = tmp_paths();
        let path = p.record_json("x").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"schema_version":1,"name":"x","commit":"a","installed_at":"","pacman_diff":{}}"#,
        )
        .unwrap();
        let r = load_record(&path).unwrap();
        assert!(r.nix.is_none());
    }

    #[test]
    fn an_emitted_only_install_needs_no_store_path() {
        let (_t, p) = tmp_paths();
        let mut r = sample();
        r.nix = Some(NixInstall {
            store_path: None,
            launch_argv: vec![],
            snippet_path: Some(PathBuf::from("/home/x/.local/share/rice-cooker/install-snippets/x.nix")),
        });
        let path = p.record_json(&r.name).unwrap();
        save_record(&path, &r).unwrap();
        assert_eq!(load_record(&path).unwrap(), r);
        // Neither absent-store-path nor empty-argv should reach the wire.
        let body = fs::read_to_string(&path).unwrap();
        assert!(!body.contains("store_path"), "{body}");
        assert!(!body.contains("launch_argv"), "{body}");
    }

    #[test]
    fn a_nix_record_omits_the_pacman_diff_on_the_wire() {
        let (_t, p) = tmp_paths();
        let mut r = sample();
        r.nix = Some(NixInstall {
            store_path: Some(PathBuf::from("/nix/store/abc-x")),
            launch_argv: vec!["/nix/store/abc-x/bin/x".into()],
            snippet_path: Some(PathBuf::from("/home/x/.local/share/rice-cooker/install-snippets/x.nix")),
        });
        let path = p.record_json(&r.name).unwrap();
        save_record(&path, &r).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"nix\""), "the nix block should be serialized");
        assert!(body.contains("snippet_path"));
    }
}
