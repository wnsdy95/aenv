use anyhow::{anyhow, bail, Context, Result};

use crate::cli::ImportProfileArgs;
use crate::env::manifest::Manifest;
use crate::env::Env;
use crate::paths;
use crate::store::source::unpack_tarball;
use crate::tx;

pub fn run(args: ImportProfileArgs) -> Result<u8> {
    // Stage into a temp dir using the same safe extractor the plugin store
    // uses — rejects archive symlinks/hardlinks, absolute paths, and `..`.
    // A naive tar_reader.unpack() here would let a crafted bundle plant
    // `.claude/plugins -> /tmp/target` and escape the env on later install.
    let tmp = tempfile::tempdir()?;
    unpack_tarball(&args.file, tmp.path())
        .with_context(|| format!("safe-extract {}", args.file.display()))?;

    // Manifest is required.
    let manifest_path = tmp.path().join("aenv.toml");
    if !manifest_path.is_file() {
        bail!("bundle missing aenv.toml");
    }
    let body = std::fs::read_to_string(&manifest_path)?;
    let mut manifest: Manifest = toml::from_str(&body).context("parse imported aenv.toml")?;
    manifest.validate()?;

    // Hook RCE gate: an imported `hooks.pre_activate` command would run
    // `sh -c <command>` on every claude launch in the imported env. Anyone
    // who can hand the user a bundle could ship code execution. Require an
    // explicit `--trust-hooks` to acknowledge the risk. If not granted, we
    // strip the hook before persisting (rest of the bundle is still imported).
    if let Some(cmd) = manifest.hooks.pre_activate.clone() {
        if !args.trust_hooks {
            eprintln!(
                "aenv: warning: bundle declares hooks.pre_activate:\n\
                 \n    {cmd}\n\n\
                 This would run on every claude launch. Stripping it. \
                 Re-run with --trust-hooks to keep it."
            );
            manifest.hooks.pre_activate = None;
        }
    }

    let target_name = args.name.unwrap_or_else(|| manifest.env.name.clone());
    crate::env::validate_name(&target_name)?;
    manifest.env.name = target_name.clone();
    let dst = paths::env_dir(&target_name)?;

    // Defense in depth: verify the staged tree contains no symlinks (the safe
    // tar extractor already skips them, but a future tar-format change or a
    // bundle produced by a different tool could slip one through). copy_tree
    // would otherwise replicate the symlink and escape the env root.
    reject_symlinks(tmp.path())
        .context("imported bundle contains symlinks; refusing for safety")?;

    let force = args.force;
    let staged = tmp.path().to_path_buf();

    tx::with_tx(
        "import-profile",
        Some(&target_name),
        std::slice::from_ref(&dst),
        Some(format!("import bundle into env '{target_name}'")),
        || -> Result<()> {
            if dst.exists() && !force {
                return Err(anyhow!(
                    "env '{target_name}' exists; pass --force to overwrite"
                ));
            }
            if dst.exists() {
                std::fs::remove_dir_all(&dst)?;
            }
            paths::ensure_dir(dst.parent().unwrap())?;
            crate::env::copy_tree(&staged, &dst)?;
            // Re-write manifest with possibly renamed env.
            manifest.save(&dst)?;
            // Re-create XDG dirs (excluded from bundle).
            for kind in [
                paths::XdgKind::Config,
                paths::XdgKind::Data,
                paths::XdgKind::State,
                paths::XdgKind::Cache,
            ] {
                paths::ensure_dir(&paths::env_xdg_dir(&target_name, kind)?)?;
            }
            // copy_tree replicates source mode bits, which for an exported
            // bundle from a different user can leak ownership patterns. Snap
            // back to the same owner-only invariant `Env::create` enforces.
            paths::lock_down_dir(&dst)?;
            paths::lock_down_dir(&dst.join(".claude"))?;
            let settings = dst.join(".claude/settings.json");
            if settings.is_file() {
                paths::lock_down_file(&settings)?;
            }
            Ok(())
        },
    )?;

    let _ = Env::open(&target_name)?;
    println!("imported env '{}'. Next:", target_name);
    println!("  aenv install -E {target_name}    # materialize plugins/skills");
    println!("  aenv secrets list -E {target_name}    # add any required secrets");
    Ok(0)
}

/// Recursively reject any symlink under `root`. The safe tar extractor
/// already skips symlink entries; this is defense in depth.
fn reject_symlinks(root: &std::path::Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.context("walk staged tree")?;
        if entry.file_type().is_symlink() {
            bail!("symlink in bundle at {}", entry.path().display());
        }
    }
    Ok(())
}
