# Contributing

This guide covers bug reports, feature requests, and pull requests. It applies to everyone, including AI agents filing on someone's behalf.

In the GitHub web interface, issue forms guide you through the structure. If you file through the API or the `gh` CLI, the forms are bypassed: copy the matching skeleton from [Skeletons](#skeletons) and keep the headings exactly as written.

Bug reports and pull requests are welcome on
[GitHub](https://github.com/elektronaut/carte). Everyone participating
is expected to follow the [code of conduct](CODE_OF_CONDUCT.md).

## Principles

### For all issues and pull requests

**Report only what you observed or verified.** Whoever investigates, human or agent, anchors on the claims in a report. Unverified claims cost more time than they save.

**No hypotheses about causes.** Diagnosis is the maintainers' job. They add theories as comments, where the author shows how much weight to give them. Speculation in a report misleads everyone who reads it.

**Keep it short.** Length hides the useful information. A long report usually means two reports, or padding.

**One concern per issue or pull request.** Mixed concerns can't be tracked, reviewed, or reverted independently.

**Use the exact headings.** Maintainers and their tools rely on them to process reports.

### Bug reports

**Minimal, numbered steps you actually ran.** A reproduction nobody has run is speculation too.

**Raw output, not a paraphrase.** The details that matter are the ones a summary drops.

**Ruled out: negative results only.** "Still happens with caching disabled" narrows the search without steering it.

### Feature requests

**Problem before solution.** A proposed solution constrains the design before anyone familiar with the code has weighed in.

### Pull requests

**Open an issue before writing non-trivial code.** Code is cheap to write; choosing the right change is the expensive part. An issue lets maintainers weigh in on the approach before anything is built, while an unsolicited implementation anchors the discussion on one solution. Open a pull request directly only when there's one obvious way to make the change: typos, broken links, or a clear-cut fix. Reporting those costs more than fixing them. They don't need the template; a one-line description is ideal.

**Describe what changed, not what it might fix.** The diff shows how. Broad claims steer reviewers the same way hypotheses steer investigators.

### Security

**Never report vulnerabilities publicly.** Report them privately as described in [SECURITY.md](SECURITY.md).

## Getting started

carte is a Rust workspace. Install a stable toolchain with
[rustup](https://rustup.rs); the minimum supported version is the
`rust-version` in `Cargo.toml`.

- `crates/carte-core`: configuration, naming, resolution, templates, env
  and lifecycle policies. No long-running IO.
- `crates/carte-api`: types exchanged over the daemon's Unix socket; also
  the CLI's `--json` output.
- `crates/carte`: the binary (CLI, daemon, shim, TUI).
- `crates/fixture-app`: an HTTP app with knobs, used by integration tests.

Run the checks CI runs before pushing:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs them on macOS and Linux. `carte setup` is macOS-only for now, but
the rest of the suite passes on both. CI runs the tests with
[cargo-nextest](https://nexte.st), `cargo nextest run --workspace --profile ci`,
which runs each test in its own process and fails one that is still running
after three minutes (`.config/nextest.toml`). Either runner works locally.

CI also checks dependencies with [cargo-deny](https://github.com/EmbarkStudios/cargo-deny)
against `deny.toml`: licenses, sources and duplicate versions, and security
advisories. A new advisory shows up in the run without failing it. Run
`cargo deny check` to see the same locally.

Property tests in carte-core use proptest and run with the rest of the
suite. When one fails, proptest saves the case under
`crates/carte-core/proptest-regressions/`; commit that file with the fix.

## Tests and commits

- Add tests for any behavior you change.
- Tests must not change the machine: no resolver files, keychain entries,
  LaunchAgents or privileged ports. Use a test TLD and high ports.
- Write commit messages using
  [Conventional Commits](https://www.conventionalcommits.org), scoped by
  area: `feat(core): …`, `fix(daemon): …`, `test(shim): …`. The changelog
  and releases are generated from them, so the `feat:` and `fix:` prefixes
  decide what ends up in the next release. Before 1.0.0 a `feat:` bumps the
  minor version and a fix the patch.
- Leave the version and `CHANGELOG.md` alone. Both are updated
  automatically when a release is cut.

### Fuzzing

`fuzz/` holds [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets
for carte-core's parsers. It isn't part of the workspace and needs a nightly
toolchain: `cargo +nightly fuzz run strip`. A weekly workflow runs every
target for a few minutes. When it finds a crash, open an issue with the
input from the run's artifacts and add a regression test with the fix.

## Skeletons

Leave out optional sections you have nothing for.

### Bug report

```markdown
### Expected behavior

<!-- What should happen. -->

### Actual behavior

<!-- What happened instead, as observed. Include the error message, if any. -->

### Steps to reproduce

1.
2.
3.

### Error output

<!-- Optional. Raw output in a code block, such as `carte logs` or the daemon's log lines. -->

### Version

<!-- The output of `carte --version`, and your OS version. -->

### Ruled out

<!-- Optional. Negative results only, e.g. "Still happens after restarting the daemon." -->
```

### Feature request

```markdown
### Problem

<!-- What you're trying to do, and what's blocking you. -->

### Current workaround

<!-- Optional. How you get around it today, if at all. -->

### Proposed solution

<!-- Optional, and brief. -->
```

### Pull request

See [.github/pull_request_template.md](.github/pull_request_template.md).
