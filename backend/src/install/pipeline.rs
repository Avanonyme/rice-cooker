//! preview / install / uninstall / list / status pipeline.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::catalog::{Catalog, PreviewMode, RiceEntry};
use crate::compositor;
use crate::deps;
use crate::events::{
    ConfigFormat, Event, EventWriter, SCHEMA_VERSION as EVENT_SCHEMA_VERSION, Step, StepState,
};
use crate::git;
use crate::lock::{Lock, LockError};
use crate::paths::{OriginalShell, Paths, expand_config_path};
use crate::platform::{self, PlatformId};
use crate::process::{self, VerifyResult};
use regex::Regex;

use super::record::{
    InstallRecord, NixInstall, PacmanDiff, PendingDeps, SCHEMA_VERSION, clear_current,
    clear_pending_deps, load_pending_deps, load_record, read_current, save_pending_deps,
    save_record, write_current,
};
use super::symlink as symlink_shape;

#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListRow {
    pub name: String,
    pub display_name: String,
    pub creator_name: String,
    pub repo: String,
    pub install_supported: bool,
    pub installed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusRow {
    pub installed: Option<InstallRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivateMode {
    Install,
    Preview,
}

/// How the rice's shell is started once realized, and therefore how it is
/// verified. Arch launches `quickshell -c <name>` and is verified by process
/// name; Nix launches an explicit argv from a store path and is verified by
/// argv matching plus layer ownership, because the store path is not a fixed
/// program name.
enum LaunchPlan {
    ByName,
    Nix {
        launch_argv: Vec<String>,
        namespaces: Vec<Regex>,
    },
}

impl ActivateMode {
    fn subcommand(self) -> &'static str {
        match self {
            ActivateMode::Install => "install",
            ActivateMode::Preview => "preview",
        }
    }

    fn deps_for(self, entry: &RiceEntry) -> &[String] {
        match self {
            ActivateMode::Install => &entry.install_deps,
            ActivateMode::Preview => &entry.preview_deps,
        }
    }
}

/// Unwrap or emit a Fail event and return Ok(false). Post-hello contract.
macro_rules! try_stage {
    ($events:expr, $stage:literal, $expr:expr $(,)?) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                emit_fail($events, $stage, &format!("{e:#}"), None)?;
                return Ok(false);
            }
        }
    };
    ($events:expr, $stage:literal, $reason_prefix:literal, $expr:expr $(,)?) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                emit_fail(
                    $events,
                    $stage,
                    &format!("{}: {:#}", $reason_prefix, e),
                    None,
                )?;
                return Ok(false);
            }
        }
    };
}

pub fn run_install<W: Write>(
    cat: &Catalog,
    paths: &Paths,
    name: &str,
    events: &mut EventWriter<W>,
) -> Result<bool> {
    run_activate(cat, paths, name, ActivateMode::Install, events)
}

pub fn run_preview<W: Write>(
    cat: &Catalog,
    paths: &Paths,
    name: &str,
    events: &mut EventWriter<W>,
) -> Result<bool> {
    run_activate(cat, paths, name, ActivateMode::Preview, events)
}

fn run_activate<W: Write>(
    cat: &Catalog,
    paths: &Paths,
    name: &str,
    mode: ActivateMode,
    events: &mut EventWriter<W>,
) -> Result<bool> {
    hello(events, mode.subcommand())?;
    try_stage!(events, "init", paths.ensure_rices());
    try_stage!(events, "init", paths.ensure_installs());
    let Some(_lock) = acquire_lock(paths, events)? else {
        return Ok(false);
    };
    try_stage!(events, "deps", reconcile_pending_deps(paths));

    let entry = match cat.get(name) {
        Some(e) => e,
        None => {
            emit_fail(
                events,
                "preflight",
                &format!("{name}: not in catalog"),
                None,
            )?;
            return Ok(false);
        }
    };

    let platform = match platform::detect() {
        Ok(platform) => platform,
        Err(e) => {
            emit_fail(events, "preflight", &format!("{e:#}"), None)?;
            return Ok(false);
        }
    };

    if mode == ActivateMode::Install && !entry.install_is_supported(platform) {
        let reason = match platform {
            PlatformId::Arch => format!(
                "{name}: install is not supported; use preview instead"
            ),
            PlatformId::Nix => format!(
                "{name}: install is not supported: the catalog entry declares no [nix] block, \
                 so there is no configuration to emit"
            ),
        };
        emit_fail(events, "preflight", &reason, None)?;
        return Ok(false);
    }

    // ── install on a declarative platform ────────────────────────────────────
    // `install` produces configuration the user adopts; it cannot mutate the
    // system. So it builds nothing, launches nothing, and takes no graphical
    // session — which also means it never kills the shell you are running, and
    // works over SSH.
    if platform == PlatformId::Nix && mode == ActivateMode::Install {
        return emit_install_config(paths, events, name, entry);
    }

    let selected_deps = mode.deps_for(entry);

    step(events, Step::Preflight, StepState::Start)?;
    try_stage!(events, "preflight", "git", git::preflight());
    let session = try_stage!(
        events,
        "preflight",
        "graphical_session",
        process::check_graphical_session()
    );
    // The compositor is a declared capability, not an assumption. A Hyprland-only
    // rice must fail cleanly on niri rather than half-install.
    let compositor_id = session.compositor_id();
    if !entry.supports(compositor_id) {
        let declared = entry
            .compositors
            .iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        emit_fail(
            events,
            "preflight",
            &format!("{name}: does not support {compositor_id} (declares {declared})"),
            None,
        )?;
        return Ok(false);
    }
    // On Nix the flake *is* the realization, so an entry with no `[nix]` block
    // cannot be previewed at all. Refuse in preflight rather than failing in the
    // middle of the pipeline.
    if platform == PlatformId::Nix && entry.nix.is_none() {
        emit_fail(
            events,
            "preflight",
            &format!(
                "{name}: declared for Arch only (no [nix] block), so it cannot be \
                 realized on Nix"
            ),
            None,
        )?;
        return Ok(false);
    }

    // A source preview launches the system's quickshell — the rice is only its
    // configuration — so report a missing shell here rather than failing opaquely.
    if platform == PlatformId::Nix
        && mode == ActivateMode::Preview
        && entry
            .nix
            .as_ref()
            .is_some_and(|nix| nix.preview_mode(entry) == PreviewMode::QuickshellSource)
        && platform::which("quickshell").is_none()
    {
        emit_fail(
            events,
            "preflight",
            &format!(
                "{name}: previewing this rice needs `quickshell` on PATH — the rice is \
                 configuration only, and the shell it configures is the system's"
            ),
            None,
        )?;
        return Ok(false);
    }

    // Only Arch needs an AUR helper and its polkit prompt.
    if platform == PlatformId::Arch && !selected_deps.is_empty() {
        try_stage!(
            events,
            "preflight",
            "polkit_agent",
            deps::check_polkit_agent()
        );
    }
    let current = try_stage!(events, "preflight", read_current(paths));
    if !paths.original_is_recorded() {
        try_stage!(
            events,
            "preflight",
            "record_original",
            record_original(paths)
        );
    }
    step(events, Step::Preflight, StepState::Done)?;

    let mut prior_pacman_diff = PacmanDiff::default();
    let same_current = current.as_deref() == Some(name);
    if same_current {
        // current.json can be stale (crash, manual kill) — only short-circuit if
        // the shell is actually running and the rice's requirements are met.
        let record_path = try_stage!(events, "record", "path", paths.record_json(name));
        let record = try_stage!(events, "record", "load", load_record(&record_path));
        let alive = match &record.nix {
            Some(nix) => try_stage!(events, "liveness", shell_alive_for(nix)),
            None => try_stage!(events, "liveness", process::rice_shell_alive(name)),
        };
        // A Nix rice has no package work: the store path is the whole closure.
        let satisfied = match &record.nix {
            Some(_) => true,
            None => try_stage!(events, "deps", deps::missing(selected_deps)).is_empty(),
        };
        if alive && satisfied {
            // On Nix, `install` must still emit its artifact. Short-circuiting
            // here reported success for an install that wrote nothing, which is
            // exactly what a preview-then-install does.
            if mode == ActivateMode::Install
                && platform == PlatformId::Nix
                && record.nix.as_ref().is_some_and(|n| n.snippet_path.is_none())
                && let Some(nix) = record.nix.as_ref()
            {
                let snippet = try_stage!(
                    events,
                    "record",
                    platform::write_install_snippet(paths, entry, name, platform::compositor_hint())
                );
                step(events, Step::Record, StepState::Start)?;
                try_stage!(
                    events,
                    "record",
                    do_record_nix(
                        paths,
                        name,
                        entry,
                        nix.store_path.as_deref(),
                        &nix.launch_argv,
                        Some(snippet)
                    )
                );
                step(events, Step::Record, StepState::Done)?;
            }
            events.emit(&Event::Success {
                active: Some(name.to_string()),
            })?;
            return Ok(true);
        }
        prior_pacman_diff = record.pacman_diff;
    }

    // Evict outgoing rice (no replay — we launch a new one next).
    if let Some(outgoing) = current.as_deref().filter(|_| !same_current) {
        step(events, Step::Evict, StepState::Start)?;
        if !uninstall_locked(paths, Flags::default(), events, outgoing)? {
            return Ok(false);
        }
        step(events, Step::Evict, StepState::Done)?;
    }

    // ── realize: platform-specific ──────────────────────────────────────────
    // Arch clones a repo and installs packages, then links a config directory.
    // Nix builds a flake and needs neither: a Nix-packaged shell is a wrapper
    // carrying its own config path, so there is nothing to symlink.
    let mut last_tree: Option<PathBuf> = None;
    let plan: LaunchPlan = if platform == PlatformId::Nix {
        let Some(nix) = entry.nix.as_ref() else {
            // Unreachable given the preflight guard above; kept graceful so a
            // reordering cannot turn this into a panic.
            emit_fail(
                events,
                "preflight",
                &format!("{name}: no [nix] block to realize"),
                None,
            )?;
            return Ok(false);
        };
        let flake = nix.flake_ref(&entry.repo, &entry.commit);

        // How this rice is previewed is a declared property, not a derivation from
        // how it installs: a compiled shell must be built, a quickshell dotfiles
        // rice must be fetched and pointed at, and a module-only rice cannot be
        // previewed at all.
        step(events, Step::Deps, StepState::Start)?;
        let store_path = match nix.preview_mode(entry) {
            PreviewMode::Unsupported => {
                emit_fail(
                    events,
                    "preflight",
                    &format!(
                        "{name}: this rice has no runnable artifact — no `build`, and no \
                         symlink_src to point a shell at — so it can be installed but not \
                         previewed. Set `nix.preview` if that is wrong."
                    ),
                    None,
                )?;
                return Ok(false);
            }
            PreviewMode::Package => {
                let attr = nix
                    .build_attr()
                    .expect("a Package preview implies a declared build attribute");
                match platform::build_store_path(&flake, attr) {
                    Ok(path) => Some(path),
                    Err(e) => {
                        emit_fail(events, "deps", &format!("{e:#}"), None)?;
                        return Ok(false);
                    }
                }
            }
            PreviewMode::QuickshellSource => {
                let tree = match platform::fetch_source(&entry.repo, &entry.commit) {
                    Ok(path) => path,
                    Err(e) => {
                        emit_fail(events, "deps", &format!("{e:#}"), None)?;
                        return Ok(false);
                    }
                };
                // Keep the tree for the argv below; no store path is recorded
                // because nothing was built.
                last_tree = Some(tree);
                None
            }
        };
        step(events, Step::Deps, StepState::Done)?;

        let launch_argv = match (&store_path, last_tree.as_deref()) {
            (Some(store), _) => platform::launch_argv(entry, name, store),
            (None, Some(tree)) => {
                match platform::source_launch_argv(entry, tree) {
                    Ok(argv) => argv,
                    Err(e) => {
                        emit_fail(events, "launch", &format!("{e:#}"), None)?;
                        return Ok(false);
                    }
                }
            }
            (None, None) => {
                emit_fail(events, "launch", &format!("{name}: nothing to launch"), None)?;
                return Ok(false);
            }
        };
        let namespaces = try_stage!(
            events,
            "preflight",
            "layer_namespaces",
            compositor::compile_namespaces(compositor_id, &entry.layer_namespaces)
        );
        // On Nix, `install` cannot mutate a declarative system, so it emits the
        // configuration the user adopts instead.
        let snippet_path = if mode == ActivateMode::Install {
            Some(try_stage!(
                events,
                "record",
                platform::write_install_snippet(paths, entry, name, platform::compositor_hint())
            ))
        } else {
            None
        };

        step(events, Step::Record, StepState::Start)?;
        try_stage!(
            events,
            "record",
            do_record_nix(
                paths,
                name,
                entry,
                store_path.as_deref(),
                &launch_argv,
                snippet_path
            )
        );
        step(events, Step::Record, StepState::Done)?;

        LaunchPlan::Nix {
            launch_argv,
            namespaces,
        }
    } else {
        step(events, Step::Clone, StepState::Start)?;
        try_stage!(events, "clone", do_clone(paths, name, entry));
        step(events, Step::Clone, StepState::Done)?;

        step(events, Step::Deps, StepState::Start)?;
        let deps_outcome = try_stage!(events, "deps", do_deps(paths, name, entry, selected_deps));
        if deps_outcome.install_error.is_none() {
            step(events, Step::Deps, StepState::Done)?;
        }
        let mut pacman_diff = deps_outcome.pacman_diff;
        let current_run_changed =
            !pacman_diff.added_explicit.is_empty() || !pacman_diff.removed.is_empty();
        pacman_diff.added_explicit =
            union_sorted(prior_pacman_diff.added_explicit, pacman_diff.added_explicit);
        if same_current {
            pacman_diff.removed = union_sorted(prior_pacman_diff.removed, pacman_diff.removed);
        }
        if let Some(reason) = deps_outcome.install_error {
            if current_run_changed {
                step(events, Step::Record, StepState::Start)?;
                try_stage!(events, "record", do_record(paths, name, entry, pacman_diff));
                try_stage!(events, "record", clear_pending_deps(paths));
                step(events, Step::Record, StepState::Done)?;
                return fail_and_rollback_activation(paths, events, name, "deps", &reason, None);
            } else {
                try_stage!(events, "deps", clear_pending_deps(paths));
            }
            emit_fail(events, "deps", &reason, None)?;
            return Ok(false);
        }

        // Record persists BEFORE symlink so a symlink failure still leaves
        // a record uninstall can use to roll back the packages.
        step(events, Step::Record, StepState::Start)?;
        try_stage!(events, "record", do_record(paths, name, entry, pacman_diff));
        try_stage!(events, "record", clear_pending_deps(paths));
        step(events, Step::Record, StepState::Done)?;

        step(events, Step::Symlink, StepState::Start)?;
        if let Err(e) = do_symlink(paths, name, entry) {
            return fail_and_rollback_activation(
                paths,
                events,
                name,
                "symlink",
                &format!("{e:#}"),
                None,
            );
        }
        step(events, Step::Symlink, StepState::Done)?;

        LaunchPlan::ByName
    };

    step(events, Step::Notifiers, StepState::Start)?;
    if let Err(e) = process::kill_notif_daemons() {
        return fail_and_rollback_activation(
            paths,
            events,
            name,
            "notifiers",
            &format!("{e:#}"),
            None,
        );
    }
    step(events, Step::Notifiers, StepState::Done)?;

    // A Nix rice's binary is a store path, so the default matcher would leave a
    // previously previewed shell alive and two shells would fight for surfaces.
    let incoming_shell = match &plan {
        LaunchPlan::Nix { launch_argv, .. } => launch_argv
            .first()
            .map(|argv0| process::normalize_shell_name(argv0))
            .into_iter()
            .collect::<Vec<String>>(),
        LaunchPlan::ByName => Vec::new(),
    };
    step(events, Step::KillQuickshell, StepState::Start)?;
    if let Err(e) = process::kill_quickshell_with(&incoming_shell) {
        return fail_and_rollback_activation(
            paths,
            events,
            name,
            "kill_quickshell",
            &format!("{e:#}"),
            None,
        );
    }
    step(events, Step::KillQuickshell, StepState::Done)?;

    // Snapshot taken after eviction and before launch: a surface the outgoing
    // shell left behind must not be read as the new shell having come up. Only
    // the Nix plan needs it; on Hyprland the pid evidence alone suffices.
    let baseline = match &plan {
        LaunchPlan::Nix { .. } => session.compositor.layers().unwrap_or_default(),
        LaunchPlan::ByName => Vec::new(),
    };

    let log_file = paths.last_run_log();
    step(events, Step::Launch, StepState::Start)?;
    let launched = match &plan {
        LaunchPlan::ByName => process::launch_detached_by_name(name, &log_file, &paths.home),
        LaunchPlan::Nix { launch_argv, .. } => {
            process::launch_argv(launch_argv, &paths.home, &log_file)
        }
    };
    if let Err(e) = launched {
        let tail = read_tail(&log_file);
        return fail_and_rollback_activation(
            paths,
            events,
            name,
            "launch",
            &format!("{e:#}"),
            Some(tail),
        );
    }
    step(events, Step::Launch, StepState::Done)?;

    step(events, Step::Verify, StepState::Start)?;
    let verify_result = match &plan {
        LaunchPlan::ByName => process::verify_by_name(name, &log_file),
        LaunchPlan::Nix {
            launch_argv,
            namespaces,
        } => {
            let matcher = match launch_argv.first() {
                Some(argv0) => {
                    process::ShellMatcher::new([process::normalize_shell_name(argv0)])
                }
                None => process::ShellMatcher::new([name]),
            };
            let probe = process::OwnershipProbe {
                compositor: &session.compositor,
                namespaces,
                baseline: &baseline,
            };
            process::verify_argv(
                &matcher,
                Path::new(process::PROC_ROOT),
                &log_file,
                Some(&probe),
            )
        }
    };
    let verify_result = match verify_result {
        Ok(r) => r,
        Err(e) => {
            let tail = read_tail(&log_file);
            return fail_and_rollback_activation(
                paths,
                events,
                name,
                "verify",
                &format!("{e:#}"),
                Some(tail),
            );
        }
    };
    match verify_result {
        VerifyResult::Ok => step(events, Step::Verify, StepState::Done)?,
        VerifyResult::Dead { reason, log_tail } => {
            // `reason` distinguishes "the process is gone" from "the compositor
            // listed none of its surfaces" and "its config failed to load", which
            // the old hardcoded `qs_exited` conflated into one misleading slug.
            return fail_and_rollback_activation(
                paths,
                events,
                name,
                "verify",
                reason,
                Some(log_tail),
            );
        }
    }

    events.emit(&Event::Success {
        active: Some(name.to_string()),
    })?;
    Ok(true)
}

pub fn run_uninstall<W: Write>(
    paths: &Paths,
    flags: Flags,
    events: &mut EventWriter<W>,
) -> Result<bool> {
    hello(events, "uninstall")?;
    try_stage!(events, "init", paths.ensure_rices());
    try_stage!(events, "init", paths.ensure_installs());
    let Some(_lock) = acquire_lock(paths, events)? else {
        return Ok(false);
    };
    try_stage!(events, "deps", reconcile_pending_deps(paths));

    step(events, Step::Preflight, StepState::Start)?;
    let current = try_stage!(events, "preflight", read_current(paths));
    step(events, Step::Preflight, StepState::Done)?;

    let Some(name) = current else {
        events.emit(&Event::Success { active: None })?;
        return Ok(true);
    };

    if !uninstall_locked(paths, flags, events, &name)? {
        return Ok(false);
    }
    if !replay_original_shell(paths, events)? {
        return Ok(false);
    }
    events.emit(&Event::Success { active: None })?;
    Ok(true)
}

/// Kill qs and clear state for `name`.
fn uninstall_locked<W: Write>(
    paths: &Paths,
    flags: Flags,
    events: &mut EventWriter<W>,
    name: &str,
) -> Result<bool> {
    // A tampered current.json must surface as a Fail, not bare Err (post-hello contract).
    let record_path = try_stage!(events, "record", "path", paths.record_json(name));
    let record = try_stage!(events, "record", "load", load_record(&record_path));

    // `caelestia-shell` is not `quickshell`, so a Nix rice needs its own argv0 to
    // be matched or it survives its own uninstall.
    let outgoing_shell = match &record.nix {
        Some(nix) => nix
            .launch_argv
            .first()
            .map(|argv0| process::normalize_shell_name(argv0))
            .into_iter()
            .collect::<Vec<String>>(),
        None => Vec::new(),
    };
    step(events, Step::KillQuickshell, StepState::Start)?;
    try_stage!(
        events,
        "kill_quickshell",
        process::kill_quickshell_with(&outgoing_shell)
    );
    step(events, Step::KillQuickshell, StepState::Done)?;

    // A Nix record has no packages to remove and no symlink to drop: the store
    // path is garbage-collected on its own, and undo is simply stopping the
    // shell, which the KillQuickshell step below already did. The emitted config
    // snippet, however, is a real file that revert has to take back.
    if let Some(nix) = &record.nix {
        if let Some(snippet) = &nix.snippet_path {
            step(events, Step::Symlink, StepState::Start)?;
            try_stage!(
                events,
                "symlink",
                "remove_snippet",
                platform::remove_install_snippet(snippet)
            );
            step(events, Step::Symlink, StepState::Done)?;
        }
    }
    if record.nix.is_none() {
    // Pre-filter via pacman -Q so retries don't abort on "target not found".
    step(events, Step::Deps, StepState::Start)?;
    if !record.pacman_diff.added_explicit.is_empty() {
        let still = try_stage!(
            events,
            "deps",
            "filter_installed",
            deps::installed(&record.pacman_diff.added_explicit)
        );
        let removed: HashSet<&str> = record
            .pacman_diff
            .removed
            .iter()
            .map(String::as_str)
            .collect();
        let mut remove_first = Vec::new();
        for pkg in still {
            let is_replacement = !removed.is_empty()
                && try_stage!(
                    events,
                    "deps",
                    "classify_replacement",
                    pacman_relations_overlap_removed(&pkg, &removed)
                );
            if !is_replacement {
                remove_first.push(pkg);
            }
        }
        if !remove_first.is_empty() {
            try_stage!(
                events,
                "deps",
                "remove_packages",
                deps::remove_packages(&remove_first)
            );
        }
    }
    if !record.pacman_diff.removed.is_empty() {
        try_stage!(
            events,
            "deps",
            "restore_removed",
            deps::install_packages(&record.pacman_diff.removed)
        );
    }
    step(events, Step::Deps, StepState::Done)?;

    step(events, Step::Symlink, StepState::Start)?;
    try_stage!(events, "symlink", remove_rice_symlink(&record));
    step(events, Step::Symlink, StepState::Done)?;
    }

    // Clear the pointer (current.json) BEFORE the target (record) — so if the
    // record removal then fails, status still reports None sanely.
    step(events, Step::Record, StepState::Start)?;
    try_stage!(events, "record", "clear_current", clear_current(paths));
    match fs::remove_file(&record_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if flags.force => eprintln!(
            "rice-cooker: warn: --force: could not remove record {}: {e}",
            record_path.display()
        ),
        Err(e) => {
            emit_fail(
                events,
                "record",
                &format!("removing record {}: {e}", record_path.display()),
                None,
            )?;
            return Ok(false);
        }
    }
    step(events, Step::Record, StepState::Done)?;
    Ok(true)
}

fn fail_and_rollback_activation<W: Write>(
    paths: &Paths,
    events: &mut EventWriter<W>,
    name: &str,
    stage: &str,
    reason: &str,
    log_tail: Option<String>,
) -> Result<bool> {
    if !uninstall_locked(paths, Flags::default(), events, name)? {
        return Ok(false);
    }
    if !replay_original_shell(paths, events)? {
        return Ok(false);
    }
    emit_fail(events, stage, reason, log_tail)?;
    Ok(false)
}

fn replay_original_shell<W: Write>(paths: &Paths, events: &mut EventWriter<W>) -> Result<bool> {
    let original = try_stage!(events, "replay", "read_original", paths.original());
    let replay_err = if let Some(shell) = original
        && !shell.argv.is_empty()
    {
        step(events, Step::Replay, StepState::Start)?;
        let cwd = shell
            .cwd
            .as_deref()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("/"));
        // Its own log: this runs after a failure, and the rice's output is the
        // evidence we still need.
        let log = paths.last_replay_log();
        match process::launch_argv(&shell.argv, cwd, &log) {
            Ok(()) => {
                step(events, Step::Replay, StepState::Done)?;
                None
            }
            Err(e) => Some((format!("{e:#}"), read_tail(&log))),
        }
    } else {
        None
    };
    let clear_err = paths.clear_original().err();
    match (replay_err, clear_err) {
        (Some((reason, tail)), None) => {
            emit_fail(events, "replay", &reason, Some(tail))?;
            Ok(false)
        }
        (Some((reason, tail)), Some(ce)) => {
            // Include the clear_original error — dropping it leaves a silent
            // landmine: stale `original` on disk will mis-replay on the next install.
            emit_fail(
                events,
                "replay",
                &format!("{reason}; and clear_original also failed: {ce:#}"),
                Some(tail),
            )?;
            Ok(false)
        }
        (None, Some(ce)) => {
            emit_fail(events, "replay", &format!("clear_original: {ce:#}"), None)?;
            Ok(false)
        }
        (None, None) => Ok(true),
    }
}

fn remove_rice_symlink(record: &InstallRecord) -> Result<()> {
    let (Some(symlink_path), Some(symlink_target)) = (&record.symlink_path, &record.symlink_target)
    else {
        return Ok(());
    };
    let md = match fs::symlink_metadata(symlink_path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow!("reading {}: {e}", symlink_path.display())),
    };
    if !md.file_type().is_symlink() {
        eprintln!(
            "rice-cooker: skipping {}: not a symlink anymore",
            symlink_path.display()
        );
        return Ok(());
    }
    let target = fs::read_link(symlink_path)
        .with_context(|| format!("read_link {}", symlink_path.display()))?;
    if target != *symlink_target {
        eprintln!(
            "rice-cooker: skipping {}: target is {target:?}, expected {:?} (user-retargeted?)",
            symlink_path.display(),
            symlink_target
        );
        return Ok(());
    }
    fs::remove_file(symlink_path)
        .with_context(|| format!("removing symlink {}", symlink_path.display()))
}

/// `platform` is a parameter rather than a hidden `detect()` call so the output
/// is reproducible: on a machine that happens to have both `nix` and `pacman` on
/// PATH, a hidden read would make this list environment-dependent.
pub fn list(cat: &Catalog, paths: &Paths, platform: PlatformId) -> Result<Vec<ListRow>> {
    let current = read_current(paths)?;
    Ok(cat
        .rices
        .iter()
        .map(|(name, entry)| ListRow {
            name: name.clone(),
            display_name: entry.display_name.clone(),
            creator_name: entry.creator_name.clone(),
            repo: entry.repo.clone(),
            install_supported: entry.install_is_supported(platform),
            installed: current.as_deref() == Some(name.as_str()),
        })
        .collect())
}

pub fn status(paths: &Paths) -> Result<StatusRow> {
    let Some(name) = read_current(paths)? else {
        return Ok(StatusRow { installed: None });
    };
    Ok(StatusRow {
        installed: Some(load_record(&paths.record_json(&name)?)?),
    })
}

// ── install step helpers ──────────────────────────────────────────────────────

/// True when HEAD matches `commit`; accepts either side as a prefix.
fn clone_cache_hit(clone_dir: &Path, commit: &str) -> bool {
    if !clone_dir.join(".git").exists() {
        return false;
    }
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(clone_dir)
        .args(["rev-parse", "HEAD"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let head = stdout.trim();
    head.starts_with(commit) || commit.starts_with(head)
}

fn do_clone(paths: &Paths, name: &str, entry: &RiceEntry) -> Result<()> {
    if entry.package_managed {
        return Ok(());
    }
    let clone = paths.clone_dir(name)?;
    let tmp = clone.with_extension("rctmp");
    remove_dir_all_forceful(&tmp)?;
    if clone_cache_hit(&clone, &entry.commit) {
        return Ok(());
    }
    git::clone_at_commit(&entry.repo, &entry.commit, &tmp)?;
    if clone.exists() {
        remove_dir_all_forceful(&clone)
            .with_context(|| format!("removing stale clone {}", clone.display()))?;
    }
    fs::rename(&tmp, &clone)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), clone.display()))
}

struct DepsOutcome {
    pacman_diff: PacmanDiff,
    install_error: Option<String>,
}

fn do_deps(
    paths: &Paths,
    name: &str,
    entry: &RiceEntry,
    all_deps: &[String],
) -> Result<DepsOutcome> {
    if all_deps.is_empty() {
        return Ok(DepsOutcome {
            pacman_diff: PacmanDiff::default(),
            install_error: None,
        });
    }
    if deps::missing(all_deps)?.is_empty() {
        return Ok(DepsOutcome {
            pacman_diff: PacmanDiff::default(),
            install_error: None,
        });
    }
    let pre_all = pacman_all().context("pacman -Qq pre-snapshot")?;
    let pre_explicit = pacman_explicit().context("pacman -Qqe pre-snapshot")?;
    save_pending_deps(
        paths,
        &PendingDeps {
            name: name.to_string(),
            commit: entry.commit.clone(),
            symlink_path: symlink_path_for(paths, entry),
            symlink_target: symlink_target_for(paths, name, entry)?,
            pre_all: pre_all.clone(),
            pre_explicit: pre_explicit.clone(),
        },
    )?;
    let install_result = deps::install_packages(all_deps);
    let post_all = pacman_all().context("pacman -Qq post-snapshot")?;
    let post_explicit = pacman_explicit().context("pacman -Qqe post-snapshot")?;
    Ok(DepsOutcome {
        pacman_diff: PacmanDiff {
            added_explicit: diff_packages(&pre_explicit, &post_explicit),
            removed: diff_packages(&post_all, &pre_all),
        },
        install_error: install_result.err().map(|e| format!("{e:#}")),
    })
}

/// Where the rice's config symlink should point, or `None` when nothing is
/// linked. Shared by the pending-deps journal and the install record so the two
/// can never disagree about what uninstall has to undo.
fn symlink_path_for(paths: &Paths, entry: &RiceEntry) -> Option<PathBuf> {
    if !entry.links_into_config() {
        return None;
    }
    entry
        .symlink()
        .map(|(_, dst)| expand_config_path(dst, &paths.home, &paths.config_home))
}

/// What that symlink points at, or `None` when nothing is linked.
fn symlink_target_for(paths: &Paths, name: &str, entry: &RiceEntry) -> Result<Option<PathBuf>> {
    if !entry.links_into_config() {
        return Ok(None);
    }
    match entry.symlink() {
        Some((src, _)) => Ok(Some(paths.clone_dir(name)?.join(src))),
        None => Ok(None),
    }
}

fn reconcile_pending_deps(paths: &Paths) -> Result<()> {
    let Some(pending) = load_pending_deps(paths)? else {
        return Ok(());
    };
    let post_all = pacman_all().context("pacman -Qq pending-deps recovery snapshot")?;
    let post_explicit = pacman_explicit().context("pacman -Qqe pending-deps recovery snapshot")?;
    let pacman_diff = PacmanDiff {
        added_explicit: diff_packages(&pending.pre_explicit, &post_explicit),
        removed: diff_packages(&post_all, &pending.pre_all),
    };
    if !pacman_diff.added_explicit.is_empty() || !pacman_diff.removed.is_empty() {
        let record = InstallRecord {
            schema_version: SCHEMA_VERSION,
            name: pending.name.clone(),
            commit: pending.commit,
            installed_at: InstallRecord::now_rfc3339(),
            symlink_path: pending.symlink_path,
            symlink_target: pending.symlink_target,
            // The pending-deps journal only ever covers the Arch path.
            nix: None,
            pacman_diff,
        };
        save_record(&paths.record_json(&pending.name)?, &record)?;
        write_current(paths, &pending.name)?;
    }
    clear_pending_deps(paths)
}

fn union_sorted(mut prior: Vec<String>, next: Vec<String>) -> Vec<String> {
    prior.extend(next);
    prior.sort();
    prior.dedup();
    prior
}

fn do_record(paths: &Paths, name: &str, entry: &RiceEntry, pacman_diff: PacmanDiff) -> Result<()> {
    let record = InstallRecord {
        schema_version: SCHEMA_VERSION,
        name: name.to_string(),
        commit: entry.commit.clone(),
        installed_at: InstallRecord::now_rfc3339(),
        symlink_path: symlink_path_for(paths, entry),
        symlink_target: symlink_target_for(paths, name, entry)?,
        nix: None,
        pacman_diff,
    };
    save_record(&paths.record_json(name)?, &record)?;
    write_current(paths, name)
}

/// The install record for a rice realized from a Nix flake.
fn do_record_nix(
    paths: &Paths,
    name: &str,
    entry: &RiceEntry,
    store_path: Option<&Path>,
    launch_argv: &[String],
    snippet_path: Option<PathBuf>,
) -> Result<()> {
    let record = InstallRecord {
        schema_version: SCHEMA_VERSION,
        name: name.to_string(),
        commit: entry.commit.clone(),
        installed_at: InstallRecord::now_rfc3339(),
        // Nothing is linked and no package diff exists on Nix.
        symlink_path: None,
        symlink_target: None,
        nix: Some(NixInstall {
            store_path: store_path.map(Path::to_path_buf),
            launch_argv: launch_argv.to_vec(),
            snippet_path,
        }),
        pacman_diff: PacmanDiff::default(),
    };
    save_record(&paths.record_json(name)?, &record)?;
    write_current(paths, name)
}

/// Is the shell recorded for this Nix rice still running?
///
/// Not `rice_shell_alive`: that matches `quickshell -c <name>`, and a Nix shell
/// is a store path whose argv0 basename is the rice's own binary.
/// `install` on Nix: write the configuration, record where, and hand it to the UI.
fn emit_install_config<W: Write>(
    paths: &Paths,
    events: &mut EventWriter<W>,
    name: &str,
    entry: &RiceEntry,
) -> Result<bool> {
    let snippet = try_stage!(
        events,
        "record",
        platform::write_install_snippet(paths, entry, name, platform::compositor_hint())
    );
    let text = match fs::read_to_string(&snippet) {
        Ok(text) => text,
        Err(e) => {
            emit_fail(
                events,
                "record",
                &format!("reading back {}: {e}", snippet.display()),
                None,
            )?;
            return Ok(false);
        }
    };

    step(events, Step::Record, StepState::Start)?;
    try_stage!(
        events,
        "record",
        do_record_nix(paths, name, entry, None, &[], Some(snippet.clone()))
    );
    step(events, Step::Record, StepState::Done)?;

    events.emit(&Event::Config {
        format: ConfigFormat::Nix,
        text,
        path: Some(snippet),
    })?;
    events.emit(&Event::Success {
        active: Some(name.to_string()),
    })?;
    Ok(true)
}

fn shell_alive_for(nix: &NixInstall) -> Result<bool> {
    let Some(argv0) = nix.launch_argv.first() else {
        return Ok(false);
    };
    let matcher = process::ShellMatcher::new([process::normalize_shell_name(argv0)]);
    Ok(!process::matching_pids(Path::new(process::PROC_ROOT), &matcher)?.is_empty())
}

fn do_symlink(paths: &Paths, name: &str, entry: &RiceEntry) -> Result<()> {
    if !entry.links_into_config() {
        return Ok(());
    }
    let clone = paths.clone_dir(name)?;
    symlink_shape::create_symlink(&clone, entry, &paths.home, &paths.config_home)
        .context("run `rice-cooker-backend uninstall` to roll back")
}

// ── NDJSON + misc helpers ─────────────────────────────────────────────────────

fn hello<W: Write>(events: &mut EventWriter<W>, subcommand: &str) -> Result<()> {
    events.emit(&Event::Hello {
        version: EVENT_SCHEMA_VERSION,
        subcommand: subcommand.to_string(),
    })?;
    Ok(())
}

fn step<W: Write>(events: &mut EventWriter<W>, step: Step, state: StepState) -> Result<()> {
    events.emit(&Event::Step { step, state })?;
    Ok(())
}

fn emit_fail<W: Write>(
    events: &mut EventWriter<W>,
    stage: &str,
    reason: &str,
    log_tail: Option<String>,
) -> Result<()> {
    events.emit(&Event::Fail {
        stage: stage.to_string(),
        reason: reason.to_string(),
        plugins: None,
        log_tail,
    })?;
    Ok(())
}

fn acquire_lock<W: Write>(paths: &Paths, events: &mut EventWriter<W>) -> Result<Option<Lock>> {
    match Lock::try_acquire(&paths.lock()) {
        Ok(l) => Ok(Some(l)),
        Err(LockError::AlreadyHeld) => {
            emit_fail(events, "lock", "already_held", None)?;
            Ok(None)
        }
        Err(LockError::Io(e)) => {
            emit_fail(events, "lock", "io", Some(format!("{e:#}")))?;
            Ok(None)
        }
    }
}

fn read_tail(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(c) => process::tail_lines(&c, 20),
        Err(e) => format!("<log unreadable at {}: {}>", path.display(), e),
    }
}

fn record_original(paths: &Paths) -> Result<()> {
    match process::find_running_quickshell()? {
        Some(proc) => paths.set_original(Some(&OriginalShell {
            argv: proc.cmdline,
            cwd: proc.cwd.map(|p| p.to_string_lossy().into_owned()),
        })),
        None => paths.set_original(None),
    }
}

fn pacman_explicit() -> Result<Vec<String>> {
    pacman_query(&["-Qqe"])
}

fn pacman_all() -> Result<Vec<String>> {
    pacman_query(&["-Qq"])
}

fn pacman_relations_overlap_removed(pkg: &str, removed: &HashSet<&str>) -> Result<bool> {
    let out = Command::new("pacman")
        .env("LC_ALL", "C")
        .args(["-Qi", pkg])
        .output()
        .with_context(|| format!("running pacman -Qi {pkg}"))?;
    if !out.status.success() {
        return Err(anyhow!("pacman -Qi {pkg} exited {:?}", out.status.code()));
    }

    Ok(String::from_utf8_lossy(&out.stdout).lines().any(|line| {
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        matches!(key.trim(), "Provides" | "Conflicts With" | "Replaces")
            && value
                .split_whitespace()
                .filter(|name| *name != "None")
                .map(|name| {
                    name.split_once(['=', '<', '>'])
                        .map_or(name, |(base, _)| base)
                })
                .any(|name| removed.contains(name))
    }))
}

fn pacman_query(args: &[&str]) -> Result<Vec<String>> {
    let out = Command::new("pacman")
        .args(args)
        .output()
        .with_context(|| format!("running pacman {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "pacman {} exited {:?}",
            args.join(" "),
            out.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

fn diff_packages(pre: &[String], post: &[String]) -> Vec<String> {
    use std::collections::HashSet;
    let pre_set: HashSet<&str> = pre.iter().map(String::as_str).collect();
    let mut added: Vec<String> = post
        .iter()
        .filter(|p| !pre_set.contains(p.as_str()))
        .cloned()
        .collect();
    added.sort();
    added
}

/// Fall back to `rm -rf --` — std::fs can't traverse makepkg's 0111 `pkg/`.
fn remove_dir_all_forceful(path: &Path) -> Result<()> {
    let fs_err = match fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => e,
    };
    let status = Command::new("rm")
        .arg("-rf")
        .arg("--")
        .arg(path)
        .status()
        .map_err(|rm_err| {
            anyhow!(
                "spawning rm -rf {}: {rm_err} (after std::fs::remove_dir_all failed: {fs_err})",
                path.display()
            )
        })?;
    if !status.success() {
        return Err(anyhow!(
            "rm -rf {} exited {:?} (after std::fs::remove_dir_all failed: {fs_err})",
            path.display(),
            status.code()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn diff_packages_finds_added() {
        let pre = vec!["a".into(), "b".into()];
        let post = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert_eq!(diff_packages(&pre, &post), vec!["c", "d"]);
    }

    #[test]
    fn diff_packages_ignores_removed_and_unchanged() {
        let pre: Vec<String> = vec!["a".into(), "b".into()];
        let shrunk: Vec<String> = vec!["a".into()];
        assert!(diff_packages(&pre, &shrunk).is_empty());
        assert!(diff_packages(&pre, &pre).is_empty());
    }

    fn init_repo_at(dir: &Path) -> String {
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("git spawn")
                .success()
                .then_some(())
                .expect("git ok");
        };
        fs::create_dir_all(dir).unwrap();
        run(&["init"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        fs::write(dir.join("README"), b"rice").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn clone_cache_hit_for_matching_head() {
        let t = tempfile::tempdir().unwrap();
        let sha = init_repo_at(t.path());
        assert!(clone_cache_hit(t.path(), &sha));
        assert!(clone_cache_hit(t.path(), &sha[..7]));
    }

    #[test]
    fn clone_cache_invalidated_for_differing_head() {
        let t = tempfile::tempdir().unwrap();
        let _sha = init_repo_at(t.path());
        assert!(!clone_cache_hit(t.path(), "deadbeef00000000"));
        let t2 = tempfile::tempdir().unwrap();
        assert!(!clone_cache_hit(t2.path(), "deadbeef"));
        assert!(!clone_cache_hit(&t2.path().join("not-there"), "deadbeef"));
    }

    fn tmp_paths() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().to_path_buf();
        let cache = home.join(".cache/rice-cooker");
        let data = home.join(".local/share/rice-cooker");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&data).unwrap();
        let p = Paths::at_roots(home, cache, data);
        p.ensure_rices().unwrap();
        p.ensure_installs().unwrap();
        (t, p)
    }

    fn entry_with_deps() -> RiceEntry {
        RiceEntry {
            display_name: "X".into(),
            creator_name: "x".into(),
            repo: "https://x".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            symlink_src: Some(".".into()),
            symlink_dst: Some("~/.config/quickshell/x".into()),
            package_managed: false,
            preview_deps: vec!["preview-a".into(), "preview-b".into()],
            install_deps: vec!["install-a".into(), "install-b".into()],
            compositors: vec![crate::compositor::CompositorId::Hyprland],
            layer_namespaces: vec![],
            launch: None,
            nix: None,
            interactive: false,
        }
    }

    #[test]
    fn activate_mode_selects_only_its_dependency_set() {
        let entry = entry_with_deps();
        assert_eq!(
            ActivateMode::Install.deps_for(&entry),
            &["install-a".to_string(), "install-b".to_string()]
        );
        assert_eq!(
            ActivateMode::Preview.deps_for(&entry),
            &["preview-a".to_string(), "preview-b".to_string()]
        );
    }

    #[test]
    fn union_sorted_sorts_and_dedups() {
        assert_eq!(
            union_sorted(
                vec!["zsh".into(), "fish".into()],
                vec!["fish".into(), "starship".into()]
            ),
            vec![
                "fish".to_string(),
                "starship".to_string(),
                "zsh".to_string()
            ]
        );
    }

    #[test]
    fn run_uninstall_is_idempotent_when_nothing_is_installed() {
        let (_t, paths) = tmp_paths();
        let mut buf = Vec::new();
        {
            let mut events = EventWriter::new(&mut buf);
            assert!(run_uninstall(&paths, Flags::default(), &mut events).unwrap());
        }
        let out = std::str::from_utf8(&buf).unwrap();
        assert!(out.contains(r#""type":"success""#));
        assert!(!out.contains(r#""type":"fail""#));
        assert!(!out.contains(r#""step":"kill_quickshell""#));
        assert!(!out.contains(r#""step":"deps""#));
    }

    #[test]
    fn run_install_emits_fail_for_missing_catalog_entry() {
        let (_t, paths) = tmp_paths();
        let cat = Catalog::default();
        let mut buf = Vec::new();
        {
            let mut events = EventWriter::new(&mut buf);
            assert!(!run_install(&cat, &paths, "x", &mut events).unwrap());
        }
        let out = std::str::from_utf8(&buf).unwrap();
        assert!(out.contains(r#""stage":"preflight""#));
        assert!(out.contains("not in catalog"));
    }

    #[test]
    fn run_install_refuses_preview_only_entry() {
        let (_t, paths) = tmp_paths();
        let cat = Catalog::parse(
            r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "https://x"
            commit = "0123456789abcdef0123456789abcdef01234567"
            symlink_src = "."
            symlink_dst = "~/.config/quickshell/x"
            "#,
        )
        .unwrap();
        let mut buf = Vec::new();
        {
            let mut events = EventWriter::new(&mut buf);
            assert!(!run_install(&cat, &paths, "x", &mut events).unwrap());
        }
        let out = std::str::from_utf8(&buf).unwrap();
        assert!(out.contains(r#""subcommand":"install""#));
        assert!(out.contains(r#""stage":"preflight""#));
        assert!(out.contains("install is not supported"));
    }

    #[test]
    fn run_preview_uses_preview_subcommand() {
        let (_t, paths) = tmp_paths();
        let cat = Catalog::default();
        let mut buf = Vec::new();
        {
            let mut events = EventWriter::new(&mut buf);
            assert!(!run_preview(&cat, &paths, "x", &mut events).unwrap());
        }
        let out = std::str::from_utf8(&buf).unwrap();
        assert!(out.contains(r#""subcommand":"preview""#));
        assert!(out.contains(r#""stage":"preflight""#));
        assert!(out.contains("not in catalog"));
    }

    #[test]
    fn run_preview_does_not_refuse_preview_only_entry() {
        let (_t, paths) = tmp_paths();
        let cat = Catalog::parse(
            r#"
            [x]
            display_name = "X"
            creator_name = "x"
            repo = "-invalid-local-repo"
            commit = "0123456789abcdef0123456789abcdef01234567"
            symlink_src = "."
            symlink_dst = "~/.config/quickshell/x"
            "#,
        )
        .unwrap();
        let mut buf = Vec::new();
        {
            let mut events = EventWriter::new(&mut buf);
            assert!(!run_preview(&cat, &paths, "x", &mut events).unwrap());
        }
        let out = std::str::from_utf8(&buf).unwrap();
        assert!(out.contains(r#""subcommand":"preview""#));
        assert!(!out.contains("install is not supported"));
    }
}
