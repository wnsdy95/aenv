---
description: Restore the most recent committed (or pending) aenv transaction
argument-hint: [--pending]
allowed-tools: Bash
---

Restores the env to the state captured before the most recent mutating
operation (`install`, `import-global`, `import-profile`). If a previous
process was killed mid-operation, the txn stays in `Pending` state and is
not picked up by default — pass `--pending` to recover.

```
!if [ "$1" = "--pending" ]; then aenv rollback --pending; else aenv rollback; fi
```

Tell the user what was restored, or that the env is now unchanged from
before the last operation.
