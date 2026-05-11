use anyhow::Result;

use crate::cli::UseArgs;
use crate::env::Env;
use crate::paths;
use crate::resolve::GlobalConfig;

pub fn run(args: UseArgs) -> Result<u8> {
    let _lock = crate::tx::GlobalLock::acquire()?;
    // Validate env exists while holding the lock so it cannot disappear
    // between resolution and pin write.
    Env::open(&args.name)?;

    if args.global {
        // Propagate parse errors instead of silently defaulting (which would
        // clobber a corrupted config with a fresh one and erase the user's
        // real_claude cache).
        let mut cfg = GlobalConfig::load()?;
        cfg.default_env = Some(args.name.clone());
        cfg.save()?;
        println!("global default env -> {}", args.name);
    } else {
        let cwd = std::env::current_dir()?;
        let f = cwd.join(".aenv-version");
        paths::write_atomic(&f, format!("{}\n", args.name).as_bytes())?;
        println!("pinned {} -> {}", f.display(), args.name);
    }

    // Heads-up when the user is already inside an active claude / codex
    // session: the running process already loaded its config dir, so
    // the new pin only takes effect after exit + relaunch.
    if std::env::var_os("AENV_ACTIVE").is_some() {
        eprintln!(
            "aenv: hint: pin written. Exit this CLI and relaunch (`exit` then \
             `claude` or `codex`) to switch into '{}'.",
            args.name
        );
    }
    Ok(0)
}
