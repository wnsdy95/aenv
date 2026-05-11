use anyhow::{anyhow, bail, Result};

use crate::cli::ImportGlobalArgs;
use crate::env::{self, Env};
use crate::resolve::GlobalConfig;
use crate::tx;

/// Copy the user's existing `~/.claude/` into an env's `.claude/` so that
/// plugins, skills, MCPs, settings, and (optionally) sessions become available
/// inside the env's isolated CLAUDE_CONFIG_DIR.
pub fn run(args: ImportGlobalArgs) -> Result<u8> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve $HOME"))?;
    let global_claude = home.join(".claude");
    if !global_claude.is_dir() {
        bail!("~/.claude does not exist — nothing to import");
    }

    let name = args.name.clone();
    let force = args.force;
    let env_root = crate::paths::env_dir(&name)?;

    let env = tx::with_tx(
        "import-global",
        Some(&name),
        std::slice::from_ref(&env_root),
        Some(format!("import ~/.claude into env '{name}'")),
        || -> anyhow::Result<Env> {
            let env = match Env::open(&name) {
                Ok(_) => {
                    if !force {
                        bail!("env '{name}' already exists. pass --force to overwrite.");
                    }
                    Env::remove(&name)?;
                    Env::create(&name, true)?
                }
                Err(_) => Env::create(&name, true)?,
            };
            let dst = env.claude_dir();
            if dst.exists() {
                std::fs::remove_dir_all(&dst).ok();
            }
            env::copy_tree(&global_claude, &dst)?;
            // copy_tree drops the locked-down mode bits Env::create set on
            // the freshly-created .claude dir. Reapply.
            crate::paths::lock_down_dir(&dst)?;
            let settings = dst.join("settings.json");
            if settings.is_file() {
                crate::paths::lock_down_file(&settings)?;
            }
            Ok(env)
        },
    )?;

    let dst = env.claude_dir();

    println!(
        "imported {} -> {} ({})",
        global_claude.display(),
        dst.display(),
        env.name
    );

    if args.set_default {
        let mut cfg = GlobalConfig::load().unwrap_or_default();
        cfg.default_env = Some(env.name.clone());
        cfg.save()?;
        println!("global default env -> {}", env.name);
    }

    Ok(0)
}
