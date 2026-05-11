use anyhow::Result;

use crate::cli::{IflArgs, NewArgs};
use crate::env::Env;
use crate::tx::GlobalLock;

pub fn run(args: NewArgs) -> Result<u8> {
    // Hold the global lock so `aenv new` doesn't race with a concurrent
    // `aenv install`/`rollback`/`secrets add` on a different env (or the same
    // one). Cheap — just a flock(2) on ~/.aenv/.lock.
    let chain_ifl = args.ifl;
    let env = {
        let _lock = GlobalLock::acquire()?;
        if let Some(src) = args.from {
            let parent = Env::open(&src)?;
            Env::clone_from(&args.name, &parent)?
        } else {
            Env::create(&args.name, args.bare)?
        }
    };
    println!("created env '{}' at {}", env.name, env.root.display());

    // `--ifl` chains directly into the import TUI with the new env as
    // the target. The lock is released before invoking ifl::run so it
    // can re-acquire (ifl is a separate transactional unit — failure
    // there leaves the new env in place but un-imported).
    if chain_ifl {
        let ifl_args = IflArgs {
            env: Some(env.name.clone()),
            from_env: Vec::new(),
            plugins: Vec::new(),
            skills: Vec::new(),
            mcps: Vec::new(),
            force_tty: false,
        };
        return crate::commands::ifl::run(ifl_args);
    }
    Ok(0)
}
