{
  description = "rice-cooker: browse, preview and install desktop-shell rices";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      home-manager,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      # The catalog doubles as the list of selectable shells, so the module's
      # `shell` enum can never drift from what the backend actually knows.
      inherit (nixpkgs) lib;

      catalog = builtins.fromTOML (builtins.readFile ./backend/catalog.toml);
      riceNames = builtins.filter (n: n != "_catalog") (builtins.attrNames catalog);

      # Union of every entry's `compositors`, with the same Hyprland-only
      # default the backend applies to v1 entries that omit the key. Drives the
      # module's `compositor` option so its accepted values cannot drift from
      # what the backend can actually activate.
      compositorIds = nixpkgs.lib.unique (
        nixpkgs.lib.concatMap (name: catalog.${name}.compositors or [ "hyprland" ]) riceNames
      );

      # `nix flake check` skips `homeManagerModules`, because it is not a standard
      # flake output. A module that cannot even evaluate therefore still reports
      # "all checks passed" — which is what happened: a `lib.optional` precedence
      # bug made `warnings` throw unconditionally and nothing noticed for two
      # commits. Evaluating the module here makes it a real gate.
      hmCase =
        system:
        { shell, compositor }:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          # Stands in for the rice's own module, so the `nix.hm_option`
          # assertion can be satisfied without pulling the rice flake in.
          riceStub = { lib, ... }: {
            options.programs.caelestia.enable = lib.mkEnableOption "caelestia";
          };
        in
        (home-manager.lib.homeManagerConfiguration {
          inherit pkgs;
          modules = [
            self.homeManagerModules.default
            riceStub
            {
              home.username = "tester";
              home.homeDirectory = "/home/tester";
              home.stateVersion = "25.11";
            }
            {
              programs.rice-cooker = {
                enable = true;
                rices.niri-caelestia = { };
                inherit shell compositor;
              };
            }
            { programs.caelestia.enable = true; }
          ];
        }).config;

      # Force `warnings` and `assertions` as well as the values: those are the
      # options the module writes messages into, and the ones a precedence slip
      # breaks silently.
      hmForced =
        system:
        { shell, compositor }:
        let
          cfg = hmCase system { inherit shell compositor; };
        in
        builtins.deepSeq [ cfg.warnings cfg.assertions ] cfg;

      hmModuleExpected = {
        normalizesCase = "niri";
        sessionVarAgrees = "niri";
        nullStaysNull = null;
        rejectsUnknownCompositor = true;
        rejectsIncompatibleRice = true;
        launchesTheShell = true;
      };

      hmModuleResults =
        system:
        let
          rejected = shell: compositor:
            !(builtins.tryEval (
              builtins.deepSeq
                (hmForced system { inherit shell compositor; }).programs.rice-cooker.compositor
                true
            )).success;
          accepted = hmForced system {
            shell = "niri-caelestia";
            compositor = "Niri";
          };
          unset = hmForced system {
            shell = "niri-caelestia";
            compositor = null;
          };
        in
        {
          normalizesCase = accepted.programs.rice-cooker.compositor;
          sessionVarAgrees = accepted.home.sessionVariables.RICE_COOKER_COMPOSITOR;
          nullStaysNull = unset.programs.rice-cooker.compositor;
          rejectsUnknownCompositor = rejected "niri-caelestia" "sway";
          rejectsIncompatibleRice = rejected "niri-caelestia" "hyprland";
          # Home Manager normalises `ExecStart` to a list, so accept either shape.
          launchesTheShell =
            let
              execStart = accepted.systemd.user.services.rice-cooker-shell.Service.ExecStart;
              parts = if builtins.isList execStart then execStart else [ execStart ];
            in
            lib.any (part: lib.hasInfix "caelestia-shell" part) parts;
        };

      perSystem =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;

          # Electron's major is pinned in package.json; prefer the matching
          # nixpkgs attribute but do not fail if nixpkgs has moved on.
          electron = lib.attrByPath [ "electron_42" ] pkgs.electron pkgs;

          # Everything the npm build and the type check need. `backend/catalog.toml`
          # is deliberately not here: it is interpolated as its own path below,
          # so the GUI build does not depend on the whole backend tree.
          npmSrc = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./package.json
              ./package-lock.json
              ./index.html
              ./electron.vite.config.ts
              ./tsconfig.json
              ./tsconfig.node.json
              ./tsconfig.web.json
              ./electron
              ./src
              # installPhase installs the icon and desktop entry from here.
              ./packaging
            ];
          };

          npmDepsHash = "sha256-n01+FU/ElBgW/4Az4QhTccu55/CsIbBGeBTGZcf4PC4=";

          # The npm `electron` package downloads a prebuilt binary in a
          # postinstall script; the nixpkgs electron above is used instead.
          npmEnv = {
            src = npmSrc;
            inherit npmDepsHash;
            ELECTRON_SKIP_BINARY_DOWNLOAD = "1";
          };
        in
        rec {
          # The install engine. `doCheck` is on: the unit suite covers the
          # catalog validator, the compositor seam and the Nix launch-argv
          # derivation, none of which need a live session.
          backend = pkgs.rustPlatform.buildRustPackage {
            pname = "rice-cooker-backend";
            version = "0.1.0";
            src = ./backend;
            cargoLock.lockFile = ./backend/Cargo.lock;
            # `git` is required by the pipeline's preflight and by its tests.
            nativeBuildInputs = [
              pkgs.git
              pkgs.makeWrapper
            ];
            # Ship the catalog and add it to XDG_DATA_DIRS, which is the lookup
            # `Paths::find_catalog` already implements. Without this the CLI has
            # no catalog at all: `nix run .#backend` failed with "no catalog
            # found" while the GUI worked, because only the GUI wrapper set
            # RICE_COOKER_CATALOG.
            postInstall = ''
              install -Dm644 ${./backend/catalog.toml} $out/share/rice-cooker/catalog.toml
              wrapProgram $out/bin/rice-cooker-backend \
                --prefix XDG_DATA_DIRS : $out/share
            '';
            meta = {
              description = "rice-cooker install engine";
              license = lib.licenses.bsd3;
              mainProgram = "rice-cooker-backend";
            };
          };

          # `electron-vite build` bundles the renderer and externalises nothing
          # the main process imports, so no node_modules are needed at runtime.
          gui = pkgs.buildNpmPackage (
            npmEnv
            // {
              pname = "rice-cooker";
              version = "0.1.0";
              nativeBuildInputs = [ pkgs.makeWrapper ];

              installPhase = ''
                runHook preInstall

                mkdir -p $out/share/rice-cooker
                cp -r out package.json $out/share/rice-cooker/
                install -Dm644 ${./backend/catalog.toml} $out/share/rice-cooker/catalog.toml

                install -Dm644 packaging/icons/rice-cooker.png \
                  $out/share/icons/hicolor/512x512/apps/rice-cooker.png
                install -Dm644 packaging/icons/rice-cooker.svg \
                  $out/share/icons/hicolor/scalable/apps/rice-cooker.svg
                install -Dm644 packaging/aur/rice-cooker/rice-cooker.desktop \
                  $out/share/applications/rice-cooker.desktop

                mkdir -p $out/bin
                makeWrapper ${lib.getExe electron} $out/bin/rice-cooker \
                  --add-flags "$out/share/rice-cooker" \
                  --set-default RICE_COOKER_BACKEND ${backend}/bin/rice-cooker-backend \
                  --set-default RICE_COOKER_CATALOG $out/share/rice-cooker/catalog.toml \
                  --prefix PATH : ${lib.makeBinPath [ pkgs.git backend ]}

                runHook postInstall
              '';

              meta = {
                description = "A visual tool for ricing your desktop";
                license = lib.licenses.bsd3;
                mainProgram = "rice-cooker";
                platforms = lib.platforms.linux;
              };
            }
          );

          # `electron-vite build` does not type check, so this is a separate
          # derivation and therefore a real `nix flake check` gate.
          typecheck = pkgs.buildNpmPackage (
            npmEnv
            // {
              pname = "rice-cooker-typecheck";
              version = "0.1.0";
              npmBuildScript = "typecheck";
              installPhase = "mkdir -p $out";
              meta.description = "Type check the rice-cooker renderer and main process";
            }
          );

          # Fails `nix flake check` at evaluation time if the module cannot
          # evaluate or stops producing what it promises.
          hmModule =
            let
              got = hmModuleResults system;
            in
            if got == hmModuleExpected then
              pkgs.writeText "rice-cooker-hm-module-ok.json" (builtins.toJSON got)
            else
              throw ''
                rice-cooker: the Home Manager module check failed.
                got:      ${builtins.toJSON got}
                expected: ${builtins.toJSON hmModuleExpected}
              '';

          default = gui;
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          p = perSystem system;
        in
        {
          inherit (p) backend gui;
          default = p.gui;
        }
      );

      apps = forAllSystems (
        system:
        let
          p = perSystem system;
        in
        {
          default = {
            type = "app";
            program = "${p.gui}/bin/rice-cooker";
            meta.description = "rice-cooker";
          };
        }
      );

      # `nix flake check` builds everything listed here, so the backend's unit
      # suite (doCheck) and the renderer type check are both real gates. Without
      # this, check would only evaluate the packages.
      checks = forAllSystems (
        system:
        let
          p = perSystem system;
        in
        {
          inherit (p) backend typecheck hmModule;
        }
      );

      # Portable install contract: no framework-specific configuration shape.
      # `enable` is what forces a shell over whatever the user's own config does.
      homeManagerModules = {
        default = import ./nix/hm-module.nix {
          inherit self catalog riceNames compositorIds;
        };
        rice-cooker = self.homeManagerModules.default;
      };

      # Exposed so downstream configs (and the backend) can read the same list,
      # and so the module's behaviour can be inspected without building:
      #   nix eval .#lib.hmModuleResults --apply 'f: f "x86_64-linux"'
      lib = {
        inherit catalog riceNames compositorIds;
        catalogPath = ./backend/catalog.toml;
        hmModuleResults = hmModuleResults;
        hmModuleExpected = hmModuleExpected;
      };
    };
}
