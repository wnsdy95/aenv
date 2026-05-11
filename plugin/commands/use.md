---
description: Queue a different aenv environment for the next claude restart
argument-hint: <env-name>
allowed-tools: Bash
---

Queue env switch: writes the pending env marker so the supervisor picks up the
new env on the next relaunch. The current session stays in the old env until
the user runs `/aenv:reload`.

```
!aenv use $1
```

Tell the user: env switch is queued; run `/aenv:reload` to apply it.
