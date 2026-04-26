# txpt

`txpt` creates reversible transaction points around Unix commands.

```sh
txpt run -- npm install zod
txpt run --shell 'npm install zod > npm.log'
txpt list
txpt show @last
txpt diff
txpt undo
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
