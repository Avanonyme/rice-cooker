use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use rice_cooker_backend::catalog::Catalog;
use rice_cooker_backend::events::EventWriter;
use rice_cooker_backend::install::{self, Flags};
use rice_cooker_backend::paths::Paths;
use rice_cooker_backend::platform;

#[derive(Parser)]
#[command(name = "rice-cooker-backend", about = "Quickshell rice install engine")]
struct Cli {
    /// Alternate catalog file path (default: XDG-data lookup for rice-cooker/catalog.toml).
    #[arg(long, global = true)]
    catalog: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Preview <name>, installing only catalog preview dependencies.
    Preview { name: String },
    /// Install <name> fully and launch it; evicts any currently-active rice.
    Install { name: String },
    /// Uninstall the active rice and replay the pre-rice shell. Clone stays
    /// cached at `~/.cache/rice-cooker/rices/<name>/`.
    Uninstall {
        #[arg(long)]
        force: bool,
    },
    /// List catalog entries (JSON).
    List,
    /// Report the detected platform, compositor and blockers (JSON).
    Env,
    /// Report whether a rice's configuration matches the compositors it declares.
    ///
    /// Scans the rice's own config directory for compositor bindings and compares
    /// them with the catalog's `compositors`. This is the measurement that decides
    /// that field, rather than a guess.
    Compat {
        name: String,
        /// An already-realized tree or store path. Without it the rice is fetched
        /// or built, exactly as a preview would.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Print the active rice's install record (JSON).
    Status,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1), // Fail event already on stdout
        Err(e) => {
            eprintln!("rice-cooker: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    let paths = Paths::from_env()?;
    match &cli.cmd {
        Cmd::Preview { name } => {
            let cat = Catalog::from_file(&catalog_path(&paths, cli.catalog.as_deref())?)?;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let mut events = EventWriter::new(&mut lock);
            install::run_preview(&cat, &paths, name, &mut events)
        }
        Cmd::Install { name } => {
            let cat = Catalog::from_file(&catalog_path(&paths, cli.catalog.as_deref())?)?;
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let mut events = EventWriter::new(&mut lock);
            install::run_install(&cat, &paths, name, &mut events)
        }
        Cmd::Uninstall { force } => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let mut events = EventWriter::new(&mut lock);
            install::run_uninstall(&paths, Flags { force: *force }, &mut events)
        }
        Cmd::Compat { name, dir } => {
            let cat = Catalog::from_file(&catalog_path(&paths, cli.catalog.as_deref())?)?;
            let Some(entry) = cat.get(name) else {
                anyhow::bail!("{name}: not in catalog");
            };
            let nix = entry
                .nix
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{name}: no [nix] block, so there is no artifact to scan"))?;

            let artifact = match dir {
                Some(dir) => dir.clone(),
                None => match entry.preview_mode() {
                    platform::PreviewMode::Package => {
                        let attr = nix
                            .build_attr()
                            .ok_or_else(|| anyhow::anyhow!("{name}: no nix.build to build"))?;
                        platform::build_store_path(&nix.flake_ref(&entry.repo, &entry.commit), attr)?
                    }
                    platform::PreviewMode::QuickshellSource => {
                        platform::fetch_source(&entry.repo, &entry.commit)?
                    }
                    platform::PreviewMode::Unsupported => anyhow::bail!(
                        "{name}: declares no runnable artifact, so there is nothing to scan"
                    ),
                },
            };

            // Scope to the rice's own config directory when it has one, so
            // Hyprland-side theming elsewhere in a dotfiles repo is not counted as
            // part of the rice.
            let scoped = entry
                .symlink_src
                .as_deref()
                .map(|src| artifact.join(src))
                .filter(|path| path.is_dir());
            let scoped_to_rice = scoped.is_some();
            let root = scoped.unwrap_or_else(|| artifact.clone());

            let counts = platform::scan_compositor_bindings(&root)?;
            let verdicts: Vec<serde_json::Value> = entry
                .compositors
                .iter()
                .map(|id| {
                    serde_json::json!({
                        "compositor": id,
                        "verdict": platform::compat_verdict(*id, &counts),
                    })
                })
                .collect();

            serde_json::to_writer_pretty(
                std::io::stdout(),
                &serde_json::json!({
                    "name": name,
                    "artifact": artifact,
                    "scanned": root,
                    "scope": if scoped_to_rice { "symlink_src" } else { "artifact root" },
                    "files_scanned": counts.files_scanned,
                    "bindings": { "hyprland": counts.hyprland, "niri": counts.niri },
                    "declared": entry.compositors,
                    "verdicts": verdicts,
                }),
            )?;
            println!();
            Ok(true)
        }
        Cmd::Env => {
            let report = platform::env_report();
            serde_json::to_writer_pretty(std::io::stdout(), &report)?;
            println!();
            Ok(true)
        }
        Cmd::List => {
            let cat = Catalog::from_file(&catalog_path(&paths, cli.catalog.as_deref())?)?;
            let rows = install::list(
                &cat,
                &paths,
                platform::detect()?,
                platform::compositor_hint(),
            )?;
            serde_json::to_writer_pretty(std::io::stdout(), &rows)?;
            println!();
            Ok(true)
        }
        Cmd::Status => {
            let row = install::status(&paths)?;
            serde_json::to_writer_pretty(std::io::stdout(), &row)?;
            println!();
            Ok(true)
        }
    }
}

/// Resolve the catalog location in preference order:
/// 1. `--catalog` flag
/// 2. `$RICE_COOKER_CATALOG` env var
/// 3. CWD-relative dev paths (`./backend/catalog.toml`, `./catalog.toml`)
/// 4. `Paths::find_catalog()` — walks `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`
///    looking for `rice-cooker/catalog.toml` (standard XDG Base Directory lookup
///    for read-only application data; the packaged install lands here).
fn catalog_path(paths: &Paths, flag: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("RICE_COOKER_CATALOG")
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    let cwd = std::env::current_dir()?;
    for rel in ["backend/catalog.toml", "catalog.toml"] {
        let p = cwd.join(rel);
        if p.exists() {
            return Ok(p);
        }
    }
    if let Some(p) = paths.find_catalog() {
        return Ok(p);
    }
    let xdg_list = paths
        .searched_catalog_paths()
        .into_iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(anyhow::anyhow!(
        "no catalog found. Tried:\n  \
         --catalog flag, $RICE_COOKER_CATALOG\n  \
         ./backend/catalog.toml, ./catalog.toml (cwd: {})\n{}\n\
         Install the rice-cooker package, pass --catalog <path>, or set \
         RICE_COOKER_CATALOG.",
        cwd.display(),
        xdg_list
    ))
}
