# AGENTS.md

## Project Context

- This repository is developed on NixOS.
- This file supplements `/home/chuck/.dotfiles/opencode/AGENTS.md`.
- Keep changes surgical and tied to the user's current request.

## Tooling On NixOS

- Prefer `nix develop` for project work.
- If a required tool is missing from `PATH`, use Nix native tools instead of installing it globally.
- Use `nix run nixpkgs#<package> -- <args>` for one-off tools when the executable name matches the package.
- Use `nix shell nixpkgs#<package> -c <command> <args>` when the executable name differs or multiple tools are needed.
- Put durable project tooling in `flake.nix`.

## Project Rules

- Do not decrypt or print secret values unless the user explicitly asks for secret inspection.
- Secret key names are contracts; secret values are not documentation.
- Postgres uses separate Joplin and MCP databases and users. Do not collapse them into one database or one user.
- VictoriaLogs for local/live testing is `http://victorialogs.lan:9428`.
- The planned local compose file is `tests/compose.local.yaml`.
- Prefer `just` commands once the repository has a `Justfile`.

## Beads

- Use `bd` for Beads issue operations.
- Do not edit `.beads/issues.jsonl` directly.
- The exported issue snapshot lives at `docs/beads-issues.jsonl` and is maintained by the configured hook.
