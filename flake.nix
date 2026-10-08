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
      catalog = builtins.fromTOML (builtins.readFile ./backend/catalog.toml);
      riceNames = builtins.filter (n: n != "_catalog") (builtins.attrNames catalog);

      perSystem =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;

          # Electron's major is pinned in package.json; prefer the matching
          # nixpkgs attribute but do not fail if nixpkgs has moved on.
          electron = lib.attrByPath [ "electron_42" ] pkgs.electron pkgs;
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
            nativeBuildInputs = [ pkgs.git ];
            meta = {
              description = "rice-cooker install engine";
              license = lib.licenses.bsd3;
              mainProgram = "rice-cooker-backend";
            };
          };

          # The Electron app. `npm run build` is `electron-vite build`, which
          # bundles the renderer and externalises nothing the main process
          # actually imports, so no node_modules are needed at runtime.
          gui = pkgs.buildNpmPackage {
            pname = "rice-cooker";
            version = "0.1.0";
            src = lib.fileset.toSource {
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
                ./packaging
              ];
            };
            npmDepsHash = "sha256-n01+FU/ElBgW/4Az4QhTccu55/CsIbBGeBTGZcf4PC4=";
            # The npm `electron` package downloads a prebuilt binary in a
            # postinstall script; the nixpkgs electron above is used instead.
            ELECTRON_SKIP_BINARY_DOWNLOAD = "1";
            nativeBuildInputs = [ pkgs.makeWrapper ];

            installPhase = ''
              runHook preInstall

              mkdir -p $out/share/rice-cooker
              cp -r out package.json $out/share/rice-cooker/
              install -Dm644 backend/catalog.toml $out/share/rice-cooker/catalog.toml

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
          };

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

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${perSystem system.gui}/bin/rice-cooker";
          meta.description = "rice-cooker";
        };
      });

      # Portable install contract: no framework-specific configuration shape.
      # `enable` is what forces a shell over whatever the user's own config does.
      homeManagerModules = {
        default = import ./nix/hm-module.nix {
          inherit self catalog riceNames;
        };
        rice-cooker = self.homeManagerModules.default;
      };

      # Exposed so downstream configs (and the backend) can read the same list.
      lib = {
        inherit catalog riceNames;
        catalogPath = ./backend/catalog.toml;
      };
    };
}
