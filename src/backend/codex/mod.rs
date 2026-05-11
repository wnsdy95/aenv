//! Codex backend.
//!
//! Codex achieves env isolation through a single environment variable:
//! `CODEX_HOME=<dir>` redirects sessions, history, credentials, MCP
//! config, and the rest of `~/.codex/`'s state into the named directory.
//! No overlay flags, no settings.json scraping, no supervisor restart
//! loop — the launch is a straight `exec(codex, argv)` with one env var
//! set. This is the upper bound on what an env-switcher can be: clean
//! tool-side support → clean wrapper.
//!
//! Layout under an aenv env:
//!   `<env_root>/codex/`
//!     `config.toml`     — codex's main config (mcp_servers etc.)
//!     `sessions/`       — codex's session journal (managed by codex)
//!     `auth.json`       — first-launch login under that CODEX_HOME
//!     `history/` `tmp/` — codex-managed runtime state
//!
//! Auth: each env starts unauthenticated the first time. `CODEX_HOME`
//! changes the auth file location, so codex can't piggyback on the
//! user's pre-aenv `~/.codex/auth.json` — the user logs in once per env
//! (or copies the file across envs themselves). Document this as the
//! tradeoff for real isolation.

pub mod mcp_render;
pub mod shim;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::backend::Backend;
use crate::env::Env;
use crate::error::AenvError;
use crate::paths;

pub struct CodexBackend;

impl Backend for CodexBackend {
    fn id(&self) -> &'static str {
        "codex"
    }
}

/// On-disk filename of the real codex binary.
pub fn codex_binary_name() -> &'static str {
    if cfg!(windows) {
        "codex.exe"
    } else {
        "codex"
    }
}

/// `CODEX_HOME` for this env. For on-disk envs that's
/// `<env_root>/codex/`; for the reserved `global` env (alias for the
/// user's real codex install) it's `~/.codex` — the dot-prefixed home
/// codex uses when `CODEX_HOME` is unset. Worth special-casing here
/// because `Env::global().root` is `$HOME`, and `$HOME/codex` (no dot)
/// would point at a fresh empty dir, defeating the alias.
pub fn codex_home_for(env: &Env) -> PathBuf {
    if crate::env::is_global(&env.name) {
        return env.root.join(".codex");
    }
    env.root.join("codex")
}

/// Find the real codex binary on PATH, excluding our own shims dir.
/// Mirrors `claude::shim::locate_real_claude`'s PATH-first ordering so
/// codex upgrades land predictably.
pub fn locate_real_codex() -> Result<PathBuf> {
    let shims = paths::shims_dir().ok();
    if let Some(path) = std::env::var_os("PATH") {
        let target_name = codex_binary_name();
        for dir in std::env::split_paths(&path) {
            if let Some(s) = &shims {
                if dir == *s {
                    continue;
                }
            }
            let candidate = dir.join(target_name);
            if candidate.is_file() && is_executable(&candidate) && !is_aenv_shim(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(AenvError::RealClaudeNotFound)
        .context("could not find real codex in PATH. install codex first, then add a codex shim")
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
        && p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
}

fn is_aenv_shim(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    if let Ok(target) = std::fs::read_link(p) {
        let target_str = target.to_string_lossy();
        if target_str.contains("/aenv") || target_str.ends_with("aenv") {
            return true;
        }
    }
    let _ = p;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_home_lives_under_env_root() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env {
            name: "x".into(),
            root: tmp.path().to_path_buf(),
            manifest_override: None,
        };
        assert_eq!(codex_home_for(&env), tmp.path().join("codex"));
    }
}
