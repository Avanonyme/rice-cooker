# rice-cooker: Nix-first architecture, with niri as a first-class compositor

Companion to `nix-recon-digest.md`, which holds the verified reconnaissance. This
document describes **what the code does now**. Anything not yet built is in
§9 and labelled as such.

Two corrections to earlier revisions of this file, both disproven by
measurement rather than by argument:

- **Home Manager specialisations are not the activation mechanism.** The whole
  `--specialisation` design is gone. Details in §3.
- **`layer_namespaces` is not `^quickshell:`.** The live shell declares
  `caelestia-*`. Details in §6.

---

## 1. Two modes, and why they are not the same thing

`rice-cooker`'s workflow is `preview` and `install`, with `revert`/`uninstall` as
undo. On Nix those necessarily diverge, because a tool cannot mutate a
declarative system.

| | Arch | Nix |
| --- | --- | --- |
| `preview` | clone → `paru -S` deps → symlink → launch → verify | **build the flake → launch from the store → verify** |
| `install` | the above, recorded, persistent | **emit adoptable configuration**; nothing is mutated |
| `install_supported` | `install_deps` is non-empty (PR #16) | a `[nix]` block exists |
| undo | remove symlink, `pacman -Rns`, replay argv | replay argv (preview); delete the emitted file (install) |

**Choosing a compositor is rebuild-scoped; choosing a shell is not.** That is the
line the design draws. `programs.rice-cooker.compositor` belongs in Nix precisely
because you do not hot-swap a window manager, whereas `shell` must stay
rebuild-free — which is why install *emits* rather than applies.

## 2. What is proven, and how

Everything below was measured on boreal (NixOS, niri 26.04, graphical session
owned by user `gamer`), not inferred.

| Claim | Evidence |
| --- | --- |
| A Nix-packaged shell launches from a store path with **no symlink** | `-p` resolved to `$out/share/caelestia-shell/shell.qml`; `Configuration Loaded` |
| The niri adaptation works | `NiriService: niri found, starting event stream`, then workspaces `{"1":…,"2":…}`, focused window, outputs `[HDMI-A-1]` |
| Layer namespaces | `caelestia-background`, `caelestia-drawers`, 4× `caelestia-border-exclusion` |
| Eviction is mandatory | `Could not register notification server … already registered` while noctalia ran |
| The build is cheap | 8 derivations, 719 MiB fetched; the GUI adds only `electron_42` |

## 3. Nix preview: build, launch, verify

Activation **never touches Home Manager**, so it needs no standalone profile, no
`home-manager` CLI, and no system rebuild. Every risk the earlier
specialisation-based plan carried — two HM instances, duplicate imports, stale
locks, GC'd generations, read-only store configs — is gone.

`backend/src/install/pipeline.rs` branches on `platform == Nix`:

| Stage | Nix implementation | Arch unchanged |
| --- | --- | --- |
| `deps` | `nix build --no-link --print-out-paths <flake>#<attr>` (`platform::build_store_path`) | `paru`/`yay` |
| `symlink` | **skipped** — a Nix shell is a wrapper carrying its own config path | symlink into `$XDG_CONFIG_HOME` |
| `record` | `InstallRecord.nix = { store_path, launch_argv, snippet_path }` | `pacman_diff` |
| `evict` / `kill` | `kill_quickshell_with([…])` matching the rice's own argv0 | same |
| `launch` | `LaunchSpec::Argv`, resolved as `<store>/bin/<bin>` | `quickshell -c <name>` |
| `verify` | `verify_argv` + `owns_layers` | `verify_by_name` + Hyprland pid |
| revert | `replay_original_shell` with the captured argv | same |

Three details that matter:

- **Step names `deps` / `launch` / `verify` are a UI contract.**
  `src/pages/pick-a-rice/PickARice.tsx` drives its progress from them.
- **A bare binary resolves against the store.** `argv = ["caelestia-shell"]`
  becomes `<store>/bin/caelestia-shell`, so the shell's *own wrapper* runs rather
  than whatever `quickshell` is on `PATH`. Checked as `<store>/bin/<name>` first;
  falls back untouched when absent, which is how `noctalia` still resolves.
- **Eviction must match the outgoing rice's argv0.** `caelestia-shell` is not
  `quickshell`, so the default matcher left a previously previewed shell running
  and two shells fought for the same surfaces.

`process::verify_argv` distinguishes three outcomes, in this order: gone
(`!alive`), log carries `Failed to load configuration`, or the layer snapshot
shows one of our surfaces since the baseline. The last one is what proves the QML
actually evaluated, because the namespace is built at runtime as
`caelestia-${name}` — matching its prefix cannot be faked by a process that merely
started.

## 4. The install contract

The flake ships the module; the user writes a few lines and imports the rice
module themselves.

```nix
imports = [
  rice-cooker.homeManagerModules.default
  inputs.niri-caelestia.homeManagerModules.default
];

programs.rice-cooker = {
  enable = true;
  shell = "niri-caelestia";
  rices.niri-caelestia = inputs.niri-caelestia;
};
```

`nix/hm-module.nix` **selects, launches and asserts**. It does not import the
rice's module, because a module cannot choose its imports from configuration
values — that is either invalid or infinite recursion. It verifies the import
instead, via `options` and the catalog's `nix.hm_option`
(`programs.caelestia.enable`), and its assertion message states the remedy.

Existence checks read `options`, never `config`. Deciding what to define by
reading what is defined is the same trap.

`enable` forces the shell by registering `rice-cooker-shell.service` with
`ExecStart` under `mkForce`, and reports the conflict as a *warning* when another
known shell is also enabled — the user's own configuration owns its startup
items, so the module cannot delete them.

`install` on Nix writes `$XDG_DATA_HOME/rice-cooker/install-snippets/<rice>.nix`
and prints the same lines. The snippet includes the `imports` line *and* the
`rices.<name>` line, because without them it would fail the module's own
assertion.

## 5. Catalog v2

`[_catalog].schema = 2`, and every added field is defaulted, so the eight v1
entries parse unchanged with `compositors` defaulting to `["hyprland"]` — exactly
the v1 assumption.

There is **no `shape` field**: it conflated two independent questions and made
`qs -p <dir>` look like a general answer when it only covers the quickshell
family. Three orthogonal declarations replace it.

| Axis | Field | noctalia | niri-caelestia | dms |
|---|---|---|---|---|
| Runnable artifact to build | `build = "default"` | yes (C++) | yes | no — it *is* quickshell config |
| Home Manager module | `module = "homeModules.default"` | yes | yes | no |
| Configuration files | `symlink_src` / `symlink_dst` | yes | no | yes |

```toml
[niri-caelestia]
repo = "https://github.com/jutraim/niri-caelestia-shell"
commit = "fe36491a77c56ac51d6cad1c6fc05f9828fad837"   # 40-hex or a ref; never a short SHA
compositors = ["niri"]
layer_namespaces = ["^caelestia-"]

[niri-caelestia.launch]
kind = "argv"
argv = ["caelestia-shell"]

[niri-caelestia.nix]
build  = "default"                     # a runnable artifact exists
module = "homeModules.default"         # and a Home Manager module
hm_option = "programs.caelestia.enable"
flake = "github:jutraim/niri-caelestia-shell/fe36491a…"
```

`preview` is a **fourth, separate** declaration, because how a rice installs
says nothing about whether it can run: `package` (build it, run
`<store>/bin/<bin>`), `quickshell-source` (fetch the tree, run
`quickshell -p <tree>/<symlink_src>`), or `unsupported` (configuration only, so a
rebuild is the only way). It is derived when unambiguous — a `build` outranks
dotfiles, since a compiled shell cannot run from a tree — and must be stated when
it is not. A test refuses a bundled Nix rice whose preview would be *implicitly*
unsupported, so the limitation is declared rather than inferred.

`quickshell-source` needs `quickshell` on `PATH`: the rice is configuration, and
the shell it configures belongs to the system. Preflight says so plainly instead
of failing opaquely.

Two parse modes: `Catalog::parse` is strict and gates CI and the bundled catalog;
`parse_lenient` skips one bad entry so a runtime-fetched catalog cannot be broken
by a field this binary does not know. This is needed because
`#[serde(flatten)]` makes the top-level table *the entry map*, so `_catalog` has
to be lifted out before deserialising.

Validation is the only channel from catalog data into generated Nix, so it is
strict: attr paths are dotted `[A-Za-z0-9_-]` segments, flake refs reject
`"`, `'`, `;`, `$`, `` ` `` and newlines, and a short SHA is refused for Nix
entries because it resolves non-deterministically as the log grows.

## 6. Compositor seam

`backend/src/compositor/{mod,hyprland,niri}.rs`:

```rust
pub enum Compositor { Hyprland { signature: String }, Niri { socket: PathBuf } }
impl Compositor {
    pub fn detect(env: &SessionEnv) -> Result<Self>;
    pub fn layers(&self) -> Option<Vec<LayerSurface>>;   // None = IPC failed
}
pub fn owns_layers(snapshot: &[LayerSurface], o: &Ownership<'_>) -> bool;
```

| | Hyprland | niri |
| --- | --- | --- |
| detection | `HYPRLAND_INSTANCE_SIGNATURE` + the instance socket existing | `$NIRI_SOCKET`, else `niri.<WAYLAND_DISPLAY>.<pid>.sock` reconstructed from `/proc` |
| layer query | `hyprctl layers -j` | `"Layers"` over the UNIX socket |
| ownership evidence | `pid` per surface | **namespace only — niri reports no pid** |

- Socket filename confirmed in niri's `IpcServer::start` and observed live as
  `/run/user/1002/niri.wayland-1.2565.sock`.
- `$NIRI_SOCKET` is unreliable (niri#2149), so a stale value falls back to
  reconstruction, and a niri session with no reachable socket produces an
  actionable error rather than "unsupported compositor".
- Requests go over the socket directly instead of via `niri msg`, and are parsed
  through `serde_json::Value`, so a *newer* compositor cannot break a working
  desktop with an unknown field.
- `layers()` returns `Option` because a failed IPC query is not evidence of zero
  surfaces; collapsing the two would make a wedged compositor look like a failed
  launch.
- Ownership is *appeared since the baseline* **and** (*pid or namespace*). The
  baseline is taken after eviction and before launch, and the diff is a
  **multiset** on `(namespace, output, layer)` — the four identical
  `caelestia-border-exclusion` surfaces are why a set diff would be wrong.

## 7. Shell identity

The single most damaging bug for this desktop: the running shell is `noctalia`,
and every lookup matched `quickshell|qs` only, so the original shell was never
captured, evicted or replayed. `pkill -x` was also structurally wrong — it
matches `comm`, truncated to 15 characters, so `.quickshell-wrapped` and
store-path argv0 binaries never matched.

`ShellMatcher` plus `normalize_shell_name` (basename, strip a leading `.`, strip
`-wrapped`) now cover `quickshell`, `qs`, `noctalia-qs`, `noctalia`, and any
catalog launch binary, matched against argv0 *or* the resolved executable. Pids
come from procfs and are signalled by pid, with `proc_root` injectable for tests.

## 8. Verifying that a shell is compatible with a compositor

Four tiers, weakest to strongest:

- **T0 — declaration.** `compositors` in the catalog. Free, instant, and the only
  tier that can lie: ChromaShell-Flake has no compositor option at all and is
  Hyprland-only, so its entry is pure assertion.
- **T1 — eval-time assertion.** Because `compositor` is known at evaluation time,
  the module can refuse an incompatible pair at build time. Does not verify the
  declaration; prevents selecting a bad pair. *(In progress.)*
- **T2 — static evidence over the built artifact.** Scan the store path for
  bindings: Hyprland → `Quickshell.Hyprland`, `Hyprland.`, `hyprctl`; niri →
  `niri msg`, `NIRI_SOCKET`, `Quickshell.Niri`. Measured separation:
  jutraim's `services/Niri.qml` has 26 `niri msg` calls; upstream Caelestia has
  zero niri references and 13 qml files touching `Quickshell.Hyprland`. Verdicts:
  `supported` / `neutral` / `contradicts` / `mixed`. **Built** as
  `rice-cooker-backend compat <name>`; with no `--dir` it fetches or builds the
  rice exactly as a preview would, and it scopes the scan to `<artifact>/<symlink_src>`
  so Hyprland-side theming elsewhere in a dotfiles repo is not counted as part of
  the rice.

**Correction to an earlier claim in this file.** It said upstream quickshell has no
niri layer-shell support. That is false: quickshell has zero niri-specific code,
but its layer surfaces go through `zwlr_layer_shell_v1`
(`src/wayland/wlr_layershell/`), which is compositor-agnostic and which niri
implements. So quickshell **renders** on niri. What it lacks is niri-specific
*bindings* — workspaces, windows, IPC — which rices work around by shelling out to
`niri msg`. The question for a rice is therefore "does its configuration import a
compositor-specific API", which is what this probe answers, not "does the
compositor support layer shells".

Measured with the probe:

| rice | files with a Hyprland binding / files scanned | declared |
|---|---|---|
| zephyr | 15 / 25 | hyprland |
| linux-retroism | 1 / 17 (`taskbar/Workspaces.qml`) | hyprland |
| whisker | 18 / 154 | hyprland |
| nandoroid | 50 / 334 | hyprland |

All four are Hyprland-bound, so none declares niri. **Measure the artifact the
catalog pins, not the repository's default branch**: zephyr's `quickshell/`
directory does not exist at HEAD at all (the repo was restructured), so a scan of
HEAD reports zero bindings for a tree that has 51 files at the pinned revision.
That mistake was made and caught here — by the probe, which is the argument for it
being a tool rather than a judgement call.
- **T3 — runtime observation.** What `verify_argv` does today: a matching layer
  surface since the baseline, which cannot be produced without real integration.
- **T4 — record the observation.** Persist which compositor a rice was seen
  working on, against the exact rev. *Not built.*

The honest limit of T2–T4: they verify the shell **runs and renders**. They do not
verify that compositor-side *features* the shell wants exist — niri `layer-rule`s
for a backdrop, Hyprland special workspaces. That is a capability gap, not a
compatibility failure, and has to be declared.

## 9. Known gaps

1. **The module has no test.** `nix flake check` warns `unknown flake output
   'homeManagerModules'` and skips it entirely, so a broken option type or an
   assertion that always fails would still report success. A
   `checks.<system>.hm-module` using `lib.evalModules` closes this.
2. **The Nix preview path has never run end to end.** Every stage is proven
   individually (build, launch, namespaces, IPC); the sequence
   build → evict → launch → verify → revert is not.
3. **`replay_original_shell` on niri is untested**, and it is the only thing
   standing between a failed preview and a user with no desktop shell.
4. **T2 and T4 are unwritten** (§8).
5. **The remote catalog is unwritten** (`catalog update` + a `catalog/v2` branch).
   It matters for safety: a fetched catalog evaluates arbitrary flakes as the
   user with no release review in between, so it needs HTTPS only, 40-hex revs,
   and a consent prompt for rices outside the bundled set.
6. **`nix run` has not been visually confirmed.** The GUI is built and its wrapper
   environment is verified; appearing on screen needs the session owner.

## 10. Testing it

```sh
# Read-only, safe: which platform and compositor were detected, and why not
nix run --refresh 'github:Avanonyme/rice-cooker?ref=feat/nix-rices#backend' -- env
nix run --refresh 'github:Avanonyme/rice-cooker?ref=feat/nix-rices#backend' -- list

# Preview: KILLS the running shell (noctalia owns the notification server)
nix run --refresh 'github:Avanonyme/rice-cooker?ref=feat/nix-rices#backend' -- preview niri-caelestia

# Recovery if revert fails
noctalia &

# The GUI
nix run --refresh github:Avanonyme/rice-cooker?ref=feat/nix-rices
```

`--refresh` is not optional: nix caches branch→rev resolution and will silently
rebuild the previous source. The symptom is an unchanged derivation hash.

Logs: `$XDG_CACHE_HOME/rice-cooker/last-run.log` and `last-run.ndjson`.

`nix flake check` gates the Rust suite and `npm run typecheck`
(`checks.<system>.backend` and `checks.<system>.typecheck`), and it caught two
bugs that macOS hid: an unparseable `RICE_COOKER_PLATFORM` silently resolving to
Arch in a sandbox without `nix` on `PATH`, and `read_dir` order making
`matching_pids` return a different sequence per filesystem.
