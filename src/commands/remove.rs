use anyhow::{bail, Result};

use crate::cli::RemoveArgs;
use crate::env::Env;
use crate::tx::GlobalLock;

pub fn run(args: RemoveArgs) -> Result<u8> {
    // Reject the reserved `global` alias before the confirmation
    // prompt — otherwise the user would see `[y/N]` for an env we'd
    // refuse to delete anyway.
    if crate::env::is_global(&args.name) {
        bail!(
            "'global' is reserved (alias for ~/.claude); refusing to remove. \
             The user's real ~/.claude is not aenv-managed."
        );
    }
    if !args.force {
        eprint!("remove env '{}' permanently? [y/N] ", args.name);
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut buf = String::new();
        // If stdin can't be read (closed/non-tty), treat as no-confirm — abort.
        if std::io::stdin().read_line(&mut buf).is_err() {
            bail!("aborted (could not read confirmation)");
        }
        if !matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("aborted");
        }
    }
    let _lock = GlobalLock::acquire()?;
    Env::remove(&args.name)?;
    println!("removed env '{}'", args.name);
    Ok(0)
}
