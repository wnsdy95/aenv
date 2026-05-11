use std::io::{BufRead, IsTerminal, Write};

use anyhow::{bail, Result};

use crate::cli::{SecretsArgs, SecretsCommand};
use crate::env::open_or_active;
use crate::secrets;

fn ensure_not_global(env: &crate::env::Env) -> Result<()> {
    if crate::env::is_global(&env.name) {
        bail!(
            "'global' aliases the user's real ~/.claude — secrets must live in a \
             real aenv env so they round-trip the keyring lookup. Pick or create \
             one (`aenv list`, `aenv new <name>`)."
        );
    }
    Ok(())
}

pub fn run(args: SecretsArgs) -> Result<u8> {
    match args.command {
        SecretsCommand::Add { key, value, env } | SecretsCommand::Rotate { key, value, env } => {
            let env_obj = open_or_active(env.as_deref())?;
            ensure_not_global(&env_obj)?;
            let v = match value {
                Some(v) => v,
                None => prompt_secret(&format!("value for {key}"))?,
            };
            secrets::set(&env_obj.name, &key, &v)?;
            secrets::remember_key(&env_obj, &key)?;
            println!("stored secret '{key}' for env '{}'", env_obj.name);
        }
        SecretsCommand::List { env } => {
            let env_obj = open_or_active(env.as_deref())?;
            ensure_not_global(&env_obj)?;
            let keys = secrets::list(&env_obj)?;
            if keys.is_empty() {
                println!("(no secrets in env '{}')", env_obj.name);
            } else {
                for k in keys {
                    println!("{k}");
                }
            }
        }
        SecretsCommand::Remove { key, env } => {
            let env_obj = open_or_active(env.as_deref())?;
            ensure_not_global(&env_obj)?;
            secrets::delete(&env_obj.name, &key)?;
            secrets::forget_key(&env_obj, &key)?;
            println!("removed secret '{key}' from env '{}'", env_obj.name);
        }
    }
    Ok(0)
}

fn prompt_secret(label: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("{label}: ");
        std::io::stderr().flush().ok();
    }
    let mut buf = String::new();
    stdin.lock().read_line(&mut buf)?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_string())
}
