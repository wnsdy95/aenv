use std::process::Command;

use anyhow::{Context, Result};

use crate::backend::claude::shim;
use crate::cli::RunArgs;
use crate::env::Env;

/// Internal: launch claude under the active or specified env. Used
/// by tests and as a fallback CLI invocation point. Routes through
/// `shim::apply_env` so the launch envelope is byte-identical to the
/// real shim path — global special-case, secret bridging, and
/// pre_activate gating all live in one place.
pub fn run(args: RunArgs) -> Result<u8> {
    let claude = match args.claude_bin {
        Some(p) => p,
        None => shim::locate_real_claude()?,
    };

    let env: Option<Env> = if let Some(n) = &args.env {
        Some(Env::open(n)?)
    } else {
        match crate::resolve::resolve(&std::env::current_dir()?)? {
            Some(r) => Some(crate::env::open_resolved(&r)?),
            None => None,
        }
    };

    let mut cmd = Command::new(&claude);
    cmd.args(&args.argv);
    shim::apply_env(&mut cmd, env.as_ref())?;
    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", claude.display()))?;
    Ok(status.code().unwrap_or(1) as u8)
}
