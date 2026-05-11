use anyhow::Result;

use crate::cli::CurrentArgs;
use crate::resolve;

pub fn run(args: CurrentArgs) -> Result<u8> {
    let cwd = std::env::current_dir()?;
    match resolve::resolve(&cwd)? {
        None => {
            if args.explain {
                println!("(no env active)");
                println!("checked: AENV_OVERRIDE, AENV, .aenv-version walk, global default");
            } else {
                println!("(none)");
            }
            Ok(1)
        }
        Some(r) => {
            if args.explain {
                println!("env:    {}", r.name);
                println!("source: {:?}", r.source);
                if let Some(p) = r.version_file {
                    println!("file:   {}", p.display());
                }
            } else {
                println!("{}", r.name);
            }
            Ok(0)
        }
    }
}
