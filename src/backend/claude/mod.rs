//! Claude Code backend.
//!
//! Single env-var redirect (`CLAUDE_CONFIG_DIR=<env>/.claude`) does the
//! whole isolation. Verified against Claude Code 2.1.138 — the bundled
//! JS resolves config root via
//! `process.env.CLAUDE_CONFIG_DIR ?? path.join(homedir(), ".claude")`,
//! and every downstream path (plugins, projects/, installed_plugins.json,
//! plugin cache) is `path.join(configRoot, …)`. So a single
//! `CLAUDE_CONFIG_DIR` setting routes auth, sessions, plugins, MCP,
//! `enabledPlugins`, hooks, and the keychain hash without any overlay
//! flags or supervisor restart loop.
//!
//! Trade-off: each env's keychain entry is independent (the macOS
//! Keychain service name is hashed from `CLAUDE_CONFIG_DIR`), so a
//! freshly-created env starts unauthenticated and the user runs
//! `/login` once. Same shape Codex has, accepted as the cost of real
//! isolation.

pub mod installed_plugins;
pub mod known_marketplaces;
pub mod shim;

use crate::backend::Backend;

/// Marker type implementing the `Backend` trait for Claude Code.
pub struct ClaudeBackend;

impl Backend for ClaudeBackend {
    fn id(&self) -> &'static str {
        "claude"
    }
}
