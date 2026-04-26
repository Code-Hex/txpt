# txpt

`txpt` is a protected shell session for common destructive workspace commands.

```sh
txpt
rm -rf generated/
npm install zod
txpt diff
txpt undo
```

It uses your real shell, wraps selected external commands with undo points, and reports what can and cannot be rolled back.

https://github.com/user-attachments/assets/a87e99ac-e805-4c07-ae32-bfa974072ff8

It protects common workspace-mutating commands such as `rm`, `mv`, `cp`, `npm install`, `cargo update`, `sed -i`, and selected Git cleanup commands. Some default guards, including `dd`, `rsync`, and Git cleanup commands, may still have side effects outside txpt's rollback guarantee; txpt warns about those in the command receipt.

It is not a sandbox. It does not protect `/bin/rm`, `command rm`, shell redirections, interpreter-driven file deletion, sudo changes, files outside the root, or external services.

## Usage

Run `txpt` in an interactive terminal to start a protected shell. Running `txpt` inside that shell shows the session dashboard, and starting a nested session is refused.

```sh
txpt diff @last
txpt undo @last --dry-run
txpt undo @last
txpt rollback @last
txpt ls --json
```

After a successful undo, that point is removed from the active stack, so the next `@last` points to the previous undoable command.
Human `txpt ls` and `txpt diff` output is colored in terminals; use `NO_COLOR=1` to disable it.

You can still create a one-off point explicitly:

```sh
txpt -- npm install zod
txpt -- cargo update
txpt -- sh -c 'find . -name "*.tmp" -delete'
```

## Shims

Session guards are controlled by `.txpt/sessions/<id>/policy.json`:

```json
{
  "protect": ["npm install:*", "cargo update:*", "rm:*"],
  "ignore": ["* --help", "* -h", "* --version", "* -v"]
}
```

Commands matching `protect` are wrapped unless they also match `ignore`. Patterns use the specifier syntax from Claude Code's `Bash(...)` permission rules, but without the `Bash(` and `)` wrapper.

```sh
txpt shims ls
txpt shims edit
```

`shims edit` requires an active txpt session. It edits a temporary copy using `$EDITOR`, or falls back to `vim`, `vi`, then `nano`; after the editor exits, txpt validates the JSON, replaces `policy.json`, and regenerates shims only when the JSON is valid. Invalid edits leave the live policy unchanged and print the temporary file path.

For zsh and bash, txpt makes a best-effort attempt to preserve shell functions such as `npm() { ... }` by wrapping that function inside the session. This is useful for existing package-manager wrappers, but it is not a shell compatibility guarantee.

See [SPEC.md](SPEC.md) for the full product behavior and rollback guarantee.
