# txpt

`txpt` is a protected shell session for risky workspace commands.

It uses your real shell, wraps selected external commands with undo points, and lets you inspect or roll back the last point.

Start a protected shell:

```sh
txpt
```

Inside the session, package-manager style commands are wrapped automatically:

```sh
npm install zod
cargo update
txpt diff
txpt undo
```

Starting another txpt session from inside a session is refused. Running `txpt` by itself inside a session shows the session dashboard.

Wrapped package-manager commands include ignored output paths for that point, so a fresh `node_modules/` created by `npm install` can be removed by `txpt undo`.

To verify that the session is actually protecting a command, check that it resolves to a txpt shim:

```sh
command -v npm
# .../.txpt/sessions/<id>/bin/npm
```

If your shell already defines a function such as `npm() { ... }`, txpt preserves it by wrapping that function inside the session:

```sh
type npm
# npm is a shell function from .../.txpt/sessions/<id>/.zshrc
```

In that case, wrapped commands still get txpt points, and pass-through commands still call your original function.

txpt does not start another interactive shell just to call that function. It records the before state, calls your original function in the current shell, then records the after state.

Shim rules are controlled by `.txpt/sessions/<id>/policy.json`:

```json
{
  "protect": ["npm install:*", "cargo update:*", "rm:*"],
  "ignore": ["* --version", "* -v"]
}
```

Commands matching `protect` are wrapped unless they also match `ignore`. Commands not matching `protect` are left alone. Patterns use the specifier syntax from Claude Code's `Bash(...)` permission rules, but without the `Bash(` and `)` wrapper. Edit the policy in your editor:

```sh
txpt shims edit
```

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

`txpt` session protects selected external commands. It does not make the shell itself transactional, and it does not capture shell builtins, redirections, or pipelines. Use `txpt run --shell '<command>'` when the shell syntax itself must be part of the point.

`txpt` can roll back protected files inside the detected transaction root.

`txpt` does not roll back:

- system files
- sudo changes
- files outside the transaction root
- ignored or sensitive files unless explicitly included
- external services, databases, containers, or network side effects

See [SPEC.md](SPEC.md) for the product and rollback guarantee.
