//! Helpers shared by every backend's shim.
//!
//! Currently exposes a single hook runner — both the claude and codex
//! shims need to fire `[hooks].pre_activate` right before `exec`, with
//! the same env-var contract and best-effort failure mode. Lives here
//! (above individual backend modules, below the universal core) so a
//! third backend doesn't have to re-implement the contract.

use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::env::Env;

/// Run `[hooks].pre_activate` for `env` if declared. Returns Err on
/// hook failure (non-zero exit *or* spawn failure), and the caller
/// must abort the launch — the user can place arbitrary checks
/// (network reachability, secrets present, repo on the right branch)
/// in the hook and use a non-zero exit to refuse the env.
///
/// The trust model lines up with what's already on disk: a manifest
/// containing a `pre_activate` line is by definition committed by the
/// user (global envs), reviewed in a PR (project mode), or imported
/// with `--trust-hooks` (profile bundles). Anywhere else strips the
/// hook before write. So when we *do* have a hook to run, treating
/// its exit code as authoritative is the consistent default.
///
/// `AENV_NAME` and `AENV_ROOT` mirror the supervisor-era contract so
/// existing user hooks (committed in projects, in shared envs) keep
/// working without rewrites. Hooks are inherently `sh -c` — same RCE
/// surface as before.
pub fn run_pre_activate(env: &Env) -> Result<()> {
    let manifest = match env.manifest() {
        Ok(m) => m,
        // A broken/missing manifest already surfaces a louder error
        // upstream (the shim resolved the env in the first place).
        // Skip the hook silently rather than double-warning here.
        Err(_) => return Ok(()),
    };
    let Some(cmd) = manifest.hooks.pre_activate.as_deref() else {
        return Ok(());
    };
    tracing::info!(env = %env.name, cmd, "running pre_activate hook");
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("AENV_NAME", &env.name)
        .env("AENV_ROOT", &env.root)
        .status()
        .with_context(|| {
            format!(
                "spawn pre_activate hook for env '{}': sh -c <hook>",
                env.name
            )
        })?;
    if !status.success() {
        bail!(
            "pre_activate hook for env '{}' refused launch (exit status {}). \
             Edit aenv.toml `[hooks].pre_activate` or run with the hook fixed.",
            env.name,
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}
