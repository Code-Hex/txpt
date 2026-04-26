# txpt

`txpt` creates reversible transaction points around Unix commands.

```sh
txpt -- npm install zod
txpt diff
txpt undo
txpt list
txpt show
txpt prune
txpt inspect --json
```

`txpt` can roll back protected files inside the detected transaction root.

`txpt` does not roll back:

- system files
- sudo changes
- files outside the transaction root
- ignored or sensitive files unless explicitly included
- external services, databases, containers, or network side effects

See [SPEC.md](SPEC.md) for the product and rollback guarantee.

