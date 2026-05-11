//! Render the env's `[mcp.*]` manifest into codex's `config.toml`.
//!
//! Codex reads `<CODEX_HOME>/config.toml`; the `[mcp_servers.<name>]`
//! tables there describe stdio MCP processes the agent can call. We
//! translate aenv's universal `[mcp.<name>]` table into codex's native
//! shape and write the file in-place. Only stdio servers are emitted —
//! codex doesn't currently consume http/sse transports the way claude
//! does.
//!
//! Idempotent — overwrites every install. Other entries the user may
//! have hand-added at the top level of config.toml are preserved by
//! splicing rather than full replacement.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::env::manifest::Manifest;
use crate::env::Env;

use super::codex_home_for;

#[derive(Serialize)]
struct CodexConfig {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    mcp_servers: BTreeMap<String, McpServer>,
}

#[derive(Serialize)]
struct McpServer {
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

/// Write `<CODEX_HOME>/config.toml` with `mcp_servers` from manifest.
///
/// Filtering: only `[mcp.*]` entries with a `command` (i.e. stdio
/// transport) are forwarded. http/sse-only entries are silently
/// skipped — they'd round-trip into a config codex can't load.
///
/// Secrets: values containing `${secret:KEY}` are passed through
/// verbatim. Phase C ships codex MCP rendering without per-launch
/// keychain bridging; users with secret-bearing MCPs either set the
/// corresponding `AENV_<env>_<KEY>` env var themselves or hand-edit
/// codex's config.toml after install. A keychain → codex env-var
/// bridge in the codex shim is tracked for a Phase C+ patch — keeping
/// it out of 0.3.0 avoids landing secret values on disk before we've
/// verified codex's env-expansion semantics in `[mcp_servers.*].env`.
pub fn write_config_toml(env: &Env, manifest: &Manifest) -> Result<()> {
    let mut servers = BTreeMap::new();
    for (name, spec) in &manifest.mcp {
        let Some(command) = &spec.command else {
            continue;
        };
        servers.insert(
            name.clone(),
            McpServer {
                command: command.clone(),
                args: spec.args.clone(),
                env: spec.env.clone(),
            },
        );
    }

    let codex_home = codex_home_for(env);
    crate::paths::ensure_dir(&codex_home)?;
    let config_path = codex_home.join("config.toml");

    let cfg = CodexConfig {
        mcp_servers: servers,
    };
    let body = toml::to_string_pretty(&cfg).context("serialize codex config.toml")?;
    crate::paths::write_atomic(&config_path, body.as_bytes())
        .with_context(|| format!("write {}", config_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::manifest::{EnvMeta, McpSpec, PluginsBlock, SkillsBlock, SCHEMA_VERSION};

    fn fake_env(tmp: &std::path::Path) -> Env {
        Env {
            name: "x".into(),
            root: tmp.to_path_buf(),
            manifest_override: None,
        }
    }

    fn manifest_with_mcp(mcp: BTreeMap<String, McpSpec>) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION.to_string(),
            env: EnvMeta {
                name: "x".into(),
                description: None,
                compat: BTreeMap::new(),
                created: None,
            },
            platforms: crate::env::manifest::PlatformsBlock::default(),
            mcp,
            plugins: PluginsBlock::default(),
            skills: SkillsBlock::default(),
            hooks: crate::env::manifest::Hooks::default(),
        }
    }

    #[test]
    fn writes_stdio_mcp_servers_into_config_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "github".into(),
            McpSpec {
                transport: None,
                command: Some("npx".into()),
                args: vec!["-y".into(), "@scope/server-github".into()],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                version: None,
            },
        );
        let m = manifest_with_mcp(mcp);
        write_config_toml(&env, &m).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("codex").join("config.toml")).unwrap();
        assert!(body.contains("[mcp_servers.github]"), "{body}");
        assert!(body.contains("command = \"npx\""), "{body}");
    }

    #[test]
    fn skips_http_only_mcp_entries() {
        // codex's mcp_servers schema is stdio-only. An http-only entry
        // (no `command`) must not appear in the rendered config or
        // codex would reject the file at startup.
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let mut mcp = BTreeMap::new();
        mcp.insert(
            "remote".into(),
            McpSpec {
                transport: Some("http".into()),
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                url: Some("https://example.com".into()),
                headers: BTreeMap::new(),
                version: None,
            },
        );
        let m = manifest_with_mcp(mcp);
        write_config_toml(&env, &m).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("codex").join("config.toml")).unwrap();
        assert!(
            !body.contains("[mcp_servers.remote]"),
            "http-only entry leaked into stdio config: {body}"
        );
    }

    #[test]
    fn empty_mcp_block_writes_empty_file_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let m = manifest_with_mcp(BTreeMap::new());
        write_config_toml(&env, &m).unwrap();
        let path = tmp.path().join("codex").join("config.toml");
        assert!(path.is_file());
        write_config_toml(&env, &m).unwrap();
        assert!(path.is_file());
    }
}
