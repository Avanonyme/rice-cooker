# Home Manager module: `programs.rice-cooker`.
#
# Deliberately framework-agnostic. Nothing here depends on how a configuration is
# organised, so it works in plain Home Manager, as a NixOS module, or under Hjem.
#
# Add this flake as an input, enable the program, and name the shell you want:
#
#   programs.rice-cooker = {
#     enable = true;
#     shell = "niri-caelestia";
#     rices.niri-caelestia = inputs.niri-caelestia;
#   };
#
# `enable` is what makes the selection win over whatever the user's own
# configuration already starts: the rice's module is imported, its package is
# installed, and its launcher is registered as a unit under `mkForce`.
{
  self,
  catalog,
  riceNames,
}:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkEnableOption mkIf mkOption types literalExpression;

  cfg = config.programs.rice-cooker;
  system = pkgs.stdenv.hostPlatform.system;

  entry = if cfg.shell == null then null else catalog.${cfg.shell};

  # The flake input the user supplied for the selected rice, if any. Rice inputs
  # are not fetched here: `builtins.getFlake` needs impure evaluation, so the
  # caller supplies them and this module wires them up.
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
  # configuration owns its startup items, so `enable` can import and force a
  # launcher but cannot delete someone else's; it reports instead of pretending.
  competingShells = lib.filter (path: lib.hasAttrByPath path config && lib.getAttrFromPath path config) [
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
        what the backend can actually activate.
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
        Flake inputs for the rices you want to select, keyed by catalog name. A
        rice becomes selectable only once its input is provided.
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
          # A named shell with no input would silently install the tool and do
          # nothing, which is the worst possible outcome for `enable = true`.
          assertion = cfg.shell == null || riceInput != null;
          message = ''
            programs.rice-cooker: shell = "${toString cfg.shell}" needs its flake
            input, and none was provided. Add it to your flake and list it:

              programs.rice-cooker.rices.${toString cfg.shell} = inputs.${toString cfg.shell};

            rice-cooker does not fetch rice flakes itself: evaluating a flake
            reference at build time requires impure evaluation.
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

    (mkIf (cfg.shell != null && riceInput != null) {
      imports = lib.optional (riceInput ? homeManagerModules) riceInput.homeManagerModules.default;

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
