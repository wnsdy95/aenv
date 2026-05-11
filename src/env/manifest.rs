use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

const MANIFEST_NAME: &str = "aenv.toml";
const LOCKFILE_NAME: &str = "aenv.lock";
/// Manifest schema version. Bumped to "2" when aenv pivoted from a
/// Claude-Code-only wrapper to a universal AI-CLI env switcher
/// (claude / codex / cursor / gemini). The breaking change in v2:
/// `[env].cc_compatible: String` (semver range, claude-only) became
/// `[env].compat: { <backend-id> = "<range>", ... }` so each backend
/// gets its own compatibility window. We don't ship a v1 → v2 migrator
/// because aenv had zero pre-1.0 users when this landed; v1 manifests
/// are rejected with a clear error pointing at the v2 schema.
pub const SCHEMA_VERSION: &str = "2";
/// Lockfile schema version. Bumped to "3" alongside manifest v2 to
/// mark the clean break: v3 lockfiles add the optional `backend` field
/// on each `LockedPlugin` / `LockedSkill` row so `aenv sync` knows
/// which backend owns a materialized resource. Like the manifest, v1
/// and v2 lockfiles are rejected — we have no users to migrate.
///
/// Changelog:
/// - "1": initial. sha256 included POSIX mode bits + raw FS-byte
///   filenames. Made lockfiles platform-specific.
/// - "2": cross-platform. Mode bits dropped, separators normalized,
///   filenames NFC-normalized.
/// - "3": pivot to universal backend support. `loaded_legacy` flag and
///   v1-migration code removed. Adds `backend` field on locked rows.
pub const LOCKFILE_SCHEMA_VERSION: &str = "3";

/// `aenv.toml` — declarative env manifest, v2 schema.
///
/// Validation rules:
/// - `aenv_schema_version` must be `"2"`.
/// - Unknown top-level fields are rejected (via serde `deny_unknown_fields`).
/// - Every `[env].compat.<backend>` value must be a parseable semver range.
/// - All plugin `version` strings must be parseable semver when not `null`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Schema version. Must equal `SCHEMA_VERSION`.
    #[serde(rename = "aenv_schema_version", default = "default_schema")]
    pub schema_version: String,

    pub env: EnvMeta,

    /// Map of MCP server name → spec.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpSpec>,

    #[serde(default)]
    pub plugins: PluginsBlock,

    #[serde(default)]
    pub skills: SkillsBlock,

    #[serde(default)]
    pub hooks: Hooks,

    /// Platforms the project promises to support. Mirrors uv's
    /// `required-environments`: <https://docs.astral.sh/uv/reference/settings/>.
    /// `aenv lock` populates per-platform entries in `aenv.lock` for every
    /// listed key; `aenv doctor` warns when the active host isn't in the
    /// list. v1 leaves this empty by default — pure-source plugins (markdown
    /// skills, JS scripts) work on every host without per-platform pinning.
    /// Canonical keys: `darwin-arm64`, `darwin-x86_64`, `linux-x86_64`,
    /// `linux-aarch64`, `windows-x86_64`, `windows-aarch64`.
    #[serde(default, skip_serializing_if = "PlatformsBlock::is_empty")]
    pub platforms: PlatformsBlock,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformsBlock {
    #[serde(default)]
    pub required: Vec<String>,
}

impl PlatformsBlock {
    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
    }
}

fn default_schema() -> String {
    SCHEMA_VERSION.to_string()
}

fn default_lockfile_schema() -> String {
    LOCKFILE_SCHEMA_VERSION.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvMeta {
    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Per-backend semver compatibility ranges. Keys are stable backend
    /// ids (`claude`, `codex`, ...); values are semver ranges checked
    /// against the resolved binary version at `aenv use` / `aenv doctor`.
    /// Empty by default — most envs don't pin a range.
    /// Example: `compat = { claude = ">=2.5,<3", codex = ">=0.7" }`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub compat: BTreeMap<String, String>,

    #[serde(default)]
    pub created: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Fired before each claude launch under this env.
    /// Receives `AENV_NAME` and `AENV_ROOT` as env vars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_activate: Option<String>,
}

/// `[plugins]` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsBlock {
    /// Items can be:
    /// - "name@version" (string form)
    /// - { name = "...", version = "...", source = "..." } (table form)
    #[serde(default)]
    pub enabled: Vec<PluginRef>,
}

/// `[skills]` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsBlock {
    #[serde(default)]
    pub enabled: Vec<SkillRef>,
}

/// Plugin reference accepts both string ("name@1.2.3") and table form.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginRef {
    Short(String),
    Detailed(PluginSpec),
}

impl PluginRef {
    pub fn into_spec(self) -> Result<PluginSpec> {
        match self {
            PluginRef::Detailed(s) => Ok(s),
            PluginRef::Short(s) => parse_short(&s).map(|(name, ver)| PluginSpec {
                name,
                version: ver,
                source: None,
                subpath: None,
                sha256: None,
                release_url: None,
                target_map: std::collections::BTreeMap::new(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// URL, git+https://..., or file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional plugin root inside `source`, for marketplace repos that
    /// contain many plugins (for example `plugins/code-review`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    /// Pinned content hash (set by `aenv lock`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// URL template for per-platform native binary releases (analogue
    /// of cargo-binstall's `pkg-url`). Variables: `{ version }`,
    /// `{ target }`, `{ archive-format }`, `{ name }`. Per-platform
    /// `target` strings come from `target_map`; `archive-format`
    /// defaults to `tar.gz` and can be overridden via the per-platform
    /// entry in the lockfile. v1 schema only — fetcher unimplemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    /// Map from canonical aenv platform key (e.g. `darwin-arm64`) to
    /// the upstream's per-platform target string used to expand
    /// `release_url`. Lets users keep aenv's canonical kernel-arch
    /// names while emitting whatever the upstream uses (Rust target
    /// triple, Go pair, custom).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub target_map: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SkillRef {
    Short(String),
    Detailed(SkillSpec),
}

impl SkillRef {
    pub fn into_spec(self) -> Result<SkillSpec> {
        match self {
            SkillRef::Detailed(s) => Ok(s),
            SkillRef::Short(s) => Ok(SkillSpec {
                name: s,
                source: None,
                sha256: None,
                release_url: None,
                target_map: std::collections::BTreeMap::new(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// See `PluginSpec::release_url` — same semantics for skills with
    /// per-platform binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub target_map: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSpec {
    /// Transport: "stdio" (default) | "http" | "sse" (deprecated upstream).
    /// Mirrors the `type` field in claude_desktop_config.json /
    /// `claude mcp add-json` / `cursor.mcp.json`.
    /// <https://code.claude.com/docs/en/mcp>
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// stdio: process command (e.g. "npx", "uvx", absolute path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// May contain `${secret:...}` / `${env:...}` placeholders.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// http/sse transport: server URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// http/sse transport: HTTP headers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Pinned MCP version (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

fn parse_short(s: &str) -> Result<(String, Option<String>)> {
    if let Some((name, ver)) = s.rsplit_once('@') {
        if name.is_empty() {
            bail!("plugin spec missing name: '{s}'");
        }
        Ok((name.to_string(), Some(ver.to_string())))
    } else {
        Ok((s.to_string(), None))
    }
}

impl Manifest {
    pub fn default_for(name: &str) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            env: EnvMeta {
                name: name.to_string(),
                description: None,
                compat: BTreeMap::new(),
                created: Some(chrono::Utc::now()),
            },
            platforms: PlatformsBlock::default(),
            mcp: BTreeMap::new(),
            plugins: PluginsBlock::default(),
            skills: SkillsBlock::default(),
            hooks: Hooks::default(),
        }
    }

    pub fn manifest_path(env_root: &Path) -> PathBuf {
        env_root.join(MANIFEST_NAME)
    }

    pub fn lockfile_path(env_root: &Path) -> PathBuf {
        env_root.join(LOCKFILE_NAME)
    }

    pub fn load(env_root: &Path) -> Result<Self> {
        Self::load_from(&Self::manifest_path(env_root))
    }

    /// Load the manifest from an arbitrary path. Used by project-local
    /// mode where the manifest lives next to the user's source tree
    /// rather than under `~/.aenv/envs/<slot>/`.
    pub fn load_from(path: &Path) -> Result<Self> {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let m: Manifest =
            toml::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
        m.validate()?;
        Ok(m)
    }

    pub fn save(&self, env_root: &Path) -> Result<()> {
        self.save_to(&Self::manifest_path(env_root))
    }

    /// Save manifest to an arbitrary path. Project-local mode writes to
    /// the project's `aenv.toml`; global mode writes to `<root>/aenv.toml`.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let body = toml::to_string_pretty(self)
            .with_context(|| format!("serialize manifest for {}", path.display()))?;
        crate::paths::write_atomic(path, body.as_bytes())?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported aenv_schema_version '{}': aenv 0.3 requires schema '{}'. \
                 v1 manifests (cc_compatible, claude-only resources) are not migrated; \
                 see CHANGELOG.md for the v2 shape.",
                self.schema_version,
                SCHEMA_VERSION
            );
        }
        crate::env::validate_resource_name("env", &self.env.name)?;
        for (backend, range) in &self.env.compat {
            semver::VersionReq::parse(range).map_err(|e| {
                anyhow!("invalid compat range for backend '{backend}': '{range}': {e}")
            })?;
        }
        for (mcp_name, spec) in &self.mcp {
            crate::env::validate_resource_name("mcp", mcp_name)?;
            // Reject `${secret:KEY}` references whose KEY isn't a POSIX
            // identifier — same constraint as `aenv secrets add`. Catches
            // collision-prone shapes like `gh-token` at manifest-load time.
            for v in spec.env.values() {
                let mut i = 0;
                let bytes = v.as_bytes();
                while i < bytes.len() {
                    if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                        if let Some(end) = v[i + 2..].find('}') {
                            let inner = &v[i + 2..i + 2 + end];
                            if let Some(key) = inner.strip_prefix("secret:") {
                                crate::secrets::validate_secret_key(key)?;
                            }
                            i = i + 2 + end + 1;
                            continue;
                        }
                    }
                    i += 1;
                }
            }
        }
        for p in &self.plugins.enabled {
            match p {
                PluginRef::Detailed(spec) => {
                    crate::env::validate_resource_name("plugin", &spec.name)?;
                    if let Some(subpath) = &spec.subpath {
                        crate::env::validate_relative_subpath("plugin", subpath)?;
                    }
                    if let Some(v) = &spec.version {
                        semver::Version::parse(v).map_err(|e| {
                            anyhow!("plugin '{}' bad version '{v}': {e}", spec.name)
                        })?;
                    }
                }
                PluginRef::Short(s) => {
                    let (name, version) = match s.rsplit_once('@') {
                        Some((n, v)) => (n, Some(v)),
                        None => (s.as_str(), None),
                    };
                    crate::env::validate_resource_name("plugin", name)?;
                    if let Some(ver) = version {
                        semver::Version::parse(ver)
                            .map_err(|e| anyhow!("plugin '{s}' bad version '{ver}': {e}"))?;
                    }
                }
            }
        }
        for s in &self.skills.enabled {
            let name = match s {
                SkillRef::Detailed(spec) => spec.name.as_str(),
                SkillRef::Short(s) => s.as_str(),
            };
            crate::env::validate_resource_name("skill", name)?;
        }
        Ok(())
    }

    /// Resolve all plugin refs (short + detailed) into PluginSpec list.
    pub fn plugin_specs(&self) -> Result<Vec<PluginSpec>> {
        self.plugins
            .enabled
            .iter()
            .cloned()
            .map(|p| p.into_spec())
            .collect()
    }

    pub fn skill_specs(&self) -> Result<Vec<SkillSpec>> {
        self.skills
            .enabled
            .iter()
            .cloned()
            .map(|s| s.into_spec())
            .collect()
    }
}

/// `aenv.lock` — content hashes for reproducibility.
///
/// `Default` returns a lockfile stamped with the current schema
/// version, NOT an empty string — `save_to` calls `validate` which
/// would reject `schema_version = ""`. Without a manual Default impl,
/// `init --here` 's empty-lockfile bootstrap fails validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    #[serde(default = "default_lockfile_schema")]
    pub schema_version: String,
    /// Generation timestamp — informational only, not deterministic.
    #[serde(default)]
    pub generated: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub plugins: Vec<LockedPlugin>,
    #[serde(default)]
    pub skills: Vec<LockedSkill>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            schema_version: LOCKFILE_SCHEMA_VERSION.to_string(),
            generated: None,
            plugins: Vec::new(),
            skills: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPlugin {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub source: String,
    /// Optional plugin root inside `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    /// What the user *requested* in the manifest (e.g. "v1.2.0", "main",
    /// "1.0.x"). Recorded alongside `sha256` per the Nix flake.lock
    /// pattern of dual rev+narHash: rev is human-meaningful provenance,
    /// sha256 is machine-verifiable identity. Either alone is
    /// insufficient — a rev can be force-pushed, a sha alone tells a
    /// reviewer nothing about the upstream change history.
    /// Optional for backward compat with v1 lockfiles that pre-date
    /// this field (`aenv install` will populate it on next sync).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<String>,
    /// Per-platform sha256 map. v1 only fills the current platform but
    /// the schema is shaped multi so future MCPs with native binaries
    /// (e.g. Rust MCP server compiled per-arch) drop in without a
    /// schema break. Keys: `darwin-arm64`, `darwin-x86_64`,
    /// `linux-x86_64`, `linux-aarch64`, `windows-x86_64`. Empty in v1.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub platforms: std::collections::BTreeMap<String, PlatformLock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedSkill {
    pub name: String,
    pub sha256: String,
    pub source: String,
    /// See `LockedPlugin::requested` — same rationale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub platforms: std::collections::BTreeMap<String, PlatformLock>,
}

/// Per-platform integrity + retrieval entry inside `platforms.<arch>`
/// of a LockedPlugin/Skill. The lockfile records WHERE the bytes came
/// from in addition to WHAT they hash to — analogous to the dual
/// rev/narHash entry on Nix flake.lock inputs and the per-arch sub-
/// packages npm/esbuild emit in their lockfiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformLock {
    pub sha256: String,
    /// Concrete download URL the manifest's `release_url` template
    /// expanded to for this platform. Recorded so the lockfile is
    /// self-contained — install can fetch without re-templating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Archive format: `tar.gz`, `tgz`, `tar`, `zip`. Used at install
    /// time to pick the right unpacker. Defaults to `tar.gz`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_format: Option<String>,
    /// Path inside the archive where the executable lives, relative
    /// to the unpacked root (e.g. `bin/foo`, `foo.exe`). Optional —
    /// most claude code plugins are scripts, not native binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_path: Option<String>,
}

impl Lockfile {
    /// Load lockfile from an arbitrary path. Project mode reads from
    /// the project root; global env mode reads from the env slot via
    /// `Manifest::lockfile_path`.
    ///
    /// Rejects pre-v3 lockfiles outright. The 0.3.0 pivot made the
    /// rest of aenv assume v3 fields (per-row `backend`), and aenv had
    /// zero pre-1.0 users when this landed, so we don't carry the v1
    /// → v2 migration plumbing forward.
    pub fn load_from(p: &Path) -> Result<Self> {
        if !p.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
        let lf: Lockfile =
            toml::from_str(&body).with_context(|| format!("parse {}", p.display()))?;
        lf.validate()
            .with_context(|| format!("validate {}", p.display()))?;
        Ok(lf)
    }

    /// Validate that every entry's `name` is a safe path component and
    /// `sha256` is exactly 64 lowercase hex characters. Without this, a
    /// hand-crafted (or compromised-via-git) lockfile could direct
    /// `sync`/`install` to materialize from outside the store, or write
    /// outside the env's plugin dir via path traversal in the name.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != LOCKFILE_SCHEMA_VERSION {
            bail!(
                "unsupported aenv.lock schema_version '{}': aenv 0.3 requires '{}'. \
                 Pre-v3 lockfiles are not migrated; delete the file and rerun `aenv install`.",
                self.schema_version,
                LOCKFILE_SCHEMA_VERSION
            );
        }
        for p in &self.plugins {
            crate::env::validate_resource_name("plugin", &p.name)?;
            if let Some(subpath) = &p.subpath {
                crate::env::validate_relative_subpath("plugin", subpath)?;
            }
            check_sha256(&p.sha256).with_context(|| format!("plugin '{}' sha256", p.name))?;
        }
        for s in &self.skills {
            crate::env::validate_resource_name("skill", &s.name)?;
            check_sha256(&s.sha256).with_context(|| format!("skill '{}' sha256", s.name))?;
        }
        Ok(())
    }

    /// Save lockfile to an arbitrary path. Symmetric to `load_from`.
    pub fn save_to(&self, p: &Path) -> Result<()> {
        self.validate()?;
        let body = toml::to_string_pretty(self)?;
        crate::paths::write_atomic(p, body.as_bytes())?;
        Ok(())
    }

    pub fn find_plugin(&self, name: &str) -> Option<&LockedPlugin> {
        self.plugins.iter().find(|p| p.name == name)
    }

    pub fn find_skill(&self, name: &str) -> Option<&LockedSkill> {
        self.skills.iter().find(|s| s.name == name)
    }
}

fn check_sha256(s: &str) -> Result<()> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("sha256 must be exactly 64 lowercase hex chars, got '{s}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Manifest {
        toml::from_str::<Manifest>(body).expect("parse")
    }

    #[test]
    fn round_trip_minimal() {
        let m = Manifest::default_for("foo");
        let body = toml::to_string(&m).unwrap();
        let back: Manifest = toml::from_str(&body).unwrap();
        assert_eq!(back.env.name, "foo");
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn unknown_top_level_key_rejected() {
        let body = r#"
            aenv_schema_version = "2"
            unknown = "rejected"
            [env]
            name = "x"
        "#;
        let res = toml::from_str::<Manifest>(body);
        assert!(res.is_err(), "unknown key should be rejected");
    }

    #[test]
    fn unknown_env_key_rejected() {
        let body = r#"
            aenv_schema_version = "2"
            [env]
            name = "x"
            weird = true
        "#;
        let res = toml::from_str::<Manifest>(body);
        assert!(res.is_err());
    }

    #[test]
    fn validate_schema_version_mismatch() {
        let body = r#"
            aenv_schema_version = "999"
            [env]
            name = "x"
        "#;
        let m = parse(body);
        let err = m.validate().unwrap_err().to_string();
        assert!(err.contains("999"), "{err}");
    }

    #[test]
    fn v1_manifest_rejected_with_clean_break_notice() {
        // 0.3.0 dropped the v1 → v2 migration; a v1 manifest
        // (cc_compatible, claude-only fields) must fail with a
        // pointer at the v2 shape rather than silently load.
        let body = r#"
            aenv_schema_version = "1"
            [env]
            name = "x"
            cc_compatible = ">=2.5,<3"
        "#;
        // toml parses fine (cc_compatible is just an unknown env field
        // → fails deny_unknown_fields), but the schema-version check
        // also catches it on the validate() path that load_from uses.
        if let Ok(m) = toml::from_str::<Manifest>(body) {
            let err = m.validate().unwrap_err().to_string();
            assert!(
                err.contains("schema") && err.contains("'1'"),
                "expected v1 rejection, got: {err}"
            );
        }
    }

    #[test]
    fn validate_compat_invalid_range() {
        let body = r#"
            aenv_schema_version = "2"
            [env]
            name = "x"
            [env.compat]
            claude = "garbage"
        "#;
        let m = parse(body);
        assert!(m.validate().is_err());
    }

    #[test]
    fn validate_compat_valid_per_backend() {
        let body = r#"
            aenv_schema_version = "2"
            [env]
            name = "x"
            [env.compat]
            claude = ">=2.1, <3"
            codex = ">=0.7"
        "#;
        let m = parse(body);
        m.validate().expect("valid ranges");
        assert_eq!(
            m.env.compat.get("claude").map(String::as_str),
            Some(">=2.1, <3")
        );
        assert_eq!(m.env.compat.get("codex").map(String::as_str), Some(">=0.7"));
    }

    #[test]
    fn plugin_short_form_parsed() {
        let pr = PluginRef::Short("foo@1.2.3".to_string());
        let s = pr.into_spec().unwrap();
        assert_eq!(s.name, "foo");
        assert_eq!(s.version, Some("1.2.3".into()));
    }

    #[test]
    fn plugin_short_form_no_version() {
        let pr = PluginRef::Short("bar".to_string());
        let s = pr.into_spec().unwrap();
        assert_eq!(s.name, "bar");
        assert_eq!(s.version, None);
    }

    #[test]
    fn plugin_invalid_semver_rejected_by_validate() {
        let body = r#"
            aenv_schema_version = "2"
            [env]
            name = "x"
            [plugins]
            enabled = ["foo@not-a-version"]
        "#;
        let m = parse(body);
        assert!(m.validate().is_err());
    }

    #[test]
    fn mcp_block_keyed_map() {
        let body = r#"
            aenv_schema_version = "2"
            [env]
            name = "x"
            [mcp.github]
            command = "npx"
            args = ["-y", "server"]
            env = { TOKEN = "${secret:t}" }
        "#;
        let m = parse(body);
        assert!(m.mcp.contains_key("github"));
        let g = &m.mcp["github"];
        assert_eq!(g.command.as_deref(), Some("npx"));
        assert_eq!(g.args, vec!["-y", "server"]);
        assert_eq!(g.env.get("TOKEN").unwrap(), "${secret:t}");
    }

    #[test]
    fn lockfile_pre_v3_rejected_with_clean_break_notice() {
        let body = r#"
            schema_version = "2"
        "#;
        let lf: Lockfile = toml::from_str(body).unwrap();
        let err = lf.validate().unwrap_err().to_string();
        assert!(
            err.contains("'2'") && err.contains("'3'"),
            "expected pre-v3 rejection, got: {err}"
        );
    }

    #[test]
    fn lockfile_find_by_name() {
        let mut lf = Lockfile::default();
        lf.plugins.push(LockedPlugin {
            name: "p".into(),
            version: "1.0.0".into(),
            sha256: "abc".into(),
            source: "x".into(),
            subpath: None,
            requested: None,
            platforms: std::collections::BTreeMap::new(),
        });
        assert!(lf.find_plugin("p").is_some());
        assert!(lf.find_plugin("q").is_none());
    }
}
