//! `aenv upgrade` — explicit, user-initiated upgrade of the aenv
//! binary plus its shim symlink/copy.
//!
//! Why this lives at all:
//! - Cargo doesn't track "where I came from", so `cargo install
//!   --force` needs the source URL passed in. This command is the
//!   single place that knows it (we hardcode it; pre-1.0 we ship
//!   from GitHub source).
//! - The currently-running aenv is the OLD binary. If we did the shim
//!   refresh from this process, the shim copy on Windows would be the
//!   old binary bytes — defeating the upgrade. Instead we shell out
//!   to the freshly-installed binary at `~/.cargo/bin/aenv` to do
//!   the refresh.
//!
//! Rejected alternatives (from prior design discussion):
//! - Auto-detect stale state on every aenv launch and self-heal:
//!   cheap but surprising — users may not want an update they didn't
//!   ask for.
//! - In-session upgrade slash command: the new shim model is single-
//!   exec with no supervisor, so a slash command can't trigger a
//!   safe binary-replacement step anyway. Running `aenv upgrade`
//!   from a shell tab is the only consistent path.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

use crate::cli::UpgradeArgs;

const UPGRADE_URL: &str = "https://github.com/wnsdy95/aenv";

pub fn run(args: UpgradeArgs) -> Result<u8> {
    let cargo_bin = cargo_bin_dir()?;
    let new_aenv = cargo_bin.join(if cfg!(windows) { "aenv.exe" } else { "aenv" });

    if args.dry_run {
        println!("aenv upgrade (dry-run):");
        println!("  cargo install --git {UPGRADE_URL} --force --locked");
        println!(
            "  {} init --force --no-guidance --no-default",
            new_aenv.display()
        );
        return Ok(0);
    }

    println!("aenv: upgrading via cargo install --git {UPGRADE_URL} --force --locked");
    let status = std::process::Command::new("cargo")
        .args(["install", "--git", UPGRADE_URL, "--force", "--locked"])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context(
            "failed to spawn `cargo`. Install Rust + cargo, or download a release binary \
             manually and run `aenv init --force` afterwards.",
        )?;
    if !status.success() {
        bail!("cargo install failed (exit code {:?})", status.code());
    }

    if !new_aenv.is_file() {
        // Cargo succeeded but the binary isn't where we expected. Most
        // likely the user has a non-default CARGO_INSTALL_ROOT.
        eprintln!(
            "aenv: cargo install succeeded but no binary found at {}.",
            new_aenv.display()
        );
        eprintln!(
            "      Set CARGO_INSTALL_ROOT or run `aenv init --force` from \
             wherever the new binary landed."
        );
        return Ok(0);
    }

    println!();
    println!("aenv: refreshing shim...");
    // Spawn the NEW binary so the shim copy/symlink reflects the just-
    // installed version. --no-guidance suppresses the shell-wiring
    // guidance which is misleading on a re-run. --no-default skips
    // the default-env seeding (an upgrade shouldn't touch envs).
    let status = std::process::Command::new(&new_aenv)
        .args(["init", "--force", "--no-guidance", "--no-default"])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .with_context(|| format!("failed to spawn {}", new_aenv.display()))?;
    if !status.success() {
        eprintln!(
            "aenv: warning: `aenv init --force` failed (exit {:?}). The binary \
             upgraded but the shim may be stale. Run it manually.",
            status.code()
        );
    }

    warn_if_stale_aenv_in_path(&cargo_bin);

    println!();
    println!("aenv: upgrade complete.");
    Ok(0)
}

/// Resolve the directory where `cargo install` drops binaries.
/// Cargo's rules: `$CARGO_INSTALL_ROOT/bin` if set, else `$CARGO_HOME/bin`,
/// else `$HOME/.cargo/bin`.
fn cargo_bin_dir() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("CARGO_INSTALL_ROOT") {
        return Ok(PathBuf::from(root).join("bin"));
    }
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(home).join("bin"));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not determine $HOME"))?;
    Ok(home.join(".cargo").join("bin"))
}

/// Warn the user if some other `aenv` earlier in PATH would shadow the
/// freshly-installed one. Common case on dev boxes that copy binaries
/// to `~/.local/bin` from an old install — `cargo install` updates
/// `~/.cargo/bin/aenv` but the user's shell still picks up the stale
/// copy. Without this check the next `aenv` invocation runs the old
/// code and the user sees no apparent change.
fn warn_if_stale_aenv_in_path(cargo_bin: &std::path::Path) {
    let Ok(path) = std::env::var("PATH") else {
        return;
    };
    let dirs: Vec<_> = std::env::split_paths(&path).collect();
    let cargo_idx = dirs.iter().position(|d| d == cargo_bin);
    let exe_name = if cfg!(windows) { "aenv.exe" } else { "aenv" };
    for (i, dir) in dirs.iter().enumerate() {
        if Some(i) == cargo_idx {
            return;
        }
        let candidate = dir.join(exe_name);
        if candidate.is_file() {
            eprintln!();
            eprintln!(
                "aenv: warning: stale aenv at {} appears earlier in $PATH than {}.",
                candidate.display(),
                cargo_bin.display()
            );
            eprintln!(
                "       The new binary won't be picked up until you remove the stale \
                 copy or reorder $PATH so {} comes first.",
                cargo_bin.display()
            );
            return;
        }
    }
}
