# Nix-first architecture for rice-cooker (with niri as a first-class compositor)

Design + diff spec. Companion to `nix-recon-digest.md`, which holds the verified
reconnaissance this is built on.

Status of this branch: the **catalog v2 seam**, the **compositor seam** (including
niri) and the **shell-identity fix** are implemented and tested. The platform seam,
Nix activation, and the Electron/UI/Den changes are specified below but not yet
written.

---

## 0. Two decisions that drive everything

**D1 — Nix does the fetching.** On the Nix platform each rice repo becomes a flake
input of a generated state flake, pinned by `rev` and locked by `narHash`. There is
no `git clone` at runtime, so `git.rs`, `install/symlink.rs` and the clone cache stay
Arch-only. This resolves the purity conflict: a store path can feed
`xdg.configFile.source` directly, and the lock file is the pin that the old
`commit = "..."` field used to be.

**D2 — activation goes through a *standalone* Home Manager, never the NixOS module.**
`home-manager switch --specialisation NAME` exists from HM 25.11 and is verified by
HM's own `tests/integration/standalone/specialisation.nix`. The NixOS-module path
activates through `${system.build.toplevel}/specialisation/<n>/activate`, and that
toplevel only changes on a system rebuild. Worse, on this desktop
(`modules/aspects/users/avanonyme.nix` sets `useGlobalPkgs` + `useUserPackages`) the
boot service `home-manager-<user>.service` re-activates the *system-pinned*
generation, so a rice activated out of band is silently reverted at the next boot or
`nixos-rebuild`. Standalone HM is therefore the only design that satisfies
"no system rebuild" durably.

---

## 1. Compositor seam — IMPLEMENTED

`backend/src/compositor/{mod,hyprland,niri}.rs`

```rust
pub enum CompositorId { Hyprland, Niri }              // serde: "hyprland" | "niri"
pub enum Compositor { Hyprland { signature: String }, Niri { socket: PathBuf } }

impl Compositor {
    pub fn detect(env: &SessionEnv) -> Result<Self>;
    pub fn id(&self) -> CompositorId;
    /// None = IPC failed or timed out. Never "zero surfaces".
    pub fn layers(&self) -> Option<Vec<LayerSurface>>;
}

pub struct LayerSurface { namespace, output, layer, pid: Option<u32> }

/// Pure, unit-testable ownership decision.
pub fn owns_layers(snapshot: &[LayerSurface], o: &Ownership<'_>) -> bool;
pub fn compile_namespaces(id: CompositorId, declared: &[String]) -> Result<Vec<Regex>>;
```

Three differences are hidden behind it:

| | Hyprland | niri |
| --- | --- | --- |
| detection | `HYPRLAND_INSTANCE_SIGNATURE` + `$XDG_RUNTIME_DIR/hypr/<sig>/.socket.sock` must exist | `$NIRI_SOCKET`, else reconstruct `niri.<WAYLAND_DISPLAY>.<pid>.sock` |
| layer query | `hyprctl layers -j` | `"Layers"` over the UNIX socket |
| ownership evidence | `pid` per surface | **namespace only — there is no pid** |

**Why this shape.** `SessionEnv` is injected rather than read from `std::env` at the
call site, so the whole detection path is testable without touching process globals —
including the fixture procfs. `layers()` returns `Option`, because a failed IPC query
is not evidence that the shell opened no layers; collapsing the two would make a
wedged compositor look like a failed launch.

**The niri no-pid problem, solved.** Ownership is *appeared since the baseline* AND
(*pid matches* OR *namespace matches*). The baseline is a `layers()` snapshot taken
after `KillQuickshell` and before `Launch`. On Hyprland the pid half still carries;
on niri only the namespace half can fire, which is why `compile_namespaces` supplies
`^quickshell` when a rice declares nothing and the compositor is niri. The baseline
diff is a **multiset** on `(namespace, output, layer)`, not a set: otherwise a
surviving surface from the shell just evicted would be read as the new one coming up.

**Anti-skew.** Requests go over the socket directly instead of via `niri msg`, and
are parsed through `serde_json::Value` rather than typed structs. `niri msg` refuses
to talk to a compositor older than itself, and `deny_unknown_fields` would turn a
*newer* compositor into a hard failure on a working desktop.

**Verified against niri source.** The socket filename is
`format!("niri.{wayland_socket_name}.{}.sock", process::id())` in
`niri/src/ipc/server.rs::IpcServer::start`. The recon digest's original
`niri-*.sock` glob was wrong and has been corrected.

## 2. Shell identity — IMPLEMENTED

`backend/src/process.rs`

The single most damaging bug for this desktop: the running shell here is `noctalia`
(`modules/aspects/desktop/niri/settings/startup.nix`), and every shell lookup matched
`quickshell|qs` only. So the original shell would never be captured, never evicted,
and never replayed — on exactly the machine this work targets.

```rust
pub const DEFAULT_SHELL_NAMES: &[&str] = &["quickshell", "qs", "noctalia-qs", "noctalia"];
pub fn normalize_shell_name(raw: &str) -> String;   // unwraps Nix `.foo-wrapped` + store paths
pub struct ShellMatcher { /* names */ }
pub fn matching_pids(proc_root: &Path, m: &ShellMatcher) -> Result<Vec<i32>>;
pub fn kill_shells(proc_root: &Path, m: &ShellMatcher) -> Result<()>;
pub fn find_running_shell(proc_root: &Path, m: &ShellMatcher) -> Result<Option<QuickshellProc>>;
```

`pkill -x` was also structurally wrong: `-x` matches `comm`, which the kernel
truncates to 15 characters, so `.quickshell-wrapped` and store-path argv0 binaries
never matched. Pids are now discovered from procfs and signalled by pid.

`find_running_quickshell()` / `kill_quickshell()` remain as default-matcher wrappers,
so `pipeline.rs` needs no change.

## 3. Platform seam — SPECIFIED

Dispatch is **per stage**, not per package operation. A trait shaped like
`install_packages`/`remove_packages` would force Nix into package-list semantics it
does not have.

`backend/src/platform/mod.rs`

```rust
pub enum Platform { Arch(arch::ArchPacman), Nix(nix::NixHome) }   // enum dispatch

pub trait RiceRealizer {
    fn detect_env(&self) -> EnvReport;                                 // feeds `env`
    fn preflight(&self, entry: &RiceEntry, mode: ActivateMode) -> Result<()>;
    /// Replaces `deps::missing` in the same_current short-circuit.
    fn is_satisfied(&self, entry: &RiceEntry, mode: ActivateMode) -> Result<bool>;
    /// Replaces Clone + Deps + Symlink (and adds Activate on Nix).
    fn realize<W: Write>(&self, ctx: &mut StageCtx<W>, name: &str, entry: &RiceEntry,
                         mode: ActivateMode, carry: Option<&Rollback>) -> Result<Realized>;
    fn rollback<W: Write>(&self, ctx: &mut StageCtx<W>, rec: &InstallRecord) -> Result<()>;
    fn reconcile_journal(&self, paths: &Paths) -> Result<()>;           // replaces reconcile_pending_deps
}
pub struct Realized { pub launch: LaunchSpec, pub rollback: Rollback }

impl Platform {
    /// --platform / $RICE_COOKER_PLATFORM > config.toml `platform`
    /// > /etc/NIXOS or (nix on PATH && hm target configured) > /etc/arch-release + paru|yay
    pub fn detect(flag: Option<PlatformId>) -> Result<Self>;
}
```

Stays concrete and shared in `pipeline.rs`: `try_stage!`, `hello`, `step`, `emit_fail`,
`acquire_lock`, evict orchestration, `fail_and_rollback_activation`,
`replay_original_shell`, `record_original`, and the Notifiers / KillQuickshell /
Launch / Verify stages.

Moves: all of `deps.rs` plus `pacman_explicit`, `pacman_all`, `pacman_query`,
`pacman_relations_overlap_removed`, `diff_packages`, `do_clone`, `clone_cache_hit`,
`do_deps`, `DepsOutcome`, `union_sorted`, `remove_rice_symlink` and
`reconcile_pending_deps` → `platform/arch.rs`, and `deps.rs` is deleted.

**Launch abstraction** (`LaunchSpec` / `LaunchHandle`):

```rust
pub enum LaunchSpec {
    Quickshell { bin: String, config: String },   // default: quickshell -c <name>
    Argv { argv: Vec<String> },                   // amane, noctalia
    SystemdUnit { unit: String },                 // the R2 "reload" path
}
```

`SystemdUnit` uses `systemctl --user restart <unit>`; the others use
`systemd-run --user --collect --unit=rice-cooker-preview --setenv=WAYLAND_DISPLAY=…
--setenv=NIRI_SOCKET=… -p StandardOutput=file:<log>` with the existing `setsid -f` as
fallback when no user manager is reachable. `LaunchSpec::Quickshell` keeps
`pgrep -xf "quickshell -c <n>"` working today; `systemd-run` is the robust path,
because a Nix wrapper changes argv0.

## 4. Catalog v2 — IMPLEMENTED (parser) / SPECIFIED (fetch)

`backend/src/catalog.rs`. Every added field is defaulted, so the eight shipped
entries parse unchanged and `compositors` defaults to `["hyprland"]` — the v1
assumption exactly.

```toml
[_catalog]
schema = 2                                    # absent ⇒ current; > 2 ⇒ refuse the catalog

[noctalia]
display_name = "Noctalia"
creator_name = "noctalia-dev"
repo   = "https://github.com/noctalia-dev/noctalia-shell"
commit = "d7b68652e79bce5813dc4fea7e51636a5da3e1b7"   # nix: 40-hex or a ref, never a short SHA
compositors = ["hyprland", "niri"]            # default ["hyprland"]
layer_namespaces = ["^noctalia-"]             # required to verify on niri

[noctalia.launch]
kind = "argv"                                 # "quickshell" | "argv" (default: quickshell -c <name>)
argv = ["noctalia"]

[noctalia.nix]
shape = "module"                              # "module" | "package" | "dotfiles"
module = "homeModules.default"
system_module = "nixosModules.default"        # informational — never applied
system_module_required = false                # true ⇒ install_supported = false
packages = ["cliphist", "wl-clipboard"]       # nixpkgs attr paths → home.packages
follows_nixpkgs = true
mutable_config = false                        # true ⇒ activation stages a writable copy
package = "default"                           # packages.<system>.<x>, shape = "package"

[noctalia.nix.hm_config.programs.noctalia]
enable = true                                 # data only — never interpolated as Nix text
```

Validation that matters:

- Nix entries reject a **short SHA** (`is_hex` + `len != 40`): a short SHA resolves
  non-deterministically as the log grows.
- `module`, `packages` and `system_module` must be dotted `[A-Za-z0-9_-]` paths, and
  flake refs are rejected if they contain `"`, `'`, `;`, `$`, `` ` `` or a newline.
  This is the only channel by which catalog data reaches the generated Nix module, so
  it is validated rather than escaped.
- `symlink_src`/`symlink_dst` are applicable to Arch entries and `shape = "dotfiles"`
  only. A `module` rice places its own config; a `package` rice has no config tree.
  They were previously mandatory, so they became `Option` and are reached through
  `RiceEntry::symlink()` / `RiceEntry::links_into_config()`.
- `_` is now a reserved name prefix, so the metadata table can never be a rice.

**Two parse modes.** `Catalog::parse` is strict and is what CI and the bundled catalog
use — `the_bundled_catalog_parses_strictly` is a release gate. `parse_lenient` skips a
single bad entry with a warning and is for a catalog fetched at runtime. Without this,
one new-field entry breaks the whole catalog for every older binary, because
`#[serde(flatten)]` on `rices` means the top-level table *is* the entry map and there is
no room for a schema key until `_catalog` is lifted out first.

**Fetching (R6).** New subcommand `catalog update` downloads from a `catalog/v2` branch
into `$XDG_CACHE_HOME/rice-cooker/catalog.toml` with ETag/If-None-Match.
`catalog_path` resolution becomes: `--catalog` → `$RICE_COOKER_CATALOG` → cached (if it
parses at schema ≤ 2) → cwd dev paths → XDG bundled. Electron must stop forcing
`--catalog` when packaged (`backendBaseArgs` currently always passes it, which blocks
R6 even once the backend is fixed).

## 5. Nix activation — SPECIFIED

Generated artifacts live in `S = $XDG_DATA_HOME/rice-cooker/nix/`.

- **`flake.nix`** — regenerated; the only interpolated text is validated input names
  and URLs. Rice input names are `rice-<name>` with `[^A-Za-z0-9_-]` → `_`.
- **`rices.json`** — data only: `shape`, `input`, `module`, `package`, `packages`,
  `hm_config`, `skip_import`, `config_rel`, `symlink_src`, `launch_argv`, `mutable`.
- **`module.nix`** — static template, one specialisation per rice:

```nix
inputs: { lib, pkgs, ... }:
let rices = builtins.fromJSON (builtins.readFile ./rices.json);
    at = p: s: lib.getAttrFromPath (lib.splitString "." p) s;
    mk = n: r: let src = inputs.${r.input}; in {
      imports = lib.optional (r.shape == "module" && !r.skip_import) (at r.module src);
      config = lib.mkMerge [
        r.hm_config
        { home.packages = map (p: at p pkgs) r.packages;
          systemd.user.services.rice-cooker-shell.Service.ExecStart =
            lib.mkForce (lib.escapeShellArgs r.launch_argv); }
        (lib.mkIf (r.shape == "dotfiles" && !r.mutable)
          { xdg.configFile.${r.config_rel}.source = "${src}/${r.symlink_src}"; })
      ];
    };
in { specialisation = lib.mapAttrs (n: r: { configuration = mk n r; }) rices; }
```

**`skip_import`** exists because a specialisation inherits the parent config through
`extendModules`: if the base config already imports the rice's module — and on this
desktop it already imports `noctalia.homeModules.default` — importing it again errors
with "option already declared". It is probed with
`nix eval '<target>#homeConfigurations."<attr>".options' --apply 'o: o ? programs && o.programs ? noctalia'`.

Activation, as argv:

| Stage | Command |
| --- | --- |
| Clone (lock) | `nix flake lock "$S"` |
| Deps (build) | `nix build --no-link --print-out-paths --no-write-lock-file --override-input rice-cooker-state "path:$S" "$HOME/.config/nix#homeConfigurations.\"avanonyme@boreal\".activationPackage"` |
| Activate | `"$out/specialisation/<rice>/activate"`, or `home-manager switch --flake … --specialisation <rice>` on HM ≥ 25.11 |
| Launch (R2) | `systemctl --user restart rice-cooker-shell.service` |
| Revert | `<pre_generation.store_path>/activate`, then restart the base unit |

`preview_deps` has no meaning on Nix: the closure is complete either way.

**System modules degrade gracefully.** `system_module_required = false` (ChromaShell):
ignore it, warn on stderr. `system_module_required = true`: `install_supported = false`
with `unsupported_reason = "needs NixOS module (system rebuild)"`, and write a Den
aspect stub to `$S/system-snippets/<rice>.nix` for the user to adopt out of band.
`nixos-rebuild` is never invoked.

## 6. Record v2 — SPECIFIED

`SCHEMA_VERSION = 2`; `load_record` keeps accepting v1 and maps it to
`Rollback::Pacman`, so a downgrade does not break `status`/`uninstall`.

```rust
pub struct InstallRecord {
    schema_version, name, commit, installed_at,
    mode: RecordMode,            // install | preview
    compositor: CompositorId,
    launch: LaunchSpec,
    rollback: Rollback,
}
pub enum Rollback {                                   // #[serde(tag = "kind")]
    Pacman { symlink_path, symlink_target, pacman_diff },
    HomeManager { target, pre_generation: Option<HmGeneration>, generation: HmGeneration,
                  locked_rev: String, nar_hash: String },
}
pub enum Journal { Pacman(PendingDeps), Nix(PendingActivation { name, commit, pre_generation, started_at }) }
```

- `PacmanDiff` is replaced as the rollback unit by HM generations — orderable,
  enumerable, and restorable without recomputing a package diff.
- `PendingDeps` becomes one variant of `Journal`, still reading the legacy
  `pending-deps.json`.
- **Nix reconcile:** read the generation from
  `readlink ~/.local/state/nix/profiles/home-manager`; if it differs from
  `pre_generation` and `<gen>/home-files/.local/state/rice-cooker/active` names the
  journal's rice, write the record; otherwise clear the journal.
- **Evict is not `uninstall_locked` on Nix.** The new record *inherits*
  `pre_generation`, so A → B → C keeps the pre-rice generation; then the outgoing
  record is deleted. Calling uninstall would replay the original shell on every hop.
- **Revert** kills via `LaunchHandle`, activates `pre_generation.store_path`, and
  restarts the base unit. In integrated mode the base `rice-cooker-shell.service` *is*
  the original, so no argv replay; `replay_original_shell` remains for own-Arch mode.
  If `argv[0]` is a garbage-collected store path, resolve its basename on `PATH`.

## 7. Remaining diff list

| File | Change |
| --- | --- |
| `backend/src/platform/{mod,arch,nix/{mod,gen,hm}}.rs` | new — §3, §5 |
| `backend/src/deps.rs` | deleted (content moves to `platform/arch.rs`) |
| `backend/src/main.rs` | add `Env`, `CatalogUpdate`, `NixSetup` subcommands; `step` emits `activate` |
| `backend/src/events.rs` | **⚠ schema bump to 2**: new `activate` step, `Hello { …, platform, compositor }`. The pinned wire cases in `every_variant_roundtrips_through_ndjson_schema` must be updated in the same commit |
| `backend/src/install/record.rs` | **⚠** `SCHEMA_VERSION` 1→2, `Rollback`, `Journal` (§6) |
| `backend/src/install/pipeline.rs` | `run_activate` takes `Platform`; preflight fails `compositor_unsupported` when `!entry.supports(session.compositor.id())`; capture the layer baseline between KillQuickshell and Launch; `ListRow` gains `compositors`, `unsupported_reason` |
| `electron/main/index.ts` | `environmentCheck` → `execFileAsync(backendBin(), ['env'])`, dropping `/etc/arch-release`, the `HYPRLAND_INSTANCE_SIGNATURE` gate and `executableInPath('quickshell')`. Delete `CONFLICTING_SHELLS` (it moves to the backend and must be scanned on **every** compositor — today it is skipped unless Hyprland). Rename `applyHyprlandWindowProps` → `applyCompositorWindowProps` (niri is a no-op; niri has no runtime `setprop`). Only pass `--catalog` when `!app.isPackaged` |
| `src/shared/backend.ts` | `EnvironmentCheckResult` gains `compositor`, `platform`, `reasons`; `RiceListRow` gains `compositors`, `unsupported_reason?` |
| `src/pages/pick-a-rice/PickARice.tsx` | boot copy from `result.reasons[0]`; **do not rename** the `deps`/`launch`/`verify` step names — progress keys off them |
| `src/pages/pick-a-rice/components/BootScreen.tsx` | same copy change; compositor-conditional sticker |
| `nix/hm-module-niri.nix` | new, exported as `homeManagerModules.niri` — a niri window-rule for `app-id="^(rice-cooker\|electron)$"`, `title="^Rice Cooker$"`: `geometry-corner-radius 0`, `clip-to-geometry true`, `shadow off`, `border off`, `focus-ring off`, `open-floating true` |
| `packaging/aur/rice-cooker/PKGBUILD` | `hyprland` moves to `optdepends` alongside `niri` |
| `~/.config/nix/modules/aspects/desktop/rice-cooker.nix` | new Den aspect (§8) |

## 8. Den integration

```nix
{ inputs, ... }: {
  flake-file.inputs.rice-cooker = {
    url = "github:amarsbar/rice-cooker";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  # The generated state flake, so `home-manager switch` in ~/.config/nix
  # activates rice specialisations alongside the base config.
  flake-file.inputs.rice-cooker-state = {
    url = "path:/home/avanonyme/.local/share/rice-cooker/nix";
    inputs.nixpkgs.follows = "nixpkgs";
    inputs.home-manager.follows = "home-manager";
  };

  den.aspects.desktop.rice-cooker.homeManager = { pkgs, ... }: {
    imports = [
      inputs.rice-cooker-state.homeManagerModules.default
      inputs.rice-cooker.homeManagerModules.niri
    ];
    home.packages = [ inputs.rice-cooker.packages.${pkgs.stdenv.hostPlatform.system}.default ];

    # The base shell is declaratively the "original rice".
    systemd.user.services.rice-cooker-shell = {
      Unit.PartOf = [ "graphical-session.target" ];
      Service.ExecStart =
        "${inputs.noctalia.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/noctalia";
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
```

Plus: remove `{_args = [ "noctalia" ];}` from
`modules/aspects/desktop/niri/settings/startup.nix` — that file already starts
`graphical-session.target`, and the shell is now a unit rice-cooker can restart.

Adopting this requires moving `avanonyme@boreal` to a **standalone Den home**
(`hmContext { home }`) and dropping NixOS-module HM for that user only. `gamer` and
`tux` are unaffected.

## 9. Risks

1. **Two Home Manager instances for one user.** Both track "the old generation"
   through the same per-user profile and gcroot, so each would clean up the other's
   files. A second HM instance is refused when
   `~/.local/state/home-manager/gcroots/current-home` or `home-manager-$USER.service`
   exists. Boreal requires the standalone migration instead. **Unverified** — read
   `modules/lib-bash/activation-init.sh` and `modules/files.nix` at the pinned HM rev
   before shipping the owned mode.
2. **Duplicate option declarations** when the base already imports the rice's module.
   Handled by the `skip_import` probe; if the probe fails, eval errors are surfaced
   cleanly rather than swallowed.
3. **Stale lock.** A plain `home-manager switch` in `~/.config/nix` without
   `--override-input` activates the stale `rice-cooker-state` and silently drops the
   rice. Mitigated by `nix flake update rice-cooker-state` after a successful install,
   at the cost of a dirty `flake.lock`.
4. **Pre-rice generation garbage-collected.** Falls back to activating the base
   config; the record's store path is pinned with `nix-store --add-root` under
   `$XDG_DATA_HOME/rice-cooker/gcroots/`.
5. **Store paths are read-only** and break rices that write into their own config
   directory. Needs `mutable_config` per rice: activation copies `${src}` once into
   `$XDG_DATA_HOME/rice-cooker/mutable/<n>` and links there via
   `config.lib.file.mkOutOfStoreSymlink`.
6. **A remote catalog executes arbitrary flake eval and activation scripts as the
   user** with no release review in between. Require 40-hex revisions, HTTPS only, and
   a one-time consent prompt before the first Nix activation of a rice not in the
   bundled catalog.
7. **`systemd-run --user` environment.** The user manager may lack
   `WAYLAND_DISPLAY`/`NIRI_SOCKET`, so pass them with `--setenv`; fall back to
   `setsid` when `systemctl --user` is unreachable.
8. **Specialisation activate vs `home-manager switch --specialisation`.** Activating
   a specialisation directly sets the profile head to that specialisation's
   generation, so revert must use the recorded `pre_generation`, not `--rollback`,
   when other generations intervened.
9. **Niri rices that need `layer-rule`s** (noctalia's overview backdrop, for example)
   cannot change niri's HM-generated `config.kdl` unless they share the Home Manager
   instance. Integrated mode can set
   `wayland.windowManager.niri.settings.layer-rule` inside the specialisation and niri
   reloads on change; owned mode cannot, so such rices are declared unsupported there.

## 10. Open verifications (need boreal)

- `ps -eo pid,comm,args | grep -Ei 'qs|quickshell|noctalia'` — confirm the shell
  process name and whether a Nix wrapper is in the path.
- `ls $XDG_RUNTIME_DIR | grep niri` — confirm the socket filename in practice.
- `home-manager --version` — confirm ≥ 25.11 for `--specialisation`.
- `nix eval .#homeConfigurations` once a standalone Den home exists.
- `amane`'s real launch subcommand (its flake exposes only `packages.default`).

Test surface per phase: `cargo test` + `cargo insta review` in `backend/`, and
`npm run typecheck` after any change touching `src/shared/backend.ts`.
