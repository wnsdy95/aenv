---
description: Show the active aenv environment and its isolation paths
allowed-tools: Bash
---

Run `aenv status` to display the current environment, its CLAUDE_CONFIG_DIR,
XDG roots, and declared plugins/skills/MCPs. The host binary is the source of
truth — this command is a thin wrapper.

```
!aenv status
```

Then summarize the result for the user in 2-3 lines.
