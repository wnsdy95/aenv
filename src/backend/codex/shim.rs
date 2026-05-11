//! Codex shim entrypoint.
//!
//! When the binary is invoked as `codex` (via shim symlink in PATH),
//! this resolves the active env, sets `CODEX_HOME=<env_root>/codex`,
//! and `exec()`s the real codex. No supervisor / restart loop — codex
//! has no in-session reload protocol so a one-shot exec is enough.
//!
//! Mirrors `backend::claude::shim::run` for the pre-launch handshake
//! (resolve, fall back to system codex on missing pin) but skips the
//! overlay/supervisor steps the claude shim needs.

use anyhow::{Context, Result};

use crate::env::Env;
use crate::resolve;

use super::{codex_home_for, locate_real_codex};

pub fn run(args: Vec<String>) -> Result<u8> {
    let cwd = std::env::current_dir().context("getcwd")?;
    let resolved = resolve::resolve(&cwd)?;

    let real_codex = locate_real_codex()?;

    let env: Option<Env> = match resolved {
        Some(r) => match crate::env::open_resolved(&r) {
            Ok(env) => Some(env),
            Err(_) => {
                let hint = match (&r.project_manifest, &r.version_file) {
                    (Some(p), _) => format!(" (from {})", p.display()),
                    (_, Some(p)) => format!(" (from {})", p.display()),
                    _ => String::new(),
                };
                eprintln!(
                    "aenv: warning: env '{}' is configured{} but not found. \
                     Running unisolated. Run `aenv list` to see available envs.",
                    r.name, hint
                );
                None
            }
        },
        None => None,
    };

    exec_codex(&real_codex, &args, env.as_ref())
}

#[cfg(unix)]
fn exec_codex(real: &std::path::Path, args: &[String], env: Option<&Env>) -> Result<u8> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(real);
    cmd.args(args);
    if let Some(env) = env {
        apply_env(&mut cmd, env)?;
    }
    // exec() never returns on success; if we get past it, it's an error.
    let err = cmd.exec();
    Err(anyhow::anyhow!("exec {} failed: {err}", real.display()))
}

#[cfg(windows)]
fn exec_codex(real: &std::path::Path, args: &[String], env: Option<&Env>) -> Result<u8> {
    let mut cmd = std::process::Command::new(real);
    cmd.args(args);
    if let Some(env) = env {
        apply_env(&mut cmd, env)?;
    }
    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", real.display()))?;
    Ok(status.code().unwrap_or(1) as u8)
}

/// Set CODEX_HOME and AENV breadcrumbs, ensure the dir exists, fire
/// the pre_activate hook. For the reserved `global` env, leave the
/// inherited environment alone except for stripping a stale
/// CODEX_HOME — codex resolves its own ~/.codex when the var is
/// unset, which is exactly the behavior aliased by `global`.
fn apply_env(cmd: &mut std::process::Command, env: &Env) -> Result<()> {
    if crate::env::is_global(&env.name) {
        cmd.env_remove("CODEX_HOME");
        cmd.env("AENV", &env.name);
        cmd.env("AENV_ACTIVE", &env.name);
        return Ok(());
    }
    let codex_home = codex_home_for(env);
    crate::paths::ensure_dir(&codex_home)
        .with_context(|| format!("ensure CODEX_HOME at {}", codex_home.display()))?;
    cmd.env("CODEX_HOME", codex_home);
    cmd.env("AENV", &env.name);
    cmd.env("AENV_ACTIVE", &env.name);
    crate::backend::common::run_pre_activate(env)?;
    Ok(())
}
