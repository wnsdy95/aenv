use anyhow::{bail, Result};

use crate::backend::claude::shim;
use crate::backend::codex;
use crate::cli::WhichArgs;
use crate::env::Env;
use crate::paths;

pub fn run(args: WhichArgs) -> Result<u8> {
    match args.target.as_str() {
        "home" => {
            println!("{}", paths::aenv_home()?.display());
        }
        "shim" => {
            println!(
                "{}",
                paths::shims_dir()?
                    .join(shim::claude_binary_name())
                    .display()
            );
        }
        "claude" => {
            println!("{}", shim::locate_real_claude()?.display());
        }
        "codex" => {
            println!("{}", codex::locate_real_codex()?.display());
        }
        "env" => {
            let name = args
                .arg
                .ok_or_else(|| anyhow::anyhow!("which env <name>"))?;
            let e = Env::open(&name)?;
            println!("{}", e.root.display());
        }
        other => bail!("unknown target '{other}' (try: env <name> | claude | codex | shim | home)"),
    }
    Ok(0)
}
