{
  description = "DAFS — distributed AI-native filesystem";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Read version and name from Cargo.toml rather than duplicating them.
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      in
      {
        packages = rec {
          default = dafs;

          dafs = pkgs.rustPlatform.buildRustPackage {
            pname = "dafs";
            version = cargoToml.workspace.package.version;

            # Filtered rather than `self` so an unrelated doc or CI edit does not
            # invalidate the derivation.
            #
            # Only `ui/dist` is included, not all of `ui/`: dafs-api embeds
            # dist/index.html with include_str!, and pulling in the frontend
            # sources, package.json, or a stray node_modules would invalidate
            # the derivation on edits that cannot affect the built output.
            #
            # `scripts` is included because dafs-daemon embeds
            # scripts/install.sh the same way, for `dafs self-update`.
            #
            # This is also why no node toolchain appears anywhere in this flake.
            # The bundle is committed (see .gitignore and the ui-bundle CI job),
            # so building the daemon needs nothing but Rust.
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./crates
                ./ui/dist
                ./scripts
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;

            # rusqlite's `bundled` feature compiles SQLite from vendored C, so
            # a C toolchain is needed but no system sqlite is. Bundling is
            # deliberate: it pins the exact SQLite version the store's pragmas
            # and STRICT tables were tested against, rather than inheriting
            # whatever the host distribution ships.
            nativeBuildInputs = [ pkgs.pkg-config ];

            # jemalloc is built from source by tikv-jemalloc-sys.
            buildInputs = [ ];

            buildAndTestSubdir = null;
            # dafs-pdf-worker is not a Cargo dependency of dafs-daemon — it is
            # spawned as a sibling binary at runtime (crates/dafs-daemon/src/
            # extract_worker.rs resolves it next to its own executable), so
            # building only "-p dafs-daemon" would silently ship a daemon that
            # can start but can never extract a PDF. Both binaries land in the
            # same $out/bin, which is exactly the layout that resolution
            # expects.
            cargoBuildFlags = [ "-p" "dafs-daemon" "-p" "dafs-pdf-worker" ];

            # The memtest crate spawns the release binary, which the Nix build
            # sandbox has no network access to bind a socket for reliably, and
            # `cargo test` here would run against the debug profile anyway. The
            # ceiling assertions are a CI responsibility (see
            # .github/workflows/ci.yml) rather than a build-time one.
            doCheck = false;

            meta = with pkgs.lib; {
              description = "Distributed AI-native filesystem daemon";
              homepage = "https://github.com/puredevotion/dafs";
              license = licenses.mit;
              mainProgram = "dafs";
              platforms = platforms.linux;
            };
          };

          # OCI image, for deployers that consume this as a container rather
          # than a Nix package. streamLayeredImage rather than buildImage: it
          # writes the tarball to stdout instead of into the store, so the
          # image never occupies store space twice.
          docker = pkgs.dockerTools.streamLayeredImage {
            name = "dafs";
            tag = cargoToml.workspace.package.version;
            contents = [ dafs pkgs.cacert ];
            config = {
              Entrypoint = [ "${dafs}/bin/dafs" ];
              # Bind all interfaces inside a container: the loopback default is
              # right for a host process but would make the container
              # unreachable. Exposing it is then the orchestrator's decision.
              Env = [ "DAFS_LISTEN=0.0.0.0:7878" "DAFS_DATA_DIR=/data" ];
              ExposedPorts."7878/tcp" = { };
              Volumes."/data" = { };
              User = "1000:1000";
            };
          };
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            cargo-audit
            cargo-deny
            cargo-llvm-cov
            pkg-config
          ];
          # cargo-fuzz needs nightly, so it is deliberately absent here rather
          # than pulling a second toolchain into every dev shell.
          shellHook = ''
            echo "dafs dev shell — cargo $(cargo --version | cut -d' ' -f2)"
            echo "fuzzing needs nightly: cargo +nightly fuzz run migrations"
            if [ -d .git ]; then
              ./scripts/install-hooks.sh
            fi
          '';
        };

        checks = {
          inherit (self.packages.${system}) dafs;
        };
      });
}
