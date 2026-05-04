{
  description = "DevEnv for joplin-mcp-rust";

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
          # Allow unfree packages used by tooling in this dev shell (yc).
          config.allowUnfree = true;
        };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            nodejs_25 # node runtime
          ];

          shell = "${pkgs.zsh}/bin/zsh";

          shellHook = ''
            # Automatically install dependencies with pnpm if not already installed
            if [ ! -d "node_modules" ]; then
              echo "Running 'pnpm install' for you..."
              pnpm install
            fi

            export OTLP_LOGS_ENABLED="true"
            export OTLP_TEST_MODE="true"
            export OTLP_LOGS_ENDPOINT="http://victorialogs.lan:9428/insert/opentelemetry/v1/logs"
            echo "================================="
            echo "Hi!"
            echo "================================="
          '';
        };
      }
    );
}
