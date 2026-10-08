# Home Manager module: `programs.rice-cooker`.
#
# Deliberately framework-agnostic. Nothing here depends on how a configuration is
# organised, so it works in plain Home Manager, as a NixOS module, or under Hjem.
#
# This module **selects, launches and asserts**. It does not import the rice's own
# module: a module cannot choose its imports from configuration values, so doing
# that from here is either invalid or infinitely recursive. The user imports it,
# and this module verifies they did — via `options`, not `config`, because
# deciding what to define by reading what is defined is the same trap.
#
#   imports = [
#     rice-cooker.homeManagerModules.default
#     inputs.niri-caelestia.homeManagerModules.default
#   ];
#
#   programs.rice-cooker = {
#     enable = true;
#     shell = "niri-caelestia";
#     rices.niri-caelestia = inputs.niri-caelestia;
#   };
#
# `enable` is what makes the selection win over whatever the user's own
# configuration already starts: unless the other shell is disabled, the
# launcher is forced with `mkForce` and the conflict is reported as a warning.
{
  self,
  catalog,
  riceNames,
  compositorIds,
}:
{
  config,
  lib,
  pkgs,
  options,
  ...
}:
let
  inherit (lib) mkEnableOption mkIf mkOption types literalExpression;

  cfg = config.programs.rice-cooker;
  system = pkgs.stdenv.hostPlatform.system;

  entry = if cfg.shell == null then null else catalog.${cfg.shell};

  # Compositors the selected rice declares. Mirrors the backend's default so a
  # v1 entry that omits `compositors` counts as Hyprland-only.
  riceCompositors =
    if entry == null then [ ]
    else entry.compositors or [ "hyprland" ];

  # Case-insensitive compositor id. `check` runs on each definition before
  # `merge`, so it must accept the raw value ("Niri"); lowercasing happens in
  # `merge`, which is why a bare `apply = lib.toLower` would not work here.
  compositorType = lib.mkOptionType {
    name = "compositor";
    description = "one of ${lib.concatStringsSep ", " compositorIds} (case-insensitive), or null";
    check = value:
      value == null
      || (lib.isString value && lib.elem (lib.toLower value) compositorIds);
    merge = loc: defs:
      let
        lowered = map (def: def // {
          value = if def.value == null then null else lib.toLower def.value;
        }) defs;
      in
        lib.mergeOneOption loc lowered;
  };

  # The flake input the user supplied for the selected rice. Rice inputs are not
  # fetched here: `builtins.getFlake` needs impure evaluation, so the caller
  # supplies them and this module wires them up.
  riceInput =
    if cfg.shell != null && builtins.hasAttr cfg.shell cfg.rices then
      cfg.rices.${cfg.shell}
    else
      null;

  # `[nix] package = "default"` picks the attribute under `packages.<system>`.
  ricePackage =
    if riceInput != null && riceInput ? packages then
      riceInput.packages.${system}.${lib.attrByPath [ "nix" "package" ] "default" entry}
    else
      null;

  # The rice's own enable option, when the catalog names one. Used to verify the
  # user imported the rice's module and switched it on.
  hmOption = if entry == null then null else lib.attrByPath [ "nix" "hm_option" ] null entry;
  hmOptionPath = if hmOption == null then null else lib.splitString "." hmOption;
  hasRiceOption = hmOptionPath != null && lib.hasAttrByPath hmOptionPath options;
  riceOptionEnabled = hasRiceOption && lib.getAttrFromPath hmOptionPath config;

  # Mirrors the backend's argv derivation: a bare binary name resolves against
  # the rice's own package, so a shell's wrapper (which carries its own config
  # path) is used instead of whatever happens to be on PATH.
  catalogLaunch =
    if entry != null && entry ? launch then
      if entry.launch.kind == "argv" then
        entry.launch.argv
      else
        [
          (entry.launch.bin or "quickshell")
          "-c"
          (entry.launch.config or cfg.shell)
        ]
    else
      [
        "quickshell"
        "-c"
        (toString cfg.shell)
      ];

  resolvedLaunch =
    if catalogLaunch == [ ] then
      catalogLaunch
    else if ricePackage != null && !(lib.hasInfix "/" (builtins.head catalogLaunch)) then
      [ "${ricePackage}/bin/${builtins.head catalogLaunch}" ] ++ builtins.tail catalogLaunch
    else
      catalogLaunch;

  launchCommand = if cfg.launchCommand != [ ] then cfg.launchCommand else resolvedLaunch;

  # Other shells this module knows how to recognise. The user's own
  # configuration owns its startup items, so `enable` can force a launcher but
  # cannot delete someone else's; it reports the conflict instead of pretending.
  competingShells = lib.filter (path: lib.hasAttrByPath path options && lib.getAttrFromPath path config) [
    [ "programs" "noctalia" "enable" ]
    [ "programs" "caelestia" "enable" ]
    [ "programs" "chromashell" "enable" ]
  ];
in
{
  options.programs.rice-cooker = {
    enable = mkEnableOption "rice-cooker, and the desktop shell it manages";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.gui;
      defaultText = literalExpression "rice-cooker.packages.\${system}.gui";
      description = "The rice-cooker application to install.";
    };

    shell = mkOption {
      type = types.nullOr (types.enum riceNames);
      default = null;
      example = "niri-caelestia";
      description = ''
        Which rice to run as this user's desktop shell. `null` installs the tool
        and forces nothing.

        The list comes from the catalog this flake ships, so it cannot drift from
        what the backend can actually activate. The rice's own Home Manager module
        must be imported separately; this module asserts that it was.
      '';
    };

    compositor = mkOption {
      type = compositorType;
      default = null;
      example = "niri";
      description = ''
        Which compositor this machine runs. `null` detects it at runtime, which
        is today's behaviour and keeps the default backwards compatible.

        Because the value is known at evaluation time, a compositor the selected
        rice does not support becomes a build-time assertion instead of a
        runtime failure. Accepted values come from the shipped catalog.
      '';
    };

    rices = mkOption {
      type = types.attrsOf types.anything;
      default = { };
      example = literalExpression ''
        {
          niri-caelestia = inputs.niri-caelestia;
        }
      '';
      description = ''
        Flake inputs for the rices you want to select, keyed by catalog name.
        Required for the selected rice so its launcher can be resolved to a store
        path rather than to whatever is on `PATH`.
      '';
    };

    launchCommand = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "caelestia-shell" ];
      description = ''
        Command that starts the selected shell. Defaults to the catalog's
        `[launch]` entry, resolved against the rice's package when it has one.
      '';
    };

    systemd = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Register the selected shell as a user service.";
      };

      target = mkOption {
        type = types.str;
        default = "graphical-session.target";
        description = "Systemd target that starts the selected shell.";
      };
    };
  };

  config = mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package ];

      assertions = [
        {
          # A named shell with no input would install the tool and resolve the
          # launcher to a PATH lookup that probably fails.
          assertion = cfg.shell == null || riceInput != null;
          message = ''
            programs.rice-cooker: shell = "${toString cfg.shell}" needs its flake
            input, and none was provided. Add it to your flake and list it:

              programs.rice-cooker.rices.${toString cfg.shell} = inputs.${toString cfg.shell};

            rice-cooker does not fetch rice flakes itself: evaluating a flake
            reference at build time requires impure evaluation.
          '';
        }
        {
          # `enable = true` next to a rice whose module was never imported would
          # start a shell that nothing configured.
          assertion = cfg.shell == null || hmOptionPath == null || riceOptionEnabled;
          message = ''
            programs.rice-cooker: shell = "${toString cfg.shell}" is forced, but
            ${toString hmOption} is not enabled, so the rice's own module was
            never imported or never switched on. Add both:

              imports = [ inputs.${toString cfg.shell}.homeManagerModules.default ];
              ${toString hmOption} = true;

            A module cannot choose its imports from configuration values, so this
            has to be done by you rather than by programs.rice-cooker.
          '';
        }
        {
          # A compositor the rice does not declare is a build-time mismatch, not
          # a runtime gamble. With `shell = null` (tool only) there is no rice to
          # contradict, so nothing is asserted.
          assertion = cfg.shell == null || cfg.compositor == null || lib.elem cfg.compositor riceCompositors;
          message = ''
            programs.rice-cooker: shell = "${toString cfg.shell}" declares
            compositors [${lib.concatStringsSep ", " (map (c: "\"${c}\"") riceCompositors)}] but
            compositor = "${cfg.compositor}" was requested. Pick one the rice
            supports, or leave `compositor = null` to detect it at runtime.
          '';
        }
      ];

      warnings =
        lib.optional (cfg.shell != null && competingShells != [ ])
          "programs.rice-cooker: shell = \"${toString cfg.shell}\" is forced, but "
          + (lib.concatStringsSep ", " (map (p: lib.concatStringsSep "." p) competingShells))
          + " is also enabled. Both will try to start a desktop shell; "
          + "disable the other one or its launcher will race this one.";
    }

    (mkIf (cfg.compositor != null) {
      # Make the runtime agree with the declaration. The backend reads this as an
      # explicit override, so the module and the install engine cannot disagree.
      home.sessionVariables.RICE_COOKER_COMPOSITOR = cfg.compositor;
    })

    (mkIf (cfg.shell != null && riceInput != null) {
      home.packages = lib.optional (ricePackage != null) ricePackage;

      systemd.user.services = mkIf cfg.systemd.enable {
        rice-cooker-shell = {
          Unit = {
            Description = "rice-cooker: ${cfg.shell} desktop shell";
            PartOf = [ cfg.systemd.target ];
          };

          Service = {
            Type = "exec";
            # mkForce: this is the whole point of `enable`. A rice whose module
            # also registers a unit, or a user definition added later, must not
            # be able to quietly replace the launcher.
            ExecStart = lib.mkForce (lib.escapeShellArgs launchCommand);
            Restart = "on-failure";
            RestartSec = "5s";
            TimeoutStopSec = "5s";
          };

          Install.WantedBy = [ cfg.systemd.target ];
        };
      };
    })
  ]);
}
