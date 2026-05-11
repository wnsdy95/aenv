use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use walkdir::WalkDir;

use crate::cli::ExportProfileArgs;
use crate::env::open_or_active;

/// Excluded directories (per env) — secrets, transient state, downloaded plugin
/// content (re-fetchable from lockfile), oauth tokens, sessions, cache.
const EXCLUDED_PREFIXES: &[&str] = &[
    ".secrets.list",
    ".claude/plugins/",
    ".claude/skills/",
    ".claude/sessions/",
    ".claude/cache/",
    ".claude/downloads/",
    ".claude/file-history/",
    ".claude/paste-cache/",
    ".claude/backups/",
    ".claude/.credentials.json",
    ".claude/.credentials.json.bak",
    "xdg/cache/",
    "xdg/state/",
];

pub fn run(args: ExportProfileArgs) -> Result<u8> {
    let env = open_or_active(args.env.as_deref())?;
    if crate::env::is_global(&env.name) {
        // `global.root` is `$HOME` (it's the alias for the user's
        // real ~/.claude / ~/.codex). Walking that and tarring up the
        // result would archive the whole home directory — secrets,
        // SSH keys, dotfiles, everything. Hard refuse rather than
        // adding an opt-out flag people can flip by accident.
        bail!(
            "'global' aliases the user's real ~/.claude — there is no aenv-managed \
             tree to export. Pick a real env (`aenv list`)."
        );
    }
    let out: PathBuf = match args.output {
        Some(p) => p,
        None => {
            let cwd = std::env::current_dir().context("getcwd for output path default")?;
            cwd.join(format!("{}.aenv.tar.gz", env.name))
        }
    };

    // If the user picked an output path inside the env, the walker would
    // see the bundle file mid-write and append it to itself — corrupt
    // bundle, possibly unbounded growth. Reject up front. Compare canonical
    // forms so symlink trickery doesn't bypass.
    let out_abs = if out.is_absolute() {
        out.clone()
    } else {
        std::env::current_dir()
            .context("getcwd for output absolute")?
            .join(&out)
    };
    let env_abs = env.root.canonicalize().unwrap_or_else(|_| env.root.clone());
    let out_parent = out_abs
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| out_abs.clone());
    let out_parent_canon = out_parent
        .canonicalize()
        .unwrap_or_else(|_| out_parent.clone());
    if out_parent_canon.starts_with(&env_abs) {
        anyhow::bail!(
            "output path {} is inside the env root {}; pick a path outside the env to avoid \
             archiving the bundle into itself",
            out_abs.display(),
            env_abs.display()
        );
    }

    let f = std::fs::File::create(&out).with_context(|| format!("create {}", out.display()))?;
    let gz = GzEncoder::new(f, Compression::default());
    let mut tar = tar::Builder::new(gz);

    let env_root = &env.root;
    for entry in WalkDir::new(env_root) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(env_root)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel_str = rel.to_string_lossy();
        if EXCLUDED_PREFIXES
            .iter()
            .any(|p| rel_str == *p || rel_str.starts_with(p))
        {
            continue;
        }
        if entry.file_type().is_dir() {
            continue;
        }
        if entry.file_type().is_file() {
            let mut f = std::fs::File::open(entry.path())?;
            tar.append_file(rel, &mut f)?;
        }
    }
    tar.into_inner()?.finish()?;
    println!("exported {} -> {}", env.name, out.display());
    println!("note: secrets/sessions/cache excluded. Imports must re-add secrets.");
    Ok(0)
}
