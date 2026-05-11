//! OS keyring integration for env-scoped secrets, plus `${secret:...}` /
//! `${env:...}` placeholder substitution.
//!
//! Keys live in the OS-native secure store (macOS Keychain / Linux Secret
//! Service / Windows Credential Manager) under service name
//! `aenv:<env-name>:<secret-key>`. They never touch disk in plaintext —
//! manifests carry only `${secret:foo}` references.

use anyhow::{anyhow, bail, Context, Result};
use keyring::Entry;

const SERVICE_PREFIX: &str = "aenv";

pub fn entry_for(env_name: &str, key: &str) -> Result<Entry> {
    validate_secret_key(key)?;
    let svc = format!("{SERVICE_PREFIX}:{env_name}:{key}");
    Entry::new(&svc, env_name).map_err(|e| anyhow!("keyring entry {svc}: {e}"))
}

/// Secret keys must be POSIX identifier-shaped: `[A-Za-z_][A-Za-z0-9_]*`.
/// This is stricter than env/plugin name validation because the key must map
/// 1:1 to the env-var name `AENV_<env>_<KEY>` that bridges the keyring value
/// into the MCP process. Without this restriction, two keys like `gh-token`
/// and `gh.token` would both sanitize to `GH_TOKEN`, causing one MCP to
/// receive another's secret.
pub fn validate_secret_key(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("secret key cannot be empty");
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        bail!("secret key '{key}' must start with a letter or underscore (POSIX env-var rules)");
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            bail!(
                "secret key '{key}' must match [A-Za-z_][A-Za-z0-9_]* — \
                 got '{c}'. Use underscores instead of '-' or '.'."
            );
        }
    }
    Ok(())
}

pub fn set(env_name: &str, key: &str, value: &str) -> Result<()> {
    let e = entry_for(env_name, key)?;
    e.set_password(value)
        .with_context(|| format!("keyring set {SERVICE_PREFIX}:{env_name}:{key}"))?;
    Ok(())
}

pub fn get(env_name: &str, key: &str) -> Result<String> {
    let e = entry_for(env_name, key)?;
    e.get_password()
        .with_context(|| format!("keyring get {SERVICE_PREFIX}:{env_name}:{key}"))
}

pub fn delete(env_name: &str, key: &str) -> Result<()> {
    let e = entry_for(env_name, key)?;
    e.delete_credential()
        .with_context(|| format!("keyring delete {SERVICE_PREFIX}:{env_name}:{key}"))?;
    Ok(())
}

/// Enumerate keys for an env. We track the *names* of stored secrets in a
/// non-secret index file (`<env>/.secrets.list`), since OS keyrings don't
/// reliably support listing by service prefix portably.
pub fn index_path(env: &crate::env::Env) -> std::path::PathBuf {
    env.root.join(".secrets.list")
}

pub fn list(env: &crate::env::Env) -> Result<Vec<String>> {
    let p = index_path(env);
    if !p.exists() {
        return Ok(vec![]);
    }
    let body = std::fs::read_to_string(&p)?;
    Ok(body
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

pub fn remember_key(env: &crate::env::Env, key: &str) -> Result<()> {
    // Hold the global lock so concurrent `aenv secrets add A & aenv secrets
    // add B` don't race on the read-modify-write of `.secrets.list`.
    let _lock = crate::tx::GlobalLock::acquire()?;
    let mut keys = list(env)?;
    if !keys.contains(&key.to_string()) {
        keys.push(key.to_string());
        keys.sort();
    }
    write_index(env, &keys)
}

pub fn forget_key(env: &crate::env::Env, key: &str) -> Result<()> {
    let _lock = crate::tx::GlobalLock::acquire()?;
    let keys: Vec<String> = list(env)?.into_iter().filter(|k| k != key).collect();
    write_index(env, &keys)
}

fn write_index(env: &crate::env::Env, keys: &[String]) -> Result<()> {
    let p = index_path(env);
    let body = keys.join("\n") + "\n";
    crate::paths::write_atomic(&p, body.as_bytes())?;
    crate::paths::lock_down_file(&p)?;
    Ok(())
}

// Note: aenv-specific placeholders in `[mcp.<name>].env` values
// (`${secret:KEY}`, `${env:VAR}`) are not resolved in this module. Instead:
//   - `install::write_mcp_servers` rewrites them to claude-native env-var
//     references at install time (keeps disk plaintext-free).
//   - The per-backend shim (`backend::claude::shim`, `backend::codex::shim`)
//     reads keyring values via `secrets::get` and exports them under
//     `install::aenv_secret_var_name` right before `exec`, so the launched
//     CLI inherits the resolved env vars without plaintext ever touching
//     disk. The legacy supervisor `run_loop` that used to do this was
//     removed when the claude backend collapsed to single-exec.
