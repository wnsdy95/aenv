use anyhow::Result;

use crate::cli::ListArgs;
use crate::env::Env;
use crate::resolve;

pub fn run(args: ListArgs) -> Result<u8> {
    let envs = Env::list()?;
    if envs.is_empty() {
        println!("(no envs)  hint: aenv new <name>");
        return Ok(0);
    }
    let active = resolve::resolve(&std::env::current_dir()?)
        .ok()
        .flatten()
        .map(|r| r.name);
    for e in envs {
        let marker = if Some(&e.name) == active.as_ref() {
            "*"
        } else if e.broken {
            "!"
        } else {
            " "
        };
        if args.long {
            let suffix = if e.broken {
                " [broken: bad/missing manifest]"
            } else {
                ""
            };
            println!(
                "{} {:<20} {}  {}{}",
                marker,
                e.name,
                e.root.display(),
                e.description.unwrap_or_default(),
                suffix
            );
        } else if e.broken {
            println!("! {} (broken)", e.name);
        } else {
            println!("{marker} {}", e.name);
        }
    }
    Ok(0)
}
