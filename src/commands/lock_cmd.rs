use anyhow::{bail, Result};

use crate::cli::LockArgs;
use crate::env::open_or_active;
use crate::install;
use crate::tx;

pub fn run(args: LockArgs) -> Result<u8> {
    let env = open_or_active(args.env.as_deref())?;
    if crate::env::is_global(&env.name) {
        bail!("'global' has no aenv.lock to refresh (alias for ~/.claude).");
    }
    let env_name = env.name.clone();
    // lockfile is the only file we touch; capture it for atomic rollback.
    // In project mode this is the project's `aenv.lock`; in global mode
    // it's the slot's copy. Both paths are visible to the rollback layer.
    let captures = vec![env.lockfile_path()];
    tx::with_tx(
        "lock",
        Some(&env_name),
        &captures,
        Some(format!("aenv lock env={env_name}")),
        || install::lock_only(&env),
    )?;
    println!("aenv.lock updated");
    Ok(0)
}
