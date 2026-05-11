//! `<env>/.claude/plugins/installed_plugins.json` — Claude Code's
//! single source of truth for plugin discovery.
//!
//! Verified against Claude Code 2.1.138's bundled JS: the `cG()`
//! function reads exactly this file and parses `version: 2` shape.
//! There is no directory walk that backs it up — a plugin without
//! an entry here is invisible to Claude Code, no matter what the
//! filesystem looks like under `<plugins-dir>/`. That's why aenv's
//! manifest plugins didn't show up before this module landed.
//!
//! Both `read()` and `write()` live here so the format never drifts
//! between the consumer (ifl::build_global_source synthesizes a
//! source from this file on the user's `~/.claude/`) and the
//! producer (install/mod.rs writes it for each manifest plugin).
//! When Claude Code bumps schema (e.g. v2 → v3), this module is
//! the single place to update.
//!
//! Schema v2 example:
//!
//! ```json
//! {
//!   "version": 2,
//!   "plugins": {
//!     "code-review@claude-plugins-official": [
//!       {
//!         "scope": "user",
//!         "installPath": "/abs/path",
//!         "installedAt": "2026-05-11T00:00:00Z",
//!         "lastUpdated": "2026-05-11T00:00:00Z",
//!         "gitCommitSha": "abc123"
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! Notes on field handling:
//! - **Top-level `version: 2`** is always written. Lower-level
//!   omission of `version` per-plugin is handled by Option<String>
//!   skip_serializing_if; Claude Code's parser accepts both presence
//!   and absence (verified by the fact that ifl read code already
//!   coerces non-semver to None).
//! - **Per-plugin entry is a JSON array** (not a single object).
//!   Claude Code reads `arr.first()` for the active install — the
//!   array shape exists for historical reasons but in practice we
//!   write a single-element array per entry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u8 = 2;

/// Top-level shape of `installed_plugins.json`. The plugins map's
/// key is `"<name>@<marketplace>"`; the value is an array of
/// install records (Claude Code reads the first one).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPluginsJson {
    /// Schema version. We always emit `2`. A `Some(other)` on read
    /// surfaces a doctor-flagged drift.
    #[serde(default = "default_schema_version")]
    pub version: u8,
    #[serde(default)]
    pub plugins: BTreeMap<String, Vec<PluginEntry>>,
    /// Preserve any unknown top-level keys Claude Code might add
    /// (forward-compat). serde flatten + Value catches everything
    /// we don't model.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_schema_version() -> u8 {
    SCHEMA_VERSION
}

/// One install record. Field optionality matches Claude Code's
/// zod schema (extracted directly from the 2.1.138 binary):
/// `scope` and `installPath` are required; everything else is
/// optional. aenv writes both required fields and omits the rest
/// when the manifest has no value to put there. Foreign entries
/// (Claude Code-added) are preserved through the `extra` flatten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Required. `"user"` for everything aenv writes. Claude Code
    /// also uses `"managed"` / `"project"` / `"local"` for the
    /// other `--scope` values, but aenv-managed installs are
    /// always user-scoped (the env's `<env>/.claude/`).
    pub scope: String,
    /// Required. Absolute path to the plugin's on-disk root (the
    /// directory containing `.claude-plugin/plugin.json`). aenv
    /// writes this to the env-local fanout dir.
    #[serde(rename = "installPath")]
    pub install_path: String,
    /// Optional. Plugin version (semver). Omitted when the
    /// manifest has no version pinned — Claude Code's parser
    /// accepts the absence (the same code path also ignores
    /// non-semver values like `"unknown"` that Claude Code itself
    /// sometimes writes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Optional. ISO8601 — first install of this entry. aenv sets
    /// both installedAt and lastUpdated to the same value on first
    /// write, and bumps lastUpdated only on re-install.
    #[serde(
        rename = "installedAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub installed_at: Option<String>,
    #[serde(
        rename = "lastUpdated",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_updated: Option<String>,
    /// Optional. Git commit sha pinning the source. Omitted when
    /// source has no `#<sha>` segment (e.g. `npm:foo@1.2.3` or
    /// `file://`).
    #[serde(
        rename = "gitCommitSha",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub git_commit_sha: Option<String>,
    /// Optional. Tag-derived semver this install resolved to. Used
    /// by Claude Code's `verifyAndDemote` in preference to
    /// `version` when the upstream forgot to bump `plugin.json`.
    /// aenv doesn't compute this today; preserved verbatim on
    /// foreign entries.
    #[serde(
        rename = "resolvedVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resolved_version: Option<String>,
    /// Optional. True when Claude Code pulled this plugin in as a
    /// dependency rather than an explicit install. aenv doesn't
    /// emit this; preserved on foreign entries so the orphan-sweep
    /// behavior is intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    /// Optional. Project path; required by Claude Code for
    /// `project`/`local` scopes only.
    #[serde(
        rename = "projectPath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub project_path: Option<String>,
    /// Forward-compat for any extra Claude Code adds in future
    /// releases. Round-tripped verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl PluginEntry {
    /// Construct a fresh aenv-managed entry. `scope` is always
    /// `"user"`, `installed_at` and `last_updated` both set to
    /// the supplied timestamp. Optional fields default to `None`.
    pub fn aenv_managed(install_path: impl Into<String>, now: &str) -> Self {
        Self {
            scope: "user".to_string(),
            install_path: install_path.into(),
            version: None,
            installed_at: Some(now.to_string()),
            last_updated: Some(now.to_string()),
            git_commit_sha: None,
            resolved_version: None,
            auto: None,
            project_path: None,
            extra: BTreeMap::new(),
        }
    }
}

impl Default for InstalledPluginsJson {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            plugins: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

/// Path to `<env>/.claude/plugins/installed_plugins.json`.
pub fn path_for(env: &crate::env::Env) -> PathBuf {
    env.claude_dir()
        .join("plugins")
        .join("installed_plugins.json")
}

/// Read `installed_plugins.json` if it exists. Missing file →
/// empty Default. Unparseable file → `Err` with context (caller
/// decides whether to bail or fall back to default).
pub fn read(path: &Path) -> Result<InstalledPluginsJson> {
    if !path.is_file() {
        return Ok(InstalledPluginsJson::default());
    }
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if body.trim().is_empty() {
        return Ok(InstalledPluginsJson::default());
    }
    serde_json::from_str(&body)
        .with_context(|| format!("parse {} as installed_plugins.json", path.display()))
}

/// Atomic-replace write. Pretty-printed (2-space) so diffs in the
/// env-local file are reviewable and a hand-edited entry round-trips
/// cleanly. We use the same `paths::write_atomic` every other
/// critical aenv state goes through (temp + fsync + rename) so a
/// SIGKILL mid-write can't leave a partial file.
pub fn write(path: &Path, doc: &InstalledPluginsJson) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    let body = serde_json::to_string_pretty(doc)
        .with_context(|| format!("serialize installed_plugins.json for {}", path.display()))?;
    crate::paths::write_atomic(path, body.as_bytes())?;
    Ok(())
}

/// Build the canonical `"<name>@<marketplace>"` key Claude Code
/// uses. Bare names (no `@`) are returned unchanged so callers can
/// special-case them when no marketplace inference is possible.
#[allow(dead_code)] // Phase 2-E (skill wrapper plugins) will call this.
pub fn plugin_key(name: &str, marketplace: Option<&str>) -> String {
    match marketplace {
        Some(mkt) if !mkt.is_empty() => format!("{name}@{mkt}"),
        _ => name.to_string(),
    }
}

/// Upsert one entry, replacing any existing record under the same
/// key. The first array element is the active record (Claude Code
/// reads `arr.first()`); we emit a single-element array.
pub fn upsert(doc: &mut InstalledPluginsJson, key: String, entry: PluginEntry) {
    doc.version = SCHEMA_VERSION;
    doc.plugins.insert(key, vec![entry]);
}

/// Remove one entry by key. Returns true iff something was
/// removed. Caller uses this to decide whether to also clean
/// related state (settings.json::enabledPlugins, fanout dir).
pub fn remove(doc: &mut InstalledPluginsJson, key: &str) -> bool {
    doc.plugins.remove(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_schema_v2_with_omitted_version_field() {
        // The user explicitly chose "omit version when manifest
        // doesn't pin one". Verify that:
        //   1. We don't write the `"version":` key for entries
        //      with version=None.
        //   2. We can still read the result back without error.
        //   3. Claude Code's parser shape (top-level "version":2)
        //      stays present.
        //   4. Required fields (scope, installPath) ARE always
        //      written — Claude Code's zod parser rejects without
        //      them.
        let mut doc = InstalledPluginsJson::default();
        upsert(
            &mut doc,
            plugin_key("code-review", Some("claude-plugins-official")),
            PluginEntry {
                git_commit_sha: Some("abc123".into()),
                ..PluginEntry::aenv_managed("/path/to/plugin", "2026-05-11T00:00:00Z")
            },
        );
        let body = serde_json::to_string_pretty(&doc).unwrap();
        assert!(
            body.contains("\"version\": 2"),
            "top-level version must be present: {body}"
        );
        assert!(
            body.contains("\"scope\": \"user\""),
            "scope is REQUIRED by Claude Code's zod parser: {body}"
        );
        assert!(
            body.contains("\"installPath\":"),
            "installPath is REQUIRED by Claude Code's zod parser: {body}"
        );
        assert!(
            !body.contains("\"version\": null"),
            "omitted entry version must not serialize as null: {body}"
        );
        // The substring `"version":` must appear EXACTLY once
        // (the top-level one) — otherwise we serialized the entry's
        // version field somehow.
        let count = body.matches("\"version\":").count();
        assert_eq!(
            count, 1,
            "only the top-level version key should serialize: {body}"
        );

        // Round-trip read.
        let parsed: InstalledPluginsJson = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.version, SCHEMA_VERSION);
        let entry = &parsed.plugins["code-review@claude-plugins-official"][0];
        assert_eq!(entry.scope, "user");
        assert_eq!(entry.install_path, "/path/to/plugin");
        assert!(entry.version.is_none());
        assert_eq!(entry.git_commit_sha.as_deref(), Some("abc123"));
    }

    #[test]
    fn upsert_preserves_user_added_entries() {
        // Claude Code's `/plugin install foo` writes its own entries
        // here. aenv install must NOT clobber them when upserting
        // its own — only replace the key it owns.
        let mut doc = InstalledPluginsJson::default();
        // User-added (not aenv's).
        upsert(
            &mut doc,
            "user-foo@user-mkt".into(),
            PluginEntry {
                version: Some("1.0.0".into()),
                ..PluginEntry::aenv_managed("/user/path", "2026-01-01T00:00:00Z")
            },
        );
        // aenv's upsert.
        upsert(
            &mut doc,
            "aenv-bar@aenv-mkt".into(),
            PluginEntry {
                git_commit_sha: Some("abc".into()),
                ..PluginEntry::aenv_managed("/aenv/path", "2026-05-11T00:00:00Z")
            },
        );
        // Both entries must be present after upsert.
        assert!(doc.plugins.contains_key("user-foo@user-mkt"));
        assert!(doc.plugins.contains_key("aenv-bar@aenv-mkt"));
    }

    #[test]
    fn forward_compat_extra_fields_round_trip_per_entry() {
        // Claude Code v3 might add new per-entry fields like
        // `auto: false` or something we haven't modeled. The
        // `extra: BTreeMap<String, Value>` flatten must catch them
        // and round-trip verbatim, otherwise we'd silently drop
        // forward-compat data on rewrite.
        let body = r#"{
            "version": 2,
            "plugins": {
                "x@m": [{
                    "scope": "user",
                    "installPath": "/p",
                    "auto": true,
                    "futurePerEntryField": "preserveMe"
                }]
            }
        }"#;
        let parsed: InstalledPluginsJson = serde_json::from_str(body).unwrap();
        let entry = &parsed.plugins["x@m"][0];
        assert_eq!(entry.auto, Some(true));
        assert!(
            entry.extra.contains_key("futurePerEntryField"),
            "future per-entry field must round-trip: {:?}",
            entry.extra
        );
        let re_serialized = serde_json::to_string(&parsed).unwrap();
        assert!(re_serialized.contains("futurePerEntryField"));
    }

    #[test]
    fn unknown_top_level_keys_round_trip() {
        // Claude Code v3 might add e.g. "lockfile" or "policy" at
        // the top level. We must preserve those bytes through
        // read+write so we never silently drop forward-compat data.
        let body = r#"{
            "version": 2,
            "plugins": {},
            "futureField": {"some": "thing"}
        }"#;
        let parsed: InstalledPluginsJson = serde_json::from_str(body).unwrap();
        assert!(parsed.extra.contains_key("futureField"));
        let re_serialized = serde_json::to_string(&parsed).unwrap();
        assert!(
            re_serialized.contains("\"futureField\""),
            "future field must round-trip: {re_serialized}"
        );
    }

    #[test]
    fn remove_returns_true_only_when_key_existed() {
        let mut doc = InstalledPluginsJson::default();
        upsert(
            &mut doc,
            "x@m".into(),
            PluginEntry::aenv_managed("/p", "2026-05-11T00:00:00Z"),
        );
        assert!(remove(&mut doc, "x@m"));
        assert!(!remove(&mut doc, "x@m"));
        assert!(!remove(&mut doc, "nonexistent@m"));
    }

    #[test]
    fn empty_or_missing_file_returns_default() {
        // Two paths to the same default: literally missing file,
        // and present-but-empty file (e.g. truncated mid-write,
        // rare but possible).
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.json");
        assert_eq!(read(&missing).unwrap().plugins.len(), 0);

        let empty = tmp.path().join("empty.json");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(read(&empty).unwrap().plugins.len(), 0);
    }

    #[test]
    fn plugin_key_handles_bare_and_qualified_names() {
        assert_eq!(plugin_key("foo", Some("bar")), "foo@bar");
        assert_eq!(plugin_key("foo", None), "foo");
        assert_eq!(plugin_key("foo", Some("")), "foo");
    }
}
