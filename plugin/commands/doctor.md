---
description: Diagnose aenv installation, env health, and pending transactions
allowed-tools: Bash
---

Run `aenv doctor` to check shim install, PATH ordering, env directory layout,
isolation roots, `cc_compatible` semver match, managed-vs-user plugin
counts, and any pending transactions left by a killed process.

```
!aenv doctor
```

If the report shows `pending transaction(s) found`, recover with
`/aenv:rollback` (or `aenv rollback --pending` from a shell). If a
`broken env` is flagged, inspect `~/.aenv/envs/<name>/aenv.toml` — it's
either missing or has a parse error.
