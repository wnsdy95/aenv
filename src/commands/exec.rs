use std::process::Command;

use anyhow::{anyhow, bail, Result};

use crate::backend::claude::shim;
use crate::cli::ExecArgs;
use crate::env::Env;

/// Run an arbitrary command under an env's launch envelope. Routes
/// through the *same* `shim::apply_env` the claude shim itself uses,
/// so secret bridging (`AENV_<env>_<KEY>`), `pre_activate` hook
/// gating, and the `global`-env special-case (no `CLAUDE_CONFIG_DIR`)
/// stay in lockstep with `claude` invocations. Without this, a child
/// claude launched via `aenv exec -E env -- claude -p …` would see a
/// different env shape than a top-level `claude`, breaking
/// reproducibility and hook-based preflight checks.
pub fn run(args: ExecArgs) -> Result<u8> {
    if args.argv.is_empty() {
        bail!("exec needs a command to run");
    }
    let env_name = match args.env {
        Some(n) => n,
        None => {
            crate::resolve::resolve(&std::env::current_dir()?)?
                .ok_or_else(|| anyhow!("no active env; pass -E <name>"))?
                .name
        }
    };
    let env = Env::open(&env_name)?;

    let mut cmd = Command::new(&args.argv[0]);
    cmd.args(&args.argv[1..]);
    shim::apply_env(&mut cmd, Some(&env))?;
    let status = cmd.status()?;
    Ok(status.code().unwrap_or(1) as u8)
}
