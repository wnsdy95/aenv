//! `<env>/.claude/plugins/known_marketplaces.json` — Claude Code's
//! registry of marketplace sources (where to fetch plugin
//! manifests from).
//!
//! Verified against Claude Code 2.1.138's bundled JS:
//!
//! ```js
//! ISH = () => v.record(v.string(), bR4())     // top-level: name → entry
//! bR4 = () => v.object({
//!     source: B6_(),                           // discriminatedUnion on "source"
//!     installLocation: v.string(),             // required
//!     lastUpdated: v.string(),                 // required (ISO 8601)
//!     autoUpdate: v.boolean().optional(),
//! })
//! ```
//!
//! The `source` discriminator covers three known shapes (extracted
//! from `ifl::marketplace_source_to_spec` round-trip + binary):
//! `{source: "github", repo: "owner/repo"}`,
//! `{source: "url", url: "..."}`,
//! `{source: "git-subdir", url: "...", path: "..."}`.
//!
//! Read+write live together so the format never drifts between
//! consumer (`ifl::build_global_source` reads `~/.claude/plugins/
//! known_marketplaces.json`) and producer (install/mod.rs writes
//! it for each manifest plugin).
//!
//! Forward-compat: any unknown source variant or unknown field is
//! preserved through the `extra` flatten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level shape: a map from marketplace name to its registry
/// entry. We wrap it in a newtype so the type name is searchable
/// across the codebase, but `transparent` keeps the JSON shape
/// flat (just a record).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KnownMarketplacesJson {
    pub marketplaces: BTreeMap<String, MarketplaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    /// The source object Claude Code uses to fetch the marketplace
    /// manifest. We store as raw `serde_json::Value` rather than a
    /// typed enum so unknown shapes (future variants like `gitlab`,
    /// `local`, …) round-trip byte-for-byte. Use the `source_*`
    /// helpers (or constructors on `MarketplaceSource`) when you
    /// need a typed view.
    pub source: serde_json::Value,
    /// Required by Claude Code's zod schema. Local cache path
    /// where the marketplace manifest is stored. aenv writes
    /// `<env>/.claude/plugins/marketplaces/<name>` so the manifest
    /// lookup stays inside the env-local tree.
    #[serde(rename = "installLocation")]
    pub install_location: String,
    /// Required ISO 8601 timestamp.
    #[serde(rename = "lastUpdated")]
    pub last_updated: String,
    #[serde(
        rename = "autoUpdate",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub auto_update: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Convenience constructors / accessors over the raw
/// `MarketplaceEntry::source` JSON value. The wire format on disk
/// is `{"source": "<kind>", ...}` matching Claude Code's
/// `discriminatedUnion("source")`.
pub struct MarketplaceSource;

impl MarketplaceSource {
    /// Build the JSON for a github source from a
    /// `git+https://github.com/owner/repo[#sha]` URL. Returns None
    /// if the URL doesn't match the github shape. The `#<sha>`
    /// fragment is dropped — sha is recorded per-plugin in
    /// `installed_plugins.json`, not per-marketplace.
    pub fn github_from_url(url: &str) -> Option<serde_json::Value> {
        let stripped = url.strip_prefix("git+").unwrap_or(url);
        let body = stripped.strip_prefix("https://github.com/")?;
        let body = body.split('#').next().unwrap_or(body);
        let body = body.strip_suffix(".git").unwrap_or(body);
        let body = body.trim_end_matches('/');
        let parts: Vec<&str> = body.split('/').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return None;
        }
        Some(serde_json::json!({
            "source": "github",
            "repo": format!("{}/{}", parts[0], parts[1]),
        }))
    }

    /// Pull `repo` out of a github-shaped source value. None for
    /// non-github (or malformed) values.
    #[allow(dead_code)] // Phase 2-F (doctor drift) will call this for source diff.
    pub fn github_repo(value: &serde_json::Value) -> Option<&str> {
        if value.get("source").and_then(|v| v.as_str()) != Some("github") {
            return None;
        }
        value.get("repo").and_then(|v| v.as_str())
    }
}

/// Path to `<env>/.claude/plugins/known_marketplaces.json`.
pub fn path_for(env: &crate::env::Env) -> PathBuf {
    env.claude_dir()
        .join("plugins")
        .join("known_marketplaces.json")
}

/// Read `known_marketplaces.json` if it exists. Missing or empty
/// → empty default. Unparseable → `Err` with context.
pub fn read(path: &Path) -> Result<KnownMarketplacesJson> {
    if !path.is_file() {
        return Ok(KnownMarketplacesJson::default());
    }
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if body.trim().is_empty() {
        return Ok(KnownMarketplacesJson::default());
    }
    serde_json::from_str(&body)
        .with_context(|| format!("parse {} as known_marketplaces.json", path.display()))
}

/// Atomic-replace write.
pub fn write(path: &Path, doc: &KnownMarketplacesJson) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    let body = serde_json::to_string_pretty(doc)
        .with_context(|| format!("serialize known_marketplaces.json for {}", path.display()))?;
    crate::paths::write_atomic(path, body.as_bytes())?;
    Ok(())
}

/// Default install_location for a given env + marketplace name —
/// matches Claude Code's expected layout (`<plugins-dir>/marketplaces/<name>`)
/// so the marketplace manifest lookup keeps working under
/// `CLAUDE_CONFIG_DIR=<env>/.claude`.
pub fn default_install_location(env: &crate::env::Env, marketplace: &str) -> String {
    env.claude_dir()
        .join("plugins")
        .join("marketplaces")
        .join(marketplace)
        .display()
        .to_string()
}

/// Upsert a marketplace, replacing any existing entry under the
/// same name. Foreign entries (Claude Code-added or user-edited)
/// at other names are preserved.
pub fn upsert(doc: &mut KnownMarketplacesJson, name: String, entry: MarketplaceEntry) {
    doc.marketplaces.insert(name, entry);
}

/// Remove a marketplace by name. Returns true iff something was
/// removed. Caller decides whether to also delete the local
/// install_location directory.
pub fn remove(doc: &mut KnownMarketplacesJson, name: &str) -> bool {
    doc.marketplaces.remove(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn github_url_normalizes_to_owner_repo() {
        for (input, expected_repo) in [
            (
                "git+https://github.com/anthropics/claude-plugins-official#abc123",
                "anthropics/claude-plugins-official",
            ),
            (
                "git+https://github.com/anthropics/claude-plugins-official",
                "anthropics/claude-plugins-official",
            ),
            (
                "https://github.com/anthropics/claude-plugins-official",
                "anthropics/claude-plugins-official",
            ),
            (
                "https://github.com/anthropics/claude-plugins-official.git",
                "anthropics/claude-plugins-official",
            ),
            (
                "https://github.com/anthropics/claude-plugins-official/",
                "anthropics/claude-plugins-official",
            ),
        ] {
            let v = MarketplaceSource::github_from_url(input)
                .unwrap_or_else(|| panic!("expected github source for {input}"));
            assert_eq!(MarketplaceSource::github_repo(&v), Some(expected_repo));
        }
    }

    #[test]
    fn non_github_urls_yield_none() {
        for input in [
            "git+https://gitlab.com/foo/bar",
            "git+https://github.com/onlyowner",
            "git+https://github.com/foo/bar/extra/segments",
            "npm:@scope/pkg@1.0.0",
            "file:///abs/path",
        ] {
            assert!(
                MarketplaceSource::github_from_url(input).is_none(),
                "expected no github source for {input}"
            );
        }
    }

    #[test]
    fn round_trip_github_marketplace_entry() {
        let mut doc = KnownMarketplacesJson::default();
        upsert(
            &mut doc,
            "claude-plugins-official".into(),
            MarketplaceEntry {
                source: MarketplaceSource::github_from_url(
                    "git+https://github.com/anthropics/claude-plugins-official#abc123",
                )
                .unwrap(),
                install_location:
                    "/abs/.aenv/envs/x/.claude/plugins/marketplaces/claude-plugins-official".into(),
                last_updated: ts("2026-05-11T00:00:00Z"),
                auto_update: None,
                extra: BTreeMap::new(),
            },
        );
        let body = serde_json::to_string_pretty(&doc).unwrap();
        assert!(body.contains("\"source\":"), "source key required: {body}");
        assert!(body.contains("\"github\""), "github discriminator: {body}");
        assert!(
            body.contains("\"repo\": \"anthropics/claude-plugins-official\""),
            "{body}"
        );
        assert!(body.contains("\"installLocation\":"), "{body}");
        assert!(body.contains("\"lastUpdated\":"), "{body}");
        assert!(
            !body.contains("\"autoUpdate\""),
            "omitted optional must not appear: {body}"
        );

        let parsed: KnownMarketplacesJson = serde_json::from_str(&body).unwrap();
        let entry = &parsed.marketplaces["claude-plugins-official"];
        assert_eq!(
            MarketplaceSource::github_repo(&entry.source),
            Some("anthropics/claude-plugins-official")
        );
        assert_eq!(entry.last_updated, "2026-05-11T00:00:00Z");
    }

    #[test]
    fn unknown_source_shape_round_trips_byte_for_byte() {
        // Forward-compat: a marketplace whose `source` is an
        // unknown shape (e.g. `{"source":"gitlab","project":...}`)
        // or even just a bare string must round-trip verbatim.
        // We store source as serde_json::Value, so any JSON the
        // parser sees is preserved without modeling.
        let body = r#"{"future-mkt":{"source":{"source":"gitlab","project":"foo/bar"},"installLocation":"/x","lastUpdated":"2026-05-11T00:00:00Z"}}"#;
        let parsed: KnownMarketplacesJson = serde_json::from_str(body).unwrap();
        let entry = &parsed.marketplaces["future-mkt"];
        assert_eq!(entry.install_location, "/x");
        assert_eq!(entry.last_updated, "2026-05-11T00:00:00Z");
        assert_eq!(
            entry.source.get("source").and_then(|v| v.as_str()),
            Some("gitlab")
        );
        assert_eq!(
            entry.source.get("project").and_then(|v| v.as_str()),
            Some("foo/bar")
        );
        // Re-serialize and verify the unknown source shape survives.
        let re = serde_json::to_string(&parsed).unwrap();
        assert!(re.contains(r#""source":"gitlab""#), "{re}");
        assert!(re.contains(r#""project":"foo/bar""#), "{re}");
    }

    #[test]
    fn empty_or_missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.json");
        assert_eq!(read(&missing).unwrap().marketplaces.len(), 0);

        let empty = tmp.path().join("empty.json");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(read(&empty).unwrap().marketplaces.len(), 0);
    }

    #[test]
    fn upsert_and_remove_keep_other_entries() {
        let mut doc = KnownMarketplacesJson::default();
        let mk_entry = |repo: &str, loc: &str| MarketplaceEntry {
            source: serde_json::json!({"source": "github", "repo": repo}),
            install_location: loc.to_string(),
            last_updated: ts("2026-01-01T00:00:00Z"),
            auto_update: None,
            extra: BTreeMap::new(),
        };
        upsert(&mut doc, "a".into(), mk_entry("x/a", "/a"));
        upsert(&mut doc, "b".into(), mk_entry("x/b", "/b"));
        assert!(remove(&mut doc, "a"));
        assert!(doc.marketplaces.contains_key("b"));
        assert!(!remove(&mut doc, "a"));
    }
}
