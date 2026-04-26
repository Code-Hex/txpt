# txpt

`txpt` is a protected shell session for common destructive workspace commands.

```sh
txpt
rm -rf generated/
npm install zod
txpt diff
txpt undo
```

It uses your real shell and wraps selected external commands with undo points.

It protects common workspace-mutating commands such as `rm`, `mv`, `cp`, `npm install`, `cargo update`, `sed -i`, and selected Git cleanup commands.

It is not a sandbox. It does not protect `/bin/rm`, `command rm`, shell redirections, interpreter-driven file deletion, sudo changes, files outside the root, or external services.

Default guards include file-mutating commands such as `rm`, `unlink`, `rmdir`, `mv`, `cp`, `ln`, `chmod`, `chown`, `truncate`, `patch`, `tee`, `rsync`, and `dd`; in-place editors such as `sed -i` and `perl -i`; destructive `find` / `xargs` patterns; selected Git workspace cleanup commands; and mutating package-manager commands for JavaScript, Rust, Go, and Python tools.

Starting another txpt session from inside a session is refused. Running `txpt` by itself inside a session shows the session dashboard.

Wrapped package-manager commands include ignored output paths for that point, so a fresh `node_modules/` created by `npm install` can be removed by `txpt undo`.

To verify that the session is actually protecting a command, check that it resolves to a txpt shim:

```sh
command -v npm
# .../.txpt/sessions/<id>/bin/npm
```

For zsh and bash, txpt makes a best-effort attempt to preserve shell functions such as `npm() { ... }` by wrapping that function inside the session:

```sh
type npm
# npm is a shell function from .../.txpt/sessions/<id>/.zshrc
```

In that case, wrapped commands still get txpt points, and pass-through commands still call your original function. This is not a shell compatibility guarantee.

txpt does not start another interactive shell just to call that function. It records the before state, calls your original function in the current shell, then records the after state.

Shim rules are controlled by `.txpt/sessions/<id>/policy.json`:

```json
{
  "protect": ["npm install:*", "cargo update:*", "rm:*"],
  "ignore": ["* --help", "* -h", "* --version", "* -v", "* version", "* help"]
}
```

Commands matching `protect` are wrapped unless they also match `ignore`. Commands not matching `protect` are left alone. Patterns use the specifier syntax from Claude Code's `Bash(...)` permission rules, but without the `Bash(` and `)` wrapper. Edit the policy in your editor:

```sh
txpt shims edit
```

`shims edit` requires an active txpt session. It uses `$EDITOR`, or falls back to `vim`, `vi`, then `nano`; after the editor exits, txpt validates `policy.json` and regenerates shims only when the JSON is valid.

txpt generates shims only for commands named by `protect` or `ignore` rules. It does not try to wrap your whole `PATH`.

You can still create a one-off point explicitly:

```sh
txpt -- npm install zod
txpt -- cargo update
txpt -- sh -c 'find . -name "*.tmp" -delete'
```

After the command, `txpt` shows what changed and whether the point is still undoable.

```sh
txpt diff @last
txpt undo @last --dry-run
txpt undo @last
txpt rollback @last
```

`txpt` is not a shell, backup system, container, or AI wrapper. It is a small command receipt and rollback layer for files under the detected project root.

`txpt` is not a sandbox. A session protects normal interactive command use through shell functions and `PATH` shims. It does not protect absolute-path invocations such as `/bin/rm`, `command rm`, shell redirections, interpreter-driven file deletion, or external side effects. Use `txpt run --shell '<command>'` when the shell syntax itself must be part of the point.

`txpt` can roll back protected files inside the detected transaction root.

`txpt` does not roll back:

- system files
- sudo changes
- files outside the transaction root
- ignored or sensitive files unless explicitly included
- external services, databases, containers, or network side effects

See [SPEC.md](SPEC.md) for the product and rollback guarantee.
