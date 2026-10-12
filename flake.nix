{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };
  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (system:
      let
        pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
        honkd = rustPlatform.buildRustPackage {
          pname = "honkd";
          version = "0.0.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "honkd" "-p" "honk" ];
          # Real test suite needs a live goose / stub instance and is run by CI.
          doCheck = false;
        };
      in {
        packages = {
          inherit honkd;
          default = honkd;
        };
        devShells.default = pkgs.mkShell {
          packages = [ rust pkgs.pkg-config pkgs.openssl pkgs.jq pkgs.websocat pkgs.shellcheck ];
        };
      });
}
