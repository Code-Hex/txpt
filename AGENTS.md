## Project

This repository contains `txpt`, a Rust CLI for creating reversible transaction points around Unix commands.

Product behavior, CLI semantics, rollback guarantees, platform behavior, and release scope belong in `SPEC.md`. Read `SPEC.md` before changing user-visible behavior. Do not duplicate the full product specification here.

## Working principles

- Keep changes small, reviewable, and directly tied to the requested task.
- Prefer simple, explicit Rust over clever abstractions.
- Preserve rollback safety over convenience.
- Treat filesystem writes, path handling, symlink behavior, and restore logic as security-sensitive.
- Do not broaden the rollback guarantee unless tests and documentation are updated.
- Do not add new runtime dependencies without a clear reason.
- Do not introduce network calls in normal CLI execution.

## Repository layout

Expected layout:

- `src/main.rs`: CLI entry point.
- `src/cli/`: argument parsing and command dispatch.
- `src/root/`: root detection.
- `src/ignore/`: include and exclude policy.
- `src/manifest/`: filesystem scanning and manifest generation.
- `src/snapshot/`: snapshot engines.
- `src/runner/`: child process execution.
- `src/diff/`: change detection and reporting.
- `src/rollback/`: undo planning and restore logic.
- `src/storage/`: transaction directory layout and metadata.
- `src/platform/`: Linux and macOS platform-specific code.
- `tests/`: integration tests.
- `SPEC.md`: product and behavior specification.
- `skills/`: reusable agent workflows for this project.

If the layout changes, update this section.

## Rust style

- Use stable Rust.
- Run `cargo fmt` before finishing.
- Run `cargo clippy --all-targets --all-features -- -D warnings` before finishing.
- Prefer `Result<T, Error>` over panics in library code.
- Panics are acceptable only for programmer errors in tests.
- Keep platform-specific code behind small interfaces.
- Keep unsafe code isolated, documented, and covered by tests.
- Use `Path` and `PathBuf` for paths. Do not manipulate filesystem paths as plain strings.
- Normalize and validate root-relative paths before using them for restore operations.
- Never follow symlinks during rollback unless `SPEC.md` explicitly requires it.

## Error handling

- User-facing errors must be precise and actionable.
- Preserve the child process exit code when `txpt run` successfully records a transaction.
- Use distinct `txpt` errors when `txpt` itself fails before or after child execution.
- Do not hide partial rollback or unsupported rollback conditions.
- If a path is not protected, report it as unprotected rather than implying rollback support.

## Snapshot and rollback rules

- Do not use hardlinks as snapshots.
- Prefer copy-on-write clone engines where available.
- On macOS, implement APFS clone behavior through a dedicated snapshot engine.
- On Linux, implement reflink behavior through a dedicated snapshot engine.
- Fall back to normal copy when clone support is unavailable.
- Rollback must validate the current state against the recorded after-state before modifying files.
- On conflict, do not overwrite by default.
- `--force` behavior must preserve conflicting current files in the transaction conflict area before restoring.
- Restore files atomically through a temporary file and rename.
- Do not write outside the transaction root during rollback.

## Testing

Run the relevant subset while iterating, then run the full suite before finishing.

Minimum checks:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
````

Integration tests should cover:

* modified file rollback
* created file rollback
* deleted file rollback
* created directory cleanup
* conflict detection
* forced rollback with conflict preservation
* ignored and unprotected paths
* sensitive file defaults
* symlink handling
* root detection with and without Git
* child process exit code preservation
* snapshot engine fallback behavior
* path traversal rejection
* macOS clone engine behavior where available
* Linux reflink engine behavior where available

Use temporary directories for tests. Tests must not depend on the developer's home directory, global Git config, network access, or system-specific absolute paths.

## Installation and local use

Build locally:

```sh
cargo build
```

Run from the repository:

```sh
cargo run -- --help
cargo run -- -- true
cargo run -- diff
cargo run -- undo --dry-run
```

Install locally for manual testing:

```sh
cargo install --path .
```

After installation:

```sh
txpt --help
txpt -- true
txpt list
```

Do not assume `txpt` is installed globally during tests. Prefer `cargo run` or test binaries.

## Documentation

* Update `SPEC.md` for any user-visible behavior change.
* Update command help text when flags or subcommands change.
* Update examples when output shape changes.
* Keep `AGENTS.md` concise. Put detailed behavior in `SPEC.md`.
* If Codex repeatedly makes the same mistake, add a short rule here or to a more specific `AGENTS.md` near the relevant code.

## Skills

This project should maintain local agent skills under `skills/` for repeatable workflows.

Add or update a skill when a workflow becomes recurring, such as:

* implementing a new snapshot engine
* adding rollback test cases
* reviewing filesystem safety
* preparing a release
* updating CLI help and examples
* validating Linux or macOS platform behavior

Each skill must live in its own directory and include a `SKILL.md` with clear `name` and `description` metadata. Keep skill descriptions specific so an agent can select the right skill without loading unnecessary context.

When a workflow changes, update the corresponding skill in the same pull request.

## Done criteria

A change is not complete until:

* the implementation matches `SPEC.md`
* relevant tests are added or updated
* `cargo fmt --check` passes
* `cargo clippy --all-targets --all-features -- -D warnings` passes
* `cargo test --all-targets --all-features` passes
* user-facing behavior is documented
* rollback safety assumptions are explicit
