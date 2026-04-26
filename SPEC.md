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
txpt
txpt -- <cmd> [args...]
txpt run -- <cmd> [args...]
txpt run --shell '<shell command>'
txpt diff [TX_ID]
txpt diff [TX_ID] --stat
txpt diff [TX_ID] --name-status
txpt diff [TX_ID] --json
txpt undo [TX_ID]
txpt rollback [TX_ID]
txpt show [TX_ID] [--json]
txpt list [--ids]
txpt ls [--ids]
txpt shims list
txpt shims ls
txpt shims protect <command-pattern>...
txpt shims unprotect <command-pattern>...
txpt shims ignore <command-pattern>...
txpt shims unignore <command-pattern>...
txpt shims edit
txpt prune
```

`txpt -- <cmd>` is explicit run shorthand and is equivalent to `txpt run -- <cmd>`. `txpt` does not treat unknown top-level subcommands as commands to execute. This keeps subcommand typos from creating broken transaction records without making the common path heavy.

When run without arguments in an interactive terminal, `txpt` starts a protected shell session using `$SHELL`. In non-interactive contexts, no-argument `txpt` prints help instead of starting a shell.

Nested sessions are refused. Inside a txpt session, `txpt` without arguments shows the session dashboard, while attempts to start another session through `txpt -- txpt` or `txpt run -- txpt` exit with usage error.

Use `txpt run --shell '<shell command>'` when the command intentionally needs shell parsing, such as redirects, pipes, glob expansion, shell functions, or aliases. Shell mode runs `$SHELL -ic <command>` so aliases from an interactive shell setup can work. Direct exec remains the default because it preserves argv exactly and avoids shell startup side effects.

Transaction selectors are accepted anywhere a transaction id is accepted:

- `@last`: newest transaction
- `@1`: one transaction before newest
- `@2`: two transactions before newest
- raw transaction id

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

`undo` and its alias `rollback` accept:

- `--dry-run`
- `--force`
- `--json`

## Protected Shell Session

`txpt` starts a protected shell session by prepending a temporary shim directory to `PATH`:

```text
PATH=<root>/.txpt/sessions/<id>/bin:$PATH
TXPT_ACTIVE=1
TXPT_SESSION_ID=<id>
TXPT_SESSION_DIR=<root>/.txpt/sessions/<id>
```

The session uses the user's real `$SHELL`; txpt does not parse shell syntax. Selected external commands are routed through `txpt run`, while all other shell behavior remains handled by the user's shell.

For zsh and bash, txpt installs session-local startup hooks that keep the shim directory at the front of `PATH` before each prompt and command execution. This is necessary because user shell startup files, version managers, and package managers may rewrite `PATH` after the shell starts.

If the user's shell already defines a function with the same name as a txpt shim, txpt preserves that function inside the session by moving it to `__txpt_original_<cmd>` and installing a txpt wrapper function in its place. Wrapped invocations create a transaction point and then call the original function. Pass-through invocations call the original function directly. Same-name aliases are removed inside the session because they cannot be safely invoked from txpt's subprocess boundary.

Function wrapping does not run the command inside an extra interactive shell. The wrapper asks txpt to create the before snapshot, invokes the original function in the current shell, then asks txpt to record the after state. This keeps shell-provided behavior such as security wrappers while avoiding nested shell execution for every protected command.

Inside a working session, protected commands should resolve to the session shim directory:

```sh
command -v npm
# <root>/.txpt/sessions/<id>/bin/npm
```

If a protected command was already a shell function, `type <cmd>` may show a txpt session function instead:

```sh
type npm
# npm is a shell function from <root>/.txpt/sessions/<id>/.zshrc
```

When a session starts, txpt generates shims only for commands named by the `protect` and `ignore` rules. It does not try to wrap the whole `PATH`.

Session policy is explicit. Commands matching `protect` are wrapped unless they also match `ignore`. Commands that do not match `protect` are not shimmed. Patterns use the specifier part of Claude Code's `Bash(...)` permission rule syntax. Do not include the `Bash(` and `)` wrapper in normal txpt policy:

```text
npm install:*
rm:*
* --version
```

`*` matches any sequence of characters, including spaces. A trailing `:*` is treated like a trailing ` *`, matching the command prefix at a word boundary. For example, `npm install:*` matches `npm install` and `npm install zod`, but does not match `npm installer`.

Default protect rules cover common workspace-mutating commands such as:

```text
npm install:* / npm update:* / npm uninstall:* / npm audit fix:* / npm add:* / npm remove:*
cargo update:* / cargo add:* / cargo remove:*
go get:* / go mod tidy:*
rm:* / mv:* / cp:* / mkdir:* / rmdir:* / touch:* / chmod:*
```

Default ignore rules cover command metadata checks such as `* --version` and `* -v`.

Commands not matching a protect rule are left alone. If a command is installed during a session and should be protected, add it explicitly with `txpt shims protect "command:*"` or edit `policy.json`.

When a shim wraps a protected package-manager command, ignored paths are included for that point. This allows `npm install` to roll back newly created ignored outputs such as `node_modules/`. Sensitive files remain excluded unless explicitly included by lower-level `txpt run` options.

Session policy is stored at:

```text
.txpt/sessions/<id>/policy.json
```

Example:

```json
{
  "protect": ["npm install:*", "cargo update:*", "rm:*"],
  "ignore": ["* --version", "* -v"]
}
```

`txpt shims protect`, `txpt shims unprotect`, `txpt shims ignore`, `txpt shims unignore`, and `txpt shims edit` update this policy. `txpt shims edit` opens `policy.json` with `$EDITOR`. Shims read the policy each time they run, so changes apply without restarting the shell.

`txpt session` protects selected external commands. It does not make shell builtins, redirections, pipelines, aliases, or functions transactional.

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
    diff.patch
    rollback.log
    rollback.json
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

`txpt undo` and `txpt rollback` compare the current state to the recorded after-state before changing a path.

Only paths whose current state still matches the recorded after-state are rolled back. Paths changed after the transaction are conflicts. By default conflicts are not overwritten.

Rollback is planned before it is applied. `txpt undo --dry-run` and `txpt rollback --dry-run` print the plan and do not modify the workspace. If any conflict exists, default `txpt undo` / `txpt rollback` does not modify any path and exits with code 80. Partial rollback requires an explicit future option; v0.1 does not apply partial rollback by default.

With `--force`, conflicted current files are preserved under:

```text
.txpt/conflicts/<timestamp>/<path>.current
```

Rollback behavior:

- `created_file`: delete if current hash matches after hash
- `created_dir`: delete only if the full current subtree exactly matches the recorded after subtree
- `modified_file`: atomic replace from snapshot if current hash matches after hash
- `deleted_file`: restore from snapshot if still absent
- `type_changed`: restore snapshot if current type matches after type
- `renamed`: represented as `deleted_file + created_file` in v0.1

Symlinks are restored as symlinks. txpt never follows symlinks during rollback.

`--snapshot off` records the point without preimage snapshots. Such points use `record_only` rollback guarantee, appear as `record-only` in human output, and `txpt undo` refuses them with exit code 81.

## Diff And Readiness UI

`txpt list` and `txpt ls` show rollback readiness by default:

```text
ID        AGE        STATE      EXIT  CHANGES      COMMAND
@last     2m         undoable   0     ~2 +1 -0 !0 npm install zod
@1        8m         conflict   0     ~1 +3 -0 !1 cargo update
```

`txpt list --ids` and `txpt ls --ids` print raw transaction ids only.

`txpt show` reads `meta.json`, `command.json`, `changes.jsonl`, and the rollback plan to render a transaction card. `txpt show --json` prints the same structured view as JSON.

`txpt diff` combines transaction diff and current rollback status. `txpt diff --stat`, `txpt diff --name-status`, and `txpt diff --json` provide narrower output modes. A unified `diff.patch` is written at run time for later display. Binary and non-UTF-8 files are summarized with size/hash metadata instead of being shown as empty text diffs.

## Manifest Security

Sensitive files are excluded from rollback by default and are not content-hashed in the manifest. txpt records observable metadata such as size and mtime so sensitive changes can still appear as unprotected changes.

Large protected files are hashed with streaming BLAKE3 rather than reading the whole file into memory.

Ignored directories are recorded as unprotected subtree entries, but txpt does not descend into them by default.

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

## Implementation Notes And References

The implementation keeps state near the project root because CoW clone APIs are most reliable on the same filesystem or volume.

- Apple APFS documentation describes clones as fast, power-efficient same-volume copies: <https://developer.apple.com/documentation/foundation/about-apple-file-system>
- macOS `clonefile(2)` creates copy-on-write clones: <https://www.manpagez.com/man/2/clonefile/>
- Linux `FICLONE` requires source and destination to reside on the same filesystem and preserves copy-on-write isolation: <https://man7.org/linux/man-pages/man2/ioctl_ficlonerange.2.html>
- XDG state directory guidance: <https://specifications.freedesktop.org/basedir/latest/>
- CLI output and exit-code guidance: <https://clig.dev/>
