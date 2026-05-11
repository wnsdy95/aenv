---
description: Spawn a one-shot child claude in a different env (sandbox/test/A-B compare)
argument-hint: <env-name> <prompt>
allowed-tools: Bash
---

Spawns a child Claude Code process running under a different env's plugins,
skills, and MCPs — without disturbing your current session. The killer
in-session feature: test risky plugins or compare configurations without
polluting the main env.

The child runs in headless mode (`-p`) and returns its output here.

```
!aenv exec -E "$1" -- claude -p "$2" --output-format text
```

Show the user the child's output. Note that the child's session is ephemeral.
