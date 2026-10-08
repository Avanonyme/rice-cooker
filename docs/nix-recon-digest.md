# rice-cooker — recon digest for Nix architecture planning

Phase-1 recon output. Facts below are verified against the working tree at
`/Users/avanonyme/Code/rice-cooker` (fork: `Avanonyme/rice-cooker`, upstream
`amarsbar/rice-cooker`, branch `feat/nix-rices`).

---

## 1. What the project is

Electron + React desktop toy that browses a curated catalog of Hyprland/quickshell
"rices", live-previews one on the running desktop, installs it, and can revert to
whatever shell was running before.

- `backend/` — Rust CLI (`rice-cooker-backend`), ~3.7k LOC, the entire install engine.
- `electron/main/index.ts` — Electron main; spawns the backend, parses NDJSON events,
  relays to the renderer over IPC.
- `src/` — React renderer. Talks only through `window.rice` (preload bridge).
- `packaging/aur/` — Arch PKGBUILD and launcher.

## 2. Current data flow (exact)

1. `main.rs::catalog_path` resolves `catalog.toml` (`--catalog` flag →
   `$RICE_COOKER_CATALOG` → cwd dev paths → XDG data lookup).
2. `catalog.rs::Catalog::parse` → `IndexMap<String, RiceEntry>`.
   `RiceEntry` fields (all required unless noted):
   `display_name`, `creator_name`, `repo`, `commit`, `symlink_src`,
   `symlink_dst` (must start `~/`), `package_managed` (default false),
   `preview_deps` (default []), `install_deps` (default []), `interactive` (default false,
   `true` is rejected).
   `#[serde(deny_unknown_fields)]` — adding a field is a breaking parse change
   for any entry that sets it, so new fields are additive-safe only when defaulted.
3. `install::run_activate` (pipeline.rs, 1105 LOC) drives every mode. Stage order:
   `Preflight → Evict → Clone → Deps → Record → Symlink → Notifiers → KillQuickshell
   → Launch → Verify`; uninstall adds `Replay`.
4. Clone: `git::clone_at_commit` → full clone + `git checkout --detach <commit>` into
   `$XDG_CACHE_HOME/rice-cooker/rices/<name>/`.
5. Deps: `deps.rs::install_packages` spawns `paru`/`yay` with `--sudo pkexec --useask -S
   --needed --noconfirm`; `check_polkit_agent()` preflights via `pgrep -f` over a
   hardcoded agent list, else starts `hyprpolkitagent.service`.
   Ownership/rollback is computed from `pacman -Qq` / `-Qqe` snapshots taken in
   `do_deps` and diffed by `diff_packages`; the diff is persisted as
   `PacmanDiff { added_explicit, removed }`.
6. Symlink: `install/symlink.rs::create_symlink` makes
   `~/.config/quickshell/<name>` → `<clone>/<symlink_src>` via create-at-`.rctmp` +
   `rename()`. Refuses to replace a real dir or regular file. `package_managed = true`
   skips the symlink entirely.
7. Record: `~/.local/share/rice-cooker/installs/<name>.json`
   (`InstallRecord { schema_version, name, commit, installed_at, symlink_path,
   symlink_target, pacman_diff }`) plus `current.json` = `{"name": ...}`.
   `PendingDeps` at `pending-deps.json` is the crash-recovery journal, reconciled at
   the top of every run by `reconcile_pending_deps`.
8. Launch: `process::launch_detached_by_name` → `setsid -f quickshell -c <name>`,
   stdout/stderr to `last-run.log`.
9. Verify: `process::verify_by_name` polls `pgrep -xf "quickshell -c <name>"` for 10 s,
   fails on `Failed to load configuration`, and calls `hyprland_owns_layers` which
   shells `timeout --signal=KILL 1 hyprctl layers -j` and looks for the qs pids in the
   layer list.
10. Original-shell capture / replay: `Paths::original_file` holds
    `OriginalShell { argv, cwd }` captured by scanning `/proc/*/cmdline` in
    `find_running_quickshell`; replayed on uninstall.
11. Events: `events.rs` NDJSON on stdout —
    `hello | step{step,state} | success{active} | fail{stage,reason,plugins,log_tail}`.
    `SCHEMA_VERSION = 1`. Wire forms are pinned by unit tests in `events.rs`.
12. Locking: single advisory `flock` at `$XDG_CACHE_HOME/rice-cooker/lock`
    (`lock.rs`), non-blocking; contention is a `fail` event with `stage: "lock"`.

## 3. Hardcoded Arch/Hyprland couplings (the refactor surface)

| Symbol | File | Coupling |
| --- | --- | --- |
| `deps::Helper` / `install_packages` / `remove_packages` | `backend/src/deps.rs` | paru/yay + `pkexec pacman -Rns` |
| `deps::is_installed` / `missing` / `installed` | `backend/src/deps.rs` | `pacman -Q` |
| `check_polkit_agent` | `backend/src/deps.rs` | `pgrep` list, `systemctl --user start hyprpolkitagent` |
| `pacman_explicit` / `pacman_all` / `pacman_query` / `pacman_relations_overlap_removed` / `diff_packages` | `backend/src/install/pipeline.rs` | `pacman -Qq/-Qqe/-Qi` |
| `PacmanDiff` | `backend/src/install/record.rs` | pacman diff as the rollback unit |
| `check_graphical_session` | `backend/src/process.rs` | demands `HYPRLAND_INSTANCE_SIGNATURE` |
| `hyprland_owns_layers` + `verify_by_name` | `backend/src/process.rs` | `hyprctl layers -j` |
| `launch_detached_by_name` | `backend/src/process.rs` | always `quickshell -c <name>` |
| `kill_quickshell` / `rice_shell_alive` | `backend/src/process.rs` | `pgrep -x quickshell\|qs` |
| `environmentCheck` | `electron/main/index.ts` | `existsSync('/etc/arch-release')`, `HYPRLAND_INSTANCE_SIGNATURE`, `executableInPath('quickshell')` |
| `CONFLICTING_SHELLS = ['waybar','ags','astal','eww','yambar']` | `electron/main/index.ts` | not compositor-parameterised |
| `HYPRLAND_WINDOW_EFFECTS` + `applyHyprlandWindowProps` | `electron/main/index.ts` | raw `hyprctl dispatch setprop` |
| `depends=('hyprland' 'quickshell-git' …)` | `packaging/aur/rice-cooker/PKGBUILD` | package-level |
| boot screen gate | `src/pages/pick-a-rice/components/BootScreen.tsx`, `PickARice.tsx` | Hyprland-only copy |
| `find_running_quickshell` | `backend/src/process.rs` | `/proc` scan matching argv0 basename against `quickshell\|qs` — **misses `noctalia`**, which is what `modules/aspects/desktop/niri/settings/startup.nix` actually spawns on boreal |
| `kill_quickshell` / `rice_shell_alive` | `backend/src/process.rs` | `pkill -x` / `pgrep -x` match `comm`, which is truncated to 15 chars for `.quickshell-wrap…` (Nix wrappers) and is `noctalia` here |

Grep confirms **zero** occurrences of `niri` anywhere in the tree.

`src/shared/backend.ts` is the typed IPC contract: `BackendEvent`, `RiceListRow`,
`BackendRunRequest { command: 'preview'|'install'|'uninstall'; name? }`,
`EnvironmentCheckResult { supported, conflictingShells }`.
Adding a `BackendCommand` is a two-file change (backend `Cmd` enum + this union).

## 4. Upstream intent

- Issue **#20 "Nix support & Nix flakes"** (open, `enhancement`): maintainer states
  Arch is bad for managing quickshell rices, *"Next update will likely add support for
  Nix flakes and NixOS. Rices should be recommended to be packaged with nix flakes.
  Overall current yay/paru architecture should generally work with current catalog.
  Nix-only architecture reduces maintenance as project grows."*
- Issue **#22 "niri support"** (open, `enhancement`): a developer offers a PR.
- Issue **#30 "Support for other distributions"** (open): asks to abstract the package
  manager and system deps.
- `catalog.toml`'s own header comment already says: *"We recommend packaging your rice
  with nix flakes. Rice cooker will be updated to support them soon."*
- Catalog today: 8 entries (`noctalia`, `linux-retroism`, `dms`, `zephyr`, `caelestia`,
  `whisker`, `ryu-shell`, `nandoroid`). Only `caelestia` is `package_managed`.

## 5. Target-side facts (verified)

### 5.1 Home Manager specialisations — the no-rebuild activation primitive

- Option: `specialisation.<name>.configuration` — `attrsOf submodule`, experimental,
  defined in `modules/misc/specialisation.nix`. US spelling `specialization` is a
  renamed-alias. Names may not contain `/`.
- Mechanism: `home.extraBuilderCommands` symlinks each specialisation's
  `home.activationPackage` into `$out/specialisation/<name>` of the parent generation.
- **HM ≥ 25.11 adds `home-manager switch --specialisation NAME`**, mirroring
  `nixos-rebuild switch --specialisation`. Integration test
  `tests/integration/standalone/specialisation.nix` asserts the exact failure text
  `The configuration does not contain the specialisation "<name>"`.
- **Standalone-only caveat:** that integration test is under `standalone/`. With HM as
  a *NixOS module*, tests/integration/nixos uses
  `${system.build.toplevel}/specialisation/<name>/activate` — i.e. the NixOS path goes
  through the system toplevel, which is a system rebuild. 25.11 also stopped creating
  the per-user shadow `home-manager` profile when HM is used as a NixOS module
  (`home-manager.enableLegacyProfileManagement = true` restores it).
- Manual's own note: activation creates a new Home Manager generation; older guidance
  was to run the specialisation's `activate` script by hand.
- `home-manager switch --rollback` exists; generations are listed by
  `home-manager generations`.

**Consequence for this design:** to be genuinely system-rebuild-free, rice activation
must target a *standalone* HM configuration (its own flake), not the NixOS-module HM.

### 5.2 niri IPC (for the compositor abstraction)

- niri exposes a UNIX socket at `$NIRI_SOCKET`; `niri msg` is a thin wrapper.
  `niri msg --json <cmd>` returns JSON.
- `niri-ipc` `Request` variants confirmed in-source: `Version, Outputs, Workspaces,
  Windows, Layers, KeyboardLayouts, FocusedOutput, FocusedWindow, PickWindow,
  PickColor, Action(Action), Output{..}, EventStream, ...`.
- `Reply::Layers` carries `Vec<LayerSurface { namespace, output, layer,
  keyboard_interactivity }>`.
  **There is no `pid` field on `LayerSurface`** — unlike Hyprland's `hyprctl layers -j`,
  which does expose `pid`. Liveness verification on niri therefore cannot reuse the
  pid-matching body of `hyprland_owns_layers`; it must match on surface `namespace`
  (e.g. `^noctalia-`, `quickshell`) or fall back to the existing liveness+log-clean path.
- **Socket filename, confirmed in `niri/src/ipc/server.rs::IpcServer::start`:**
  `format!("niri.{wayland_socket_name}.{}.sock", process::id())` — i.e.
  `niri.wayland-1.1234.sock`, where `wayland_socket_name` is the `WAYLAND_DISPLAY`
  value. The earlier `niri-*.sock` glob in this digest was wrong.
- Known instability: `$NIRI_SOCKET` is not always exported (niri issue #2149; fix PR
  #1967 proposes deriving it from `WAYLAND_DISPLAY`). So detection must try
  `$NIRI_SOCKET` then reconstruct the filename above from `$WAYLAND_DISPLAY`, not
  assume the env var.
**Correction — the original claim here was wrong.** It said upstream quickshell has
no niri layer-shell support. Measured against the quickshell tree instead of the
issue tracker:

- **Zero niri references** in `src/` — no `Quickshell.Niri`, no niri code at all.
- Layer surfaces are created through `zwlr_layer_shell_v1`
  (`src/wayland/wlr_layershell/`), which is the standard, compositor-agnostic
  protocol. **niri implements it**, so quickshell renders layer shells on niri
  without any niri-specific support — observed here as niri-caelestia's
  `caelestia-background`, `caelestia-drawers` and `caelestia-border-exclusion`
  surfaces in `niri msg layers`.

What quickshell lacks is niri-specific *bindings* — workspaces, windows, IPC —
which rices work around by shelling out to `niri msg` (jutraim's `services/Niri.qml`
does exactly that, 26 calls). So the question for a rice is not "does the
compositor have layer-shell support" but "does this rice's configuration import a
compositor-specific API". That is decidable by scanning the tree, which is what
`docs/nix-architecture.md` §8 calls T2.

Measured for the four configuration-only rices, by counting `Quickshell.Hyprland`
imports:

| rice | `Quickshell.Hyprland` | verdict |
|---|---|---|
| zephyr | 0 (the lone Hyprland use is `hyprctl` in a theme script) | renders on niri |
| linux-retroism | 1 (`taskbar/Workspaces.qml`) | renders, taskbar breaks |
| whisker | 7 | bars/workspaces are Hyprland-bound |
| nandoroid | 53 | same |
- niri layer rules are declarative in `config.kdl`, e.g.
  `layer-rule { match namespace="^noctalia-overview*"; place-within-backdrop true; }`.

### 5.3 Shape of the three named example flakes

**`SecLBL/ChromaShell-Flake`** — deployment-only flake wrapping a separate dotfiles repo.

- `flake.nix` inputs: `nixpkgs`, `dotfiles` (`flake = false`, the dotfiles repo),
  `caelestia-shell`, `caelestia-cli`, `spicetify-nix`, `zen-browser`.
- Outputs: `homeManagerModules.default = import ./home-module.nix inputs` and
  `nixosModules.default`.
- `home-module.nix` exposes `programs.chromashell.{enable, ...}` with typed sub-options
  (`browserDefs`, `editorDefs`), and deploys dotfiles via
  `xdg.configFile."hypr/…".source = "${dots}/hypr/…"` plus a `runCommand` that
  `chmod -R +x` a scripts dir. (An earlier revision of this digest claimed an
  `xcfdg.configFile` key; that was a transcription error — `grep -rn xcfdg` over the
  flake returns nothing.)
- `nixos-module.nix` exposes `programs.chromashell-system.{enable, hyprland.enable,
  desktop.enable, audio.enable}` — i.e. the flake *itself* splits HM-level from
  system-level, and the system module is explicitly opt-outable.
- **Hyprland-only.** No compositor option.

**`MystiaFin/amane`** — a Rust Wayland shell framework + CLI (not a dotfiles rice).

- `flake.nix` is `packages.<system>.default = rustPlatform.buildRustPackage` plus a
  `devShells.<system>.default`. No HM module, no NixOS module, no overlay.
- Ships `install.sh` for non-Nix users that branches on `pacman`/`apt-get`/`dnf` —
  i.e. it is exactly the "distro-abstracted dep install" problem in miniature.
- The CLI runs `cargo` on the *user's* config at runtime, so `postFixup` wraps the
  binary with `PATH`, `PKG_CONFIG_PATH`, `LIBRARY_PATH`, `LD_LIBRARY_PATH`.
- Consumption is therefore `packages.<system>.default` + a launch command, not a module.

**`noctalia-dev/noctalia-shell`** — the reference "packaged shell" rice.

- `flake.nix` outputs: `overlays.default`, `packages.<system>.{default, cuda(deprecated)}`,
  `devShells`, `apps.default`, `homeModules.default`, `hjemModules.default`,
  `nixosModules.default`.
- `homeModules.default` imports `nix/home-module.nix` and sets
  `programs.noctalia.package = lib.mkDefault self.packages.<system>.default`.
  **The `homeModules.default` output is the de-facto interface for a Nix rice.**
- `nix/` contains `devshell.nix hjem-module.nix home-module.nix nixos-module.nix
  package.nix`.
- Catalog entry for it currently has `install_deps = ["noctalia-shell", …]` and
  `preview_deps = ["noctalia-qs"]` — note the quickshell *fork* requirement.

**Correction (noctalia v5.2.1).** This section described the quickshell-era
revision. noctalia is now **C++** — 794 `.cpp`, 807 `.h`, 27 `.cc`, and zero
`.qmh`/`.qml` files — and its flake has exactly one input, `nixpkgs`: no
quickshell, no QML. So `noctalia-qs` is obsolete, the layer surfaces cannot be
`quickshell:*`, and the catalog pin was building the old Qt shell. `programs.noctalia`
and `homeModules.default` are unchanged, so the module rice shape still holds.

**Cross-cutting conclusion:** a "nix rice" can be any of three shapes, and the
architecture must dispatch on shape rather than assume one:

1. **module rice** — flake exposes `homeManagerModules.default` (ChromaShell, noctalia).
2. **package rice** — flake exposes `packages.<system>.default` + a launch command
   (amane; also noctalia's fallback).
3. **dotfiles rice** — flake re-exports a dotfiles tree, no module (the current
   `catalog.toml` majority). These can be wrapped mechanically: a generated HM
   fragment doing `xdg.configFile` from `inputs.<rice>` (`flake = false`).

### 5.4 Host/user config context (Avanonyme/user-nix-config, Den framework)

- `~/.config/nix`, Den (`github:denful/den`) + `flake-parts` + `import-tree`.
  `flake.nix` is generated by `flake-file` (`nix run .#write-flake`); inputs are
  declared with `flake-file.inputs.<name> = { url; inputs.nixpkgs.follows = "nixpkgs"; }`
  inside aspect files, never hand-edited in `flake.nix`.
- Aspects live under `modules/aspects/<group>/<aspect>.nix`, composed with
  `den.aspects.<group>.<name> = { includes = [...]; nixos = {...}; homeManager = {...}; }`.
- Existing desktop aspects: `desktop/niri/niri.nix` (uses `niri-nix` from
  `git+https://codeberg.org/BANanaD3V/niri-nix`), `desktop/niri/settings/*`
  (inputs, outputs, layout, animations, startup, window-rules, keybind.nix/*),
  `desktop/noctalia-v5/noctalia.nix` (includes `security.polkit`, `desktop.niri`,
  `desktop.noctalia-greeter`; `homeManager` branch imports
  `inputs.noctalia.homeModules.default` and sets `programs.noctalia.enable = true`
  with `settings = builtins.readFile ./noctalia.toml`), `desktop/noctalia-v5/noctalia-greeter.nix`.
- Relevant inputs already present: `niri` (sodiboo/niri-flake), `niri-nix`, `noctalia`,
  `noctalia-greeter`, `home-manager`, `stylix`, `zen-browser`.
- **boreal uses NixOS-module Home Manager, not standalone.** `modules/aspects/users/avanonyme.nix`
  sets `home-manager.useGlobalPkgs = true` and `home-manager.useUserPackages = true`.
  The "for standalone home-manager" string in `define-user.nix` is Den's own
  battery/option description text, not a decision by this config. Den does support
  standalone homes (`hmContext { home }`), but adopting one is a config migration,
  not a switch.
- Desktop host is **boreal** — NixOS x86_64, **niri + noctalia**, AMD GPU.
- Den composes per-user `homeManager` branches from aspects, so an aspect *can* emit
  `specialisation.<name>.configuration`.
- No existing use of `specialisation`/`specialization` anywhere in the config.

## 6. Constraints implied by the goal

1. **No system rebuild.** `nixos-rebuild` must never be on the rice activation path.
   That rules out: the NixOS-module HM specialisation route, `environment.systemPackages`
   for rice deps, and anything requiring `programs.<x>.enable` at the system layer.
2. **HM specialisations and/or reload.** Acceptable activations: `home-manager switch
   --specialisation <rice>` (standalone), plain `home-manager switch --flake <gen>`,
   or a service-level `systemctl --user reload/restart` of an already-running shell.
3. **Niri is a first-class compositor**, alongside Hyprland. Compositor must be a
   detected/declared capability, not an assumption.
4. **Must accept the three named example flakes** (ChromaShell-Flake, amane, noctalia)
   without per-rice bespoke code in the tool.
5. **Rollback must survive.** Today it is `pacman` diff + `git` commit. In Nix it is HM
   generations + flake locks.
6. **The catalog must be able to grow without shipping a tool release.** Today
   `catalog.toml` is baked into the AUR package.
7. **Derivation purity vs runtime mutation.** HM activation is a Nix build; the current
   pipeline is imperative (clone → symlink → launch). The two cannot be mixed naively:
   `git clone` into a cache dir cannot feed a HM `xdg.configFile.source` without either
   a fixed-output derivation or an out-of-store path (which HM will copy, not link).
