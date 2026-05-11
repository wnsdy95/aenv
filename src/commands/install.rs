use anyhow::{bail, Result};

use crate::cli::InstallArgs;
use crate::env::open_or_active;
use crate::install;
use crate::tx;

pub fn run(args: InstallArgs) -> Result<u8> {
    let env = open_or_active(args.env.as_deref())?;
    if crate::env::is_global(&env.name) {
        bail!(
            "'global' is the reserved alias for ~/.claude — there is no aenv.toml \
             to install from. Use a real env (`aenv list`)."
        );
    }
    // Capture every file install can mutate. Without settings.json or
    // aenv.lock in the snapshot, a partial failure (e.g., lock save fails
    // after settings was rewritten) would leave settings.json modified with
    // no rollback path.
    let captures = vec![
        env.claude_dir().join("plugins"),
        env.claude_dir().join("skills"),
        env.claude_dir().join("settings.json"),
        crate::env::manifest::Manifest::lockfile_path(&env.root),
    ];
    let env_name = env.name.clone();
    let update_lock = !args.no_lock;
    let report = tx::with_tx(
        "install",
        Some(&env_name),
        &captures,
        Some(format!("aenv install env={env_name}")),
        || install::install(&env, update_lock),
    )?;
    print_report("install", &report);
    Ok(0)
}

fn print_report(label: &str, r: &install::Report) {
    println!(
        "{label}: plugins +{} ={}, skills +{} ={}",
        r.plugins_installed.len(),
        r.plugins_already.len(),
        r.skills_installed.len(),
        r.skills_already.len()
    );
    if !r.plugins_installed.is_empty() {
        println!("  installed plugins: {}", r.plugins_installed.join(", "));
    }
    if !r.skills_installed.is_empty() {
        println!("  installed skills:  {}", r.skills_installed.join(", "));
    }
}
