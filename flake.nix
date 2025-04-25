{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";

    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    systems.url = "github:nix-systems/default";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane = {
      url = "github:ipetkov/crane";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs = { flake-parts, ... } @ inputs:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.devshell.flakeModule inputs.treefmt-nix.flakeModule ];
      systems = import inputs.systems;
      perSystem =
        { system
        , ...
        }:
        let
          # set up `pkgs` with rust-overlay
          overlays = [ (import inputs.rust-overlay) ];
          pkgs = (import inputs.nixpkgs) {
            inherit system overlays;
            config = {
              allowUnfree = true;
            };
          };

          #########
          # Rust  #
          #########
          rust_toolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
            targets = [ "wasm32-unknown-unknown" ];
          };

          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rust_toolchain;

          ##########
          # Common #
          ##########
          pname = "play-release-me";
          version = "0.1.0";

          src = ./.;

          # Common arguments for Crane builds
          commonArgs = {
            inherit src;
            strictDeps = true;
            version = version;
          };

          # Build dependencies only for caching
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # Main application package
          rustPackage = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
          });

          # Docker container for the application
          container = pkgs.dockerTools.buildLayeredImage {
            name = pname;
            tag = version;
            contents = [
              rustPackage
              pkgs.cacert
              pkgs.coreutils
              pkgs.dockerTools.usrBinEnv
              pkgs.dockerTools.binSh
              pkgs.dockerTools.fakeNss
            ];
            config = {
              Entrypoint = [ "${rustPackage}/bin/${pname}" ];
              Env = [];
              ExposedPorts = {
                "8080/tcp" = {};
              };
            };
          };

          # Common build inputs based on features
          commonBuildInputs = with pkgs; [
            # Basic tools
            pkg-config
            gcc
            cmake
            gnumake
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            # Additional darwin specific inputs
            pkgs.libiconv
            pkgs.darwin.apple_sdk.frameworks.Security
            pkgs.darwin.apple_sdk.frameworks.CoreFoundation
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
            pkgs.darwin.apple_sdk.frameworks.System
            pkgs.darwin.apple_sdk.frameworks.Foundation
          ];

        in {
          # Development shell configuration
          devshells.default = {
            packages = commonBuildInputs
              ++ [
              rust_toolchain

              # Development tools
              pkgs.cargo-edit
              pkgs.cargo-audit
              pkgs.cargo-nextest
              pkgs.cargo-watch
            ];

            commands = [
              {
                help = "Format code with rustfmt";
                name = "app-fmt";
                command = "cargo fmt";
              }
              {
                help = "Run code linting";
                name = "lint";
                command = "cargo clippy";
              }
              {
                help = "Run tests";
                name = "app-test";
                command = "cargo nextest run";
              }
              {
                help = "Build the project";
                name = "build";
                command = "cargo build";
              }
              {
                help = "Run the project";
                name = "run";
                command = "cargo run";
              }
            ];

            env = [
              {
                name = "RUST_SRC_PATH";
                value = "${rust_toolchain}/lib/rustlib/src/rust/library";
              }
            ] ++ (if pkgs.stdenv.isDarwin then [
              {
                name = "LIBRARY_PATH"; # Ensures static linking finds the correct paths
                value = "${pkgs.libiconv}/lib:${pkgs.darwin.apple_sdk.frameworks.CoreFoundation}/Library/Frameworks:${pkgs.darwin.apple_sdk.frameworks.SystemConfiguration}/Library/Frameworks:${pkgs.darwin.apple_sdk.frameworks.Security}/Library/Frameworks:${pkgs.darwin.apple_sdk.frameworks.Foundation}/Library/Frameworks";
              }
            ] else [
            ]);
          };

          # Code formatting configuration
          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixpkgs-fmt.enable = true;
              rustfmt.enable = true;
            };
          };

          # Code quality checks
          checks = {
            # Run clippy lints
            clippy = craneLib.cargoClippy (commonArgs // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            });

            # Run cargo tests
            nextest = craneLib.cargoNextest (commonArgs // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
            });

            cargoAudit = craneLib.cargoAudit (commonArgs // {
              inherit src;
              advisory-db = inputs.advisory-db;
            });
          };

          # Build packages
          packages = {
            default = rustPackage;
            container = container;
          };
        };
    };
}
