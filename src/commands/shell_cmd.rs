use anyhow::{bail, Result};

use crate::cli::ShellArgs;
use crate::shell;

pub fn run(args: ShellArgs) -> Result<u8> {
    let script = match args.shell.as_str() {
        "bash" => shell::bash::script(),
        "zsh" => shell::zsh::script(),
        "fish" => shell::fish::script(),
        other => bail!("unsupported shell: {other} (supported: bash, zsh, fish)"),
    };
    print!("{script}");
    Ok(0)
}
