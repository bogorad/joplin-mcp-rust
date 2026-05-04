{
  description = "Development environment for joplin-mcp-rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };
      in
      {
        formatter = pkgs.nixfmt;

        packages = rec {
          joplin-mcpd = pkgs.rustPlatform.buildRustPackage {
            pname = "joplin-mcpd";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            buildAndTestSubdir = "crates/joplin-mcpd";
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = with pkgs; [ openssl ];
          };

          joplin-mcp-client = pkgs.rustPlatform.buildRustPackage {
            pname = "joplin-mcp-client";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            buildAndTestSubdir = "crates/joplin-mcp-client";
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = with pkgs; [ openssl ];
          };

          default = joplin-mcpd;
        };

        checks = {
          cargo-fmt =
            pkgs.runCommand "joplin-mcp-rust-fmt"
              {
                nativeBuildInputs = with pkgs; [
                  cargo
                  rustfmt
                ];
              }
              ''
                cp -R ${./.} source
                chmod -R u+w source
                cd source
                cargo fmt --check
                touch $out
              '';

          inherit (self.packages.${system}) joplin-mcp-client joplin-mcpd;
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            cargo-nextest
            cargo-watch
            clippy
            docker-compose
            git
            jq
            just
            nodejs_25
            pkg-config
            postgresql_17
            protobuf
            ripgrep
            rust-analyzer
            rustc
            rustfmt
            sops
            sqlx-cli
            yq-go
          ];

          buildInputs = with pkgs; [
            openssl
          ];

          env = {
            OPENSSL_NO_VENDOR = "1";
            OTLP_LOGS_ENABLED = "true";
            OTLP_LOGS_ENDPOINT = "http://victorialogs.lan:9428/insert/opentelemetry/v1/logs";
            OTLP_TEST_MODE = "true";
            RUST_BACKTRACE = "1";
            RUST_LOG = "info";
            VICTORIALOGS_URL = "http://victorialogs.lan:9428";
          };

          shellHook = ''
            echo "joplin-mcp-rust dev shell"
            echo "Rust: $(rustc --version)"
          '';
        };
      }
    );
}
