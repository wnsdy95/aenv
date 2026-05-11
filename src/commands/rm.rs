use anyhow::{bail, Result};

use crate::cli::RmArgs;
use crate::env::manifest::{PluginRef, SkillRef};
use crate::env::open_or_active;

pub fn run(args: RmArgs) -> Result<u8> {
    // No outer GlobalLock — `tx::with_tx` below owns the flock.
    // Holding it twice in the same process deadlocks
    // `Transaction::begin`. The manifest read-modify-write also
    // happens INSIDE the tx closure (= under the flock) so a
    // concurrent add/rm/ifl can't be lost between our load and
    // save — see the analogous note in `commands/add.rs`.
    let env = open_or_active(args.env.as_deref())?;
    if crate::env::is_global(&env.name) {
        bail!("'global' is reserved (alias for ~/.claude); no aenv.toml to mutate.");
    }
    if !matches!(args.kind.as_str(), "mcp" | "plugin" | "skill") {
        bail!("unknown kind '{}'", args.kind);
    }

    let env_name = env.name.clone();
    let manifest_path = env.manifest_write_path();
    let captures = vec![
        manifest_path.clone(),
        env.claude_dir().join("plugins"),
        env.claude_dir().join("skills"),
        env.claude_dir().join("settings.json"),
        env.lockfile_path(),
    ];
    let kind = args.kind.clone();
    let name = args.name.clone();
    let kind_for_closure = kind.clone();
    let name_for_closure = name.clone();
    crate::tx::with_tx(
        "rm",
        Some(&env_name),
        &captures,
        Some(format!("aenv rm {kind} {name} env={env_name}")),
        move || {
            let mut manifest = env.manifest()?;
            let removed = match kind_for_closure.as_str() {
                "mcp" => manifest.mcp.remove(&name_for_closure).is_some(),
                "plugin" => {
                    let before = manifest.plugins.enabled.len();
                    manifest.plugins.enabled.retain(|p| match p {
                        PluginRef::Detailed(s) => s.name != name_for_closure,
                        PluginRef::Short(s) => {
                            s.split('@').next() != Some(name_for_closure.as_str())
                        }
                    });
                    manifest.plugins.enabled.len() < before
                }
                "skill" => {
                    let before = manifest.skills.enabled.len();
                    manifest.skills.enabled.retain(|s| match s {
                        SkillRef::Detailed(x) => x.name != name_for_closure,
                        SkillRef::Short(s) => s != &name_for_closure,
                    });
                    manifest.skills.enabled.len() < before
                }
                // Validated above the closure.
                other => bail!("unknown kind '{other}'"),
            };
            if !removed {
                bail!("'{name_for_closure}' not found in manifest");
            }
            manifest.save_to(&manifest_path)?;
            crate::install::apply_live(&env)?;
            Ok(())
        },
    )?;
    println!("removed {kind} {name} (applied to {env_name}).");
    Ok(0)
}
