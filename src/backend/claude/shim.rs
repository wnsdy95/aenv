//! Claude Code shim entrypoint.
//!
//! When the binary is invoked as `claude` (via shim symlink in PATH),
//! this resolves the active env, sets `CLAUDE_CONFIG_DIR=<env>/.claude`
//! plus per-env XDG roots, bridges `${secret:KEY}` references in the
//! manifest into `AENV_<env>_<KEY>` environment variables (read from
//! the OS keyring), and `exec()`s the real claude.
//!
//! This is the same shape as the codex shim — one env-var redirect
//! and one exec, no supervisor restart loop, no overlay flags.
//! Trade-off: each env is fully isolated (its own auth, plugins,
//! settings, sessions) but starts unauthenticated → user runs `/login`
//! once per env. Verified against Claude Code 2.1.138: `CLAUDE_CONFIG_DIR`
//! routes the entire config root, and plugin cache plus
//! `installed_plugins.json` follow it (`dW()` and `n6()` in the
//! bundled JS), so per-env plugin/MCP/session isolation drops out for
//! free.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::env::Env;
use crate::error::AenvError;
use crate::paths;
use crate::resolve;

pub fn run(args: Vec<String>) -> Result<u8> {
    let cwd = std::env::current_dir().context("getcwd")?;
    let resolved = resolve::resolve(&cwd)?;

    let real_claude = locate_real_claude()?;

    // If `.aenv-version` (or another resolution source) names a missing env,
    // don't lock the user out of claude entirely — fall back to system claude
    // with a one-line warning. This prevents broken pins from being a footgun.
    //
    // Project mode (`aenv.toml` walk-up): slot is auto-created lazily via
    // `Env::open_at_project`, so this branch never hits "not found" for it
    // — only `.aenv-version` / global default referencing a missing env can.
    let env: Option<Env> = match resolved {
        Some(r) => match crate::env::open_resolved(&r) {
            Ok(env) => Some(env),
            Err(_) => {
                let hint = match (&r.project_manifest, &r.version_file) {
                    (Some(p), _) => format!(" (from {})", p.display()),
                    (_, Some(p)) => format!(" (from {})", p.display()),
                    _ => String::new(),
                };
                eprintln!(
                    "aenv: warning: env '{}' is configured{} but not found. \
                     Running unisolated. Run `aenv list` to see available envs.",
                    r.name, hint
                );
                None
            }
        },
        None => None,
    };

    exec_claude(&real_claude, &args, env.as_ref())
}

#[cfg(unix)]
fn exec_claude(real: &std::path::Path, args: &[String], env: Option<&Env>) -> Result<u8> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(real);
    cmd.args(args);
    apply_env(&mut cmd, env)?;
    let err = cmd.exec();
    Err(anyhow::anyhow!("exec {} failed: {err}", real.display()))
}

#[cfg(windows)]
fn exec_claude(real: &std::path::Path, args: &[String], env: Option<&Env>) -> Result<u8> {
    let mut cmd = std::process::Command::new(real);
    cmd.args(args);
    apply_env(&mut cmd, env)?;
    let status = cmd
        .status()
        .with_context(|| format!("spawn {}", real.display()))?;
    Ok(status.code().unwrap_or(1) as u8)
}

/// Set every env var Claude Code needs to land in the active env, and
/// export keyring-resolved secret values for any `${secret:KEY}` MCP
/// refs the manifest declares. Also fires the env's pre_activate hook
/// (if any) — same contract the legacy supervisor honored, kept here
/// so committed user hooks survive the rename.
///
/// Special-case for the reserved `global` env: skip everything. The
/// user wants their real ~/.claude, so we explicitly *unset* any
/// inherited `CLAUDE_CONFIG_DIR` (otherwise a stale value from a
/// parent session leaks through), tag with AENV breadcrumbs, and
/// hand off to claude. No XDG redirection, no manifest hook, no
/// secret bridging — global has no aenv.toml.
///
/// Public so `aenv exec` and `aenv run` route through the exact same
/// launch shape the shim uses; otherwise those entry points would
/// drift on each shim change (secret bridging, hook gating, global
/// special-casing) and tests pinning shim behavior wouldn't transfer.
pub fn apply_env(cmd: &mut std::process::Command, env: Option<&Env>) -> Result<()> {
    if let Some(env) = env {
        if crate::env::is_global(&env.name) {
            cmd.env_remove("CLAUDE_CONFIG_DIR");
            cmd.env("AENV", &env.name);
            cmd.env("AENV_ACTIVE", &env.name);
            return Ok(());
        }
        let spec = EnvSpec::from_env_strict(env)?;
        spec.ensure_dirs()?;
        let mut env_vars: Vec<(OsString, OsString)> = Vec::new();
        spec.apply(&mut env_vars);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
        for (k, v) in claude_secret_env_vars(env)? {
            cmd.env(k, v);
        }
        crate::backend::common::run_pre_activate(env)?;
    }
    Ok(())
}

/// Per-env path bundle for full isolation: claude reads its config from
/// `<env>/.claude/`, sessions land there, the keychain entry is hashed
/// from that path, and plugin cache (`<env>/.claude/plugins/`) is
/// per-env. Same shape `aenv exec` already used; now the default shim
/// goes through this too.
#[derive(Debug, Clone)]
pub struct EnvSpec {
    pub name: String,
    pub claude_config_dir: PathBuf,
    pub xdg_config: PathBuf,
    pub xdg_data: PathBuf,
    pub xdg_state: PathBuf,
    pub xdg_cache: PathBuf,
}

impl EnvSpec {
    pub fn from_env_strict(e: &Env) -> Result<Self> {
        Ok(Self {
            name: e.name.clone(),
            claude_config_dir: e.claude_dir(),
            xdg_config: e.xdg(paths::XdgKind::Config),
            xdg_data: e.xdg(paths::XdgKind::Data),
            xdg_state: e.xdg(paths::XdgKind::State),
            xdg_cache: e.xdg(paths::XdgKind::Cache),
        })
    }

    /// `<env>/.claude/` and the four XDG roots must exist before exec —
    /// claude assumes them present and bails on first read. Created
    /// lazily here so a project-mode slot that's still cold doesn't
    /// trip the user up.
    pub fn ensure_dirs(&self) -> Result<()> {
        paths::ensure_dir(&self.claude_config_dir)?;
        paths::ensure_dir(&self.xdg_config)?;
        paths::ensure_dir(&self.xdg_data)?;
        paths::ensure_dir(&self.xdg_state)?;
        paths::ensure_dir(&self.xdg_cache)?;
        Ok(())
    }

    pub fn apply(&self, cmd_env: &mut Vec<(OsString, OsString)>) {
        cmd_env.push((
            "CLAUDE_CONFIG_DIR".into(),
            self.claude_config_dir.clone().into(),
        ));
        cmd_env.push(("XDG_CONFIG_HOME".into(), self.xdg_config.clone().into()));
        cmd_env.push(("XDG_DATA_HOME".into(), self.xdg_data.clone().into()));
        cmd_env.push(("XDG_STATE_HOME".into(), self.xdg_state.clone().into()));
        cmd_env.push(("XDG_CACHE_HOME".into(), self.xdg_cache.clone().into()));
        cmd_env.push(("AENV".into(), self.name.clone().into()));
        cmd_env.push(("AENV_ACTIVE".into(), self.name.clone().into()));
    }
}

/// Resolve `${secret:KEY}` placeholders in the env's manifest mcp.env
/// values into `AENV_<env>_<KEY>` env-var entries with the keyring
/// value. Returns each unique var only once. A missing keychain entry
/// is logged and skipped — claude will then expand the unresolved
/// `${AENV_..._...}` reference in `settings.json` to nothing, which is
/// the same failure mode the user saw under the legacy supervisor
/// path.
pub fn claude_secret_env_vars(env: &Env) -> Result<Vec<(OsString, OsString)>> {
    use std::collections::HashSet;
    let manifest = match env.manifest() {
        Ok(m) => m,
        Err(_) => return Ok(Vec::new()),
    };
    let mut exported: HashSet<String> = HashSet::new();
    let mut out: Vec<(OsString, OsString)> = Vec::new();
    for (mcp_name, mcp) in &manifest.mcp {
        for (k, raw) in &mcp.env {
            for secret_key in scan_secret_keys(raw) {
                let var = crate::install::aenv_secret_var_name(&env.name, secret_key);
                if !exported.insert(var.clone()) {
                    continue;
                }
                match crate::secrets::get(&env.name, secret_key) {
                    Ok(value) => out.push((var.into(), value.into())),
                    Err(e) => eprintln!(
                        "aenv: warning: mcp.{mcp_name}.env.{k} \
                         references unresolvable secret '{secret_key}': {e}"
                    ),
                }
            }
        }
    }
    Ok(out)
}

/// Find every `${secret:KEY}` reference in `input` and return the KEYs.
/// Handles standalone refs and refs embedded in literals (e.g. `Bearer ${secret:gh}`).
fn scan_secret_keys(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = input[i + 2..].find('}') {
                let inner = &input[i + 2..i + 2 + end];
                if let Some(key) = inner.strip_prefix("secret:") {
                    out.push(key);
                }
                i = i + 2 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The on-disk filename of the claude binary on this platform.
pub fn claude_binary_name() -> &'static str {
    if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    }
}

/// Find the real claude binary, preferring the one resolved via the
/// current shell's `$PATH` (so the user always runs the same claude
/// they'd run by typing `claude` in any other shell). The cached
/// `~/.aenv/config.toml`'s `real_claude` is a fallback for installs
/// that drop the binary in a non-PATH directory (e.g. macOS's
/// `Library/Application Support/Claude/.../claude`).
///
/// PATH-first ordering (instead of cache-first) is what makes
/// upgrades safe: if the user originally installed claude under
/// `Library/...` (cached) and later installed a newer version visible
/// via PATH (`~/.local/bin/claude`, `npm i -g`, brew, etc.), we want
/// to honor the PATH version. Cache-first would silently keep
/// launching the old binary forever.
pub fn locate_real_claude() -> Result<PathBuf> {
    let shims = paths::shims_dir().ok();
    if let Some(path) = std::env::var_os("PATH") {
        let target_name = claude_binary_name();
        for dir in std::env::split_paths(&path) {
            if let Some(s) = &shims {
                if dir == *s {
                    continue;
                }
            }
            let candidate = dir.join(target_name);
            if candidate.is_file() && is_executable(&candidate) && !is_aenv_shim(&candidate) {
                return Ok(candidate);
            }
        }
    }
    if let Ok(cfg) = resolve::GlobalConfig::load() {
        if let Some(p) = cfg.real_claude {
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(AenvError::RealClaudeNotFound.into())
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
        && p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
}

fn is_aenv_shim(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    if let Ok(target) = std::fs::read_link(p) {
        let target_str = target.to_string_lossy();
        if target_str.contains("/aenv") || target_str.ends_with("aenv") {
            return true;
        }
    }
    let _ = p;
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_standalone_secret() {
        assert_eq!(scan_secret_keys("${secret:foo}"), vec!["foo"]);
    }

    #[test]
    fn scan_finds_embedded_secret() {
        assert_eq!(scan_secret_keys("Bearer ${secret:gh}"), vec!["gh"]);
    }

    #[test]
    fn scan_ignores_other_namespaces() {
        let r = scan_secret_keys("${env:HOME}-${literal:x}-${secret:k}");
        assert_eq!(r, vec!["k"]);
    }

    #[test]
    fn scan_no_placeholders() {
        assert!(scan_secret_keys("plain").is_empty());
    }
}
