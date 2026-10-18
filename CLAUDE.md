# carte

A local daemon that runs each app's process group on demand, routes `*.test` hostnames to git worktrees, and stops idle apps. Rust workspace:

- `crates/carte-core`: configuration, naming, resolution, templates, env and lifecycle policies. No long-running IO.
- `crates/carte-api`: types exchanged over the daemon's Unix socket; also the CLI's `--json` output.
- `crates/carte`: the binary (CLI, daemon, shim, TUI).
- `crates/fixture-app`: an HTTP app with knobs, used by integration tests.

## Conventions

- Conventional Commits, scoped by area: `feat(core): …`, `fix(daemon): …`, `test(shim): …`. Before 1.0.0 a `feat:` bumps the minor version (0.x.0); fixes bump the patch.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets` must be clean before committing; run `cargo fmt --all`.
- Tests must not change the machine: no resolver files, keychain, LaunchAgents or privileged ports. Use a test TLD and high ports.
- Never use real project, client or app names in code, tests, docs or commit messages, not even ones seen in local config or logs. Use made-up names like `acme`, `nova`, `shop` or `mono`.
