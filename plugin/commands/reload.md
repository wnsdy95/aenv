---
description: Restart claude under the supervisor (optionally switching env). Auto-resumes the current session.
argument-hint: [env-name]
allowed-tools: Bash
---

Trigger a supervised restart of claude. If `$1` is given, it's the target env.
Claude will exit with code 75; the supervisor wrapper picks up the marker and
relaunches with the new env, auto-injecting `--resume <session>` so the
conversation continues.

If no env is given, the current env is preserved (useful after editing
`aenv.toml` to install a new plugin).

```
!if [ -n "$1" ]; then aenv reload --to "$1"; else aenv reload; fi
```

After this command, claude will restart. Inform the user: "session will resume
in the new env in a moment."
