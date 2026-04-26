# txpt MVP Specification

`txpt` (Transaction Point) creates reversible transaction points around Unix commands.

`txpt` can roll back protected files inside the detected transaction root.

`txpt` does not roll back:

- system files
- sudo changes
- files outside the transaction root
- ignored or sensitive files unless explicitly included
- external services, databases, containers, or network side effects

## CLI

```sh
txpt -- <cmd> [args...]
txpt run -- <cmd> [args...]
txpt run --shell '<shell command>'
txpt diff [TX_ID]
txpt undo [TX_ID]
txpt show [TX_ID]
txpt list
txpt prune
txpt doctor
txpt inspect --json
```

`txpt -- <cmd>` is shorthand for `txpt run -- <cmd>`.

Use `txpt run --shell '<shell command>'` when the command intentionally needs shell parsing, such as redirects, pipes, glob expansion, shell functions, or aliases. Shell mode runs `$SHELL -ic <command>` so aliases from an interactive shell setup can work. Direct exec remains the default because it preserves argv exactly and avoids shell startup side effects.

`run` accepts:

- `--root <path>`
- `--snapshot auto|clone|copy|off`
- `--shell '<shell command>'`
- `--json`
- `--no-stream`
- `--include-ignored`
- `--include-sensitive`
- `--strict`
- `--keep`

`undo` accepts:

- `--dry-run`
- `--force`
- `--json`

## Guarantee Boundary

The rollback guarantee applies only to paths below the transaction root that were included in the snapshot policy. Changes outside the root, sudo changes, ignored files, sensitive files, external databases, containers, and network side effects are not rolled back.

Sensitive files are excluded by default:

- `.env`
- `.env.*`
- `*.pem`
- `*.key`
- `id_rsa`
- `id_ed25519`
- `.npmrc`
- `.pypirc`
- `.netrc`

Ignored and sensitive changes are reported as `unprotected` unless explicitly included.

## Root Detection

Root detection order:

1. `--root <path>`
2. Git worktree root
3. nearest parent containing `.txpt-root`
4. nearest project marker: `package.json`, `pyproject.toml`, `Cargo.toml`, `go.mod`, `deno.json`, `bun.lock`, `pnpm-lock.yaml`
5. nearest build marker: `Makefile`, `justfile`, `Dockerfile`, `docker-compose.yml`
6. current working directory

Without Git, txpt never auto-expands to the user's home directory or `/`.

## State Layout

The default state directory is local to the project:

```text
<root>/.txpt/
```

Transactions are stored as:

```text
.txpt/
  tx/<TX_ID>/
    meta.json
    command.json
    before.manifest.jsonl
    after.manifest.jsonl
    changes.jsonl
    snapshot/
    stdout.log
    stderr.log
    rollback.log
  tmp/
  locks/
  conflicts/
```

## Snapshot Engines

MVP first-class platforms:

- macOS: APFS `clonefile` -> normal copy -> record-only
- Linux: `ioctl(FICLONE)` -> normal copy -> record-only
- other Unix: normal copy -> record-only

Hardlinks are never used as snapshots.

The clone/reflink probe creates a small source and destination in `.txpt/tmp`, mutates the source, and verifies the destination did not change.

## Run Lifecycle

1. detect root
2. acquire transaction lock
3. resolve include/exclude policy
4. probe snapshot engine
5. create before manifest
6. create preimage snapshot
7. spawn child command
8. stream stdout/stderr unless disabled
9. wait for child
10. create after manifest
11. write changes
12. print human or JSON summary
13. return the child exit code if txpt recorded successfully

Rollback mode refuses `sudo` unless `--snapshot off` is used.

## Rollback

`txpt undo` compares the current state to the recorded after-state before changing a path.

Only paths whose current state still matches the recorded after-state are rolled back. Paths changed after the transaction are conflicts. By default conflicts are not overwritten.

With `--force`, conflicted current files are preserved under:

```text
.txpt/conflicts/<timestamp>/<path>.current
```

Rollback behavior:

- `created_file`: delete if current hash matches after hash
- `created_dir`: delete if recorded tree still matches
- `modified_file`: atomic replace from snapshot if current hash matches after hash
- `deleted_file`: restore from snapshot if still absent
- `type_changed`: restore snapshot if current type matches after type
- `renamed`: represented as `deleted_file + created_file` in v0.1

Symlinks are restored as symlinks. txpt never follows symlinks during rollback.

## Exit Codes

| code | meaning |
| ---: | --- |
| child code | child command exit code |
| 64 | usage error |
| 66 | root detection failed |
| 73 | snapshot creation failed |
| 74 | after-scan or record write failed |
| 75 | temporary failure, lock conflict |
| 80 | rollback conflict |
| 81 | rollback unsupported |
| 82 | unsafe command refused |

If snapshot creation fails, the child command is not executed. If the child fails, txpt still records the transaction and returns the child exit code.

## Inspect JSON

`txpt inspect --json` prints capabilities to stdout for agents and CI:

```json
{
  "name": "txpt",
  "version": "0.1.0",
  "schema_version": 1,
  "capabilities": {
    "run": true,
    "diff": true,
    "undo": true,
    "json": true,
    "sandbox": false,
    "mcp": false
  },
  "snapshot_engines": ["apfs-clonefile", "linux-ficlone", "copy", "record-only"],
  "rollback_guarantees": [
    "full",
    "cleanup_only",
    "metadata_partial",
    "conflict",
    "unprotected",
    "unsupported"
  ]
}
```

## Implementation Notes And References

The implementation keeps state near the project root because CoW clone APIs are most reliable on the same filesystem or volume.

- Apple APFS documentation describes clones as fast, power-efficient same-volume copies: <https://developer.apple.com/documentation/foundation/about-apple-file-system>
- macOS `clonefile(2)` creates copy-on-write clones: <https://www.manpagez.com/man/2/clonefile/>
- Linux `FICLONE` requires source and destination to reside on the same filesystem and preserves copy-on-write isolation: <https://man7.org/linux/man-pages/man2/ioctl_ficlonerange.2.html>
- XDG state directory guidance: <https://specifications.freedesktop.org/basedir/latest/>
- CLI output and exit-code guidance: <https://clig.dev/>
