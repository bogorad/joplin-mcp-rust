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
        moduleTestPackage = pkgs.writeShellScriptBin "joplin-mcpd" ''
          exit 0
        '';
        moduleTestConfig =
          (nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              ./modules/services/joplin-mcp.nix
              {
                system.stateVersion = "25.05";
                services.joplin-mcp = {
                  enable = true;
                  package = moduleTestPackage;
                  credentials = {
                    postgresJoplinDsn = "/run/secrets/postgres-joplin-dsn";
                    postgresMcpDsn = "/run/secrets/postgres-mcp-dsn";
                    tokenHmacKey = "/run/secrets/token-hmac-key";
                  };
                };
              }
            ];
          }).config;
        moduleTestService = moduleTestConfig.systemd.services.joplin-mcpd;
        moduleCheckAssertions = [
          {
            assertion = moduleTestConfig.users.users.joplin-mcp.isSystemUser;
            message = "joplin-mcp user must be a system user";
          }
          {
            assertion = moduleTestConfig.users.users.joplin-mcp.group == "joplin-mcp";
            message = "joplin-mcp user must use the joplin-mcp group";
          }
          {
            assertion = moduleTestService.serviceConfig.User == "joplin-mcp";
            message = "joplin-mcpd service must run as joplin-mcp";
          }
          {
            assertion = moduleTestService.serviceConfig.Group == "joplin-mcp";
            message = "joplin-mcpd service must use group joplin-mcp";
          }
          {
            assertion = moduleTestService.serviceConfig.DynamicUser == false;
            message = "joplin-mcpd service must not use DynamicUser";
          }
          {
            assertion = moduleTestService.serviceConfig.StateDirectory == "joplin-mcp";
            message = "joplin-mcpd service must set StateDirectory";
          }
          {
            assertion = moduleTestService.serviceConfig.RuntimeDirectory == "joplin-mcp";
            message = "joplin-mcpd service must set RuntimeDirectory";
          }
          {
            assertion = moduleTestService.serviceConfig.UMask == "0077";
            message = "joplin-mcpd service must set UMask 0077";
          }
          {
            assertion =
              moduleTestService.serviceConfig.LoadCredential == [
                "postgres-joplin-dsn:/run/secrets/postgres-joplin-dsn"
                "postgres-mcp-dsn:/run/secrets/postgres-mcp-dsn"
                "token-hmac-key:/run/secrets/token-hmac-key"
              ];
            message = "joplin-mcpd service must load the expected credentials";
          }
        ];
        moduleCheck =
          assert nixpkgs.lib.asserts.assertMsg
            (nixpkgs.lib.all (check: check.assertion) moduleCheckAssertions)
            (nixpkgs.lib.concatMapStringsSep "\n" (check: check.message) moduleCheckAssertions);
          pkgs.runCommand "joplin-mcp-nixos-module-check" { } ''
            touch $out
          '';
      in
      {
        formatter = pkgs.nixfmt;

        packages = rec {
          joplin-mcpd = pkgs.rustPlatform.buildRustPackage {
            pname = "joplin-mcpd";
            version = "0.1.24";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            buildAndTestSubdir = "crates/joplin-mcpd";
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = with pkgs; [ openssl ];
          };

          joplin-mcp-client = pkgs.rustPlatform.buildRustPackage {
            pname = "joplin-mcp-client";
            version = "0.1.24";
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

          joplin-mcp-nixos-module = moduleCheck;
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
    )
    // {
      nixosModules.joplin-mcp = ./modules/services/joplin-mcp.nix;
      nixosModules.default = self.nixosModules.joplin-mcp;
    };
}
