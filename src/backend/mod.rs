//! Per-tool backend abstraction.
//!
//! aenv routes every supported AI-CLI (Claude Code, Codex, …) through
//! a uniform `Backend` registry: identify by argv[0] file_stem and
//! stable id, then hand off to the backend's shim module. The
//! universal core (manifest, lockfile, store, secrets, tx, paths,
//! audit, hooks, ifl, profile bundles) stays POSIX-only and tool-
//! agnostic, so a future Claude Code or Codex change touches only its
//! backend module.
//!
//! Both shipped backends (`claude`, `codex`) achieve isolation through
//! a single env-var redirect — `CLAUDE_CONFIG_DIR=<env>/.claude` and
//! `CODEX_HOME=<env>/codex` respectively — and exec the real binary
//! directly. No supervisor restart loop, no overlay flag injection.

use anyhow::Result;

pub mod claude;
pub mod codex;
pub mod common;

/// Minimal per-backend info exposed to `main.rs` for shim dispatch.
/// Real launch logic lives in each backend's `shim::run` — there is
/// no `build_launch` / `locate_real` indirection because both shipped
/// backends use the same exec(real, env-var) shape and tests work
/// against the shim entrypoint directly.
pub trait Backend: Send + Sync {
    /// Stable id used in lockfile / manifest / audit references.
    fn id(&self) -> &'static str;

    /// argv[0] file_stem that should dispatch to this backend's shim.
    /// Defaults to `id()`; kept separate so a single backend could in
    /// principle handle aliases.
    fn argv0(&self) -> &'static str {
        self.id()
    }
}

/// Find a backend by argv[0] file_stem.
pub fn for_argv0(stem: &str) -> Option<&'static dyn Backend> {
    all().iter().find(|b| b.argv0() == stem).copied()
}

/// Find a backend by stable id (lockfile / manifest reference).
#[allow(dead_code)]
pub fn for_id(id: &str) -> Option<&'static dyn Backend> {
    all().iter().find(|b| b.id() == id).copied()
}

/// All registered backends. Hand-coded — two tools don't justify
/// pulling in `inventory` for static registration.
pub fn all() -> &'static [&'static dyn Backend] {
    &[&claude::ClaudeBackend, &codex::CodexBackend]
}

/// Dispatch a shim invocation (binary called by its argv[0] tool name,
/// e.g. `claude` from PATH lookup). Each backend owns its own shim
/// module; this is the one place that knows which `shim::run` to call.
pub fn dispatch_shim(backend: &'static dyn Backend, args: Vec<String>) -> Result<u8> {
    match backend.id() {
        "claude" => crate::backend::claude::shim::run(args),
        "codex" => crate::backend::codex::shim::run(args),
        other => anyhow::bail!("shim dispatch for backend '{other}' not yet implemented"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_claude_by_argv0() {
        let b = for_argv0("claude").expect("claude backend registered");
        assert_eq!(b.id(), "claude");
    }

    #[test]
    fn registry_resolves_codex_by_argv0() {
        let b = for_argv0("codex").expect("codex backend registered");
        assert_eq!(b.id(), "codex");
    }

    #[test]
    fn registry_resolves_claude_by_id() {
        let b = for_id("claude").expect("claude backend registered");
        assert_eq!(b.argv0(), "claude");
    }

    #[test]
    fn unknown_argv0_returns_none() {
        assert!(for_argv0("nonexistent-cli").is_none());
    }
}
