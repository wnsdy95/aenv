use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// How an env was resolved (for /aenv current --explain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// $AENV_OVERRIDE
    Override,
    /// $AENV (long-lived shell-set)
    EnvVar,
    /// `aenv.toml` walk-up — project-local manifest (highest non-env precedence)
    ProjectManifest,
    /// `.aenv-version` walk-up — bare-name pin
    VersionFile,
    /// global config default
    GlobalDefault,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolved {
    /// User-facing label. For project-mode, this is the manifest's
    /// `[env].name`; for everything else it equals `slot`.
    pub name: String,
    /// On-disk env slot under `~/.aenv/envs/<slot>/`. Equals `name` for
    /// global envs; for project envs it is `<name>-<sha8(project-path)>`
    /// to keep two clones of the same repo from sharing materialized state.
    pub slot: String,
    pub source: Source,
    /// Path to the matched `.aenv-version` file (only set for VersionFile).
    pub version_file: Option<PathBuf>,
    /// Path to the matched `aenv.toml` file (only set for ProjectManifest).
    pub project_manifest: Option<PathBuf>,
}

impl Resolved {
    /// Convenience constructor for non-project sources.
    fn bare(name: String, source: Source) -> Self {
        let slot = name.clone();
        Self {
            name,
            slot,
            source,
            version_file: None,
            project_manifest: None,
        }
    }
}

/// Precedence:
///   1. $AENV_OVERRIDE         (one-shot, higher than persistent)
///   2. $AENV                   (persistent, e.g. exported by activate)
///   3. aenv.toml walk-up       (project-local manifest)
///   4. .aenv-version walk-up
///   5. global config default   (~/.aenv/config.toml)
///   6. reserved `global` env   (always — alias for user's real ~/.claude / ~/.codex)
///
/// Step 6 is the universal escape hatch: a hook refusing launch, a
/// missing pin, a deleted env — any path that lands here without a
/// match still resolves to `global`, so the user is never locked out
/// of the underlying tool.
pub fn resolve(cwd: &Path) -> Result<Option<Resolved>> {
    if let Ok(name) = std::env::var("AENV_OVERRIDE") {
        if !name.is_empty() {
            return Ok(Some(Resolved::bare(name, Source::Override)));
        }
    }
    if let Ok(name) = std::env::var("AENV") {
        if !name.is_empty() {
            return Ok(Some(Resolved::bare(name, Source::EnvVar)));
        }
    }
    if let Some(manifest_path) = paths::find_project_manifest(cwd) {
        // Read just enough of the manifest to extract [env].name —
        // full validation happens later when Env::open_at_project loads it.
        let body = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?;
        let label = parse_manifest_env_name(&body).with_context(|| {
            format!(
                "parse [env].name from {}: project-mode requires this field",
                manifest_path.display()
            )
        })?;
        let project_dir = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let slot = paths::project_slot_name(&label, &project_dir);
        return Ok(Some(Resolved {
            name: label,
            slot,
            source: Source::ProjectManifest,
            version_file: None,
            project_manifest: Some(manifest_path),
        }));
    }
    if let Some(file) = paths::find_version_file(cwd) {
        let body =
            std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        let name = body.trim().to_string();
        if !name.is_empty() {
            let slot = name.clone();
            return Ok(Some(Resolved {
                name,
                slot,
                source: Source::VersionFile,
                version_file: Some(file),
                project_manifest: None,
            }));
        }
    }
    if let Some(name) = global_default()? {
        return Ok(Some(Resolved::bare(name, Source::GlobalDefault)));
    }
    // Final fallback: the reserved `global` env (= user's real
    // ~/.claude / ~/.codex). Always resolves so a fresh aenv install,
    // a corrupted config, or a `aenv quit` from a deleted env all
    // land safely on the user's pre-aenv tool config.
    Ok(Some(Resolved::bare(
        crate::env::GLOBAL_NAME.to_string(),
        Source::GlobalDefault,
    )))
}

/// Extract just `[env].name` from a manifest TOML body without
/// performing full validation. We deliberately do NOT call
/// `Manifest::load`/`validate` here because the resolver is on the hot
/// path — the heavyweight validation runs once when `Env::open_at_project`
/// actually loads the env for use.
fn parse_manifest_env_name(body: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct ProjEnv {
        name: String,
    }
    #[derive(Deserialize)]
    struct ProjShape {
        env: ProjEnv,
    }
    let parsed: ProjShape = toml::from_str(body).context("manifest is not valid TOML")?;
    Ok(parsed.env.name)
}

fn global_default() -> Result<Option<String>> {
    let p = paths::config_path()?;
    if !p.is_file() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    // A truncated or hand-edited config.toml used to silently zero out
    // default_env via unwrap_or_default(). Surface the parse error with
    // the path so the user can repair it.
    let cfg: GlobalConfig = toml::from_str(&body)
        .with_context(|| format!("parse {} (corrupt? remove or repair)", p.display()))?;
    Ok(cfg.default_env)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub default_env: Option<String>,
    /// Path to the real claude binary (cached at `aenv init`).
    #[serde(default)]
    pub real_claude: Option<std::path::PathBuf>,
}

impl GlobalConfig {
    pub fn load() -> Result<Self> {
        let p = paths::config_path()?;
        if !p.is_file() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        toml::from_str(&body)
            .with_context(|| format!("parse {} (corrupt? remove or repair)", p.display()))
    }
    pub fn save(&self) -> Result<()> {
        let p = paths::config_path()?;
        if let Some(parent) = p.parent() {
            paths::ensure_dir(parent)?;
        }
        let body = toml::to_string_pretty(self)?;
        crate::paths::write_atomic(&p, body.as_bytes())?;
        Ok(())
    }
}
