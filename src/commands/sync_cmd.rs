use anyhow::{bail, Result};

use crate::cli::SyncArgs;
use crate::env::open_or_active;
use crate::install;
use crate::tx;

pub fn run(args: SyncArgs) -> Result<u8> {
    let env = open_or_active(args.env.as_deref())?;
    if crate::env::is_global(&env.name) {
        bail!("'global' has no lockfile to sync against (alias for ~/.claude).");
    }
    let env_name = env.name.clone();
    // Snapshot plugins/skills/settings.json so a mid-sync failure rolls back
    // cleanly — same envelope as `aenv install`.
    let captures = vec![
        env.claude_dir().join("plugins"),
        env.claude_dir().join("skills"),
        env.claude_dir().join("settings.json"),
    ];
    let report = tx::with_tx(
        "sync",
        Some(&env_name),
        &captures,
        Some(format!("aenv sync env={env_name}")),
        || install::sync(&env),
    )?;
    println!(
        "sync: plugins {}, skills {}",
        report.plugins_installed.len(),
        report.skills_installed.len()
    );
    Ok(0)
}
