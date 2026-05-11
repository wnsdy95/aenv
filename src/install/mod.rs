//! Manifest → env materialization. Resolves plugins/skills declared in
//! `aenv.toml`, downloads missing ones, inserts into the content-addressed
//! store, and hardlinks them into the env's `.claude/plugins/` and
//! `.claude/skills/` directories. Writes/updates `aenv.lock` with hashes.
//!
//! Pure functions — no CLI I/O. The public entry points are
//! `install`, `sync`, and `apply_live`. Every aenv mutator
//! (`add`, `rm`, `ifl`) calls `apply_live` so manifest changes
//! materialize in one shot; `install` and `sync` remain as
//! explicit reproduce/repair commands.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};

use crate::env::manifest::{LockedPlugin, LockedSkill, Lockfile, Manifest, PluginSpec, SkillSpec};
use crate::env::Env;
use crate::store;

#[derive(Debug, Default)]
pub struct Report {
    pub plugins_installed: Vec<String>,
    pub plugins_already: Vec<String>,
    pub skills_installed: Vec<String>,
    pub skills_already: Vec<String>,
}

/// Install all plugins/skills declared in the env's manifest.
/// If `update_lock` is true, `aenv.lock` is rewritten with the resolved hashes.
///
/// Also prunes lockfile entries and on-disk `.claude/plugins/skill-*` wrappers
/// for skills that were removed from the manifest. User-installed plugins
/// (without the `.aenv-managed` marker) are left alone.
///
/// **Merge semantics across the board.** No drift prompt, no
/// `--force` flag — `write_mcp_servers` and the plugin/skill
/// registry both tag aenv-managed entries with `_aenv: true` and
/// preserve un-tagged ad-hoc entries (the user's `/mcp add` and
/// `/plugin install` from inside claude). So `aenv add` + apply,
/// `aenv install` for teammate reproduction, and `aenv ifl` all
/// converge on a single safe rule: aenv only mutates rows it
/// owns.
pub fn install(env: &Env, update_lock: bool) -> Result<Report> {
    let manifest = env.manifest()?;

    let mut lock = Lockfile::load_from(&env.lockfile_path())?;
    lock.schema_version = crate::env::manifest::LOCKFILE_SCHEMA_VERSION.to_string();
    lock.generated = Some(chrono::Utc::now());

    let mut report = Report::default();

    let plugin_specs = manifest.plugin_specs()?;
    let skill_specs = manifest.skill_specs()?;

    let plugins_root = env.claude_dir().join("plugins");
    crate::paths::ensure_dir(&plugins_root)?;
    let mut native_registrations: Vec<NativeRegistration> = Vec::new();
    for spec in &plugin_specs {
        let info = install_plugin(env, spec, &lock)?;
        if info.fresh {
            report.plugins_installed.push(spec.name.clone());
        } else {
            report.plugins_already.push(spec.name.clone());
        }
        // Capture the spec + info pair so we can register the plugin
        // in Claude Code's native JSON registry below. Without this,
        // the plugin's directory is on disk but Claude Code's plugin
        // discovery (`cG()` in 2.1.138) won't see it.
        native_registrations.push(NativeRegistration::from_plugin(spec, &info, env));
        upsert_locked_plugin(&mut lock, info.into_locked());
    }

    let skills_root = env.claude_dir().join("skills");
    crate::paths::ensure_dir(&skills_root)?;
    for spec in &skill_specs {
        let info = install_skill(env, spec, &lock)?;
        if info.fresh {
            report.skills_installed.push(spec.name.clone());
        } else {
            report.skills_already.push(spec.name.clone());
        }
        // Skill wrappers register as plugins under a synthetic
        // `aenv-skills` marketplace so Claude Code's `cG()` discovery
        // sees them through the same single-source-of-truth path.
        // Without this, skills are on disk via wrapper plugin but
        // claude doesn't load them either.
        native_registrations.push(NativeRegistration::from_skill(spec, &info, env));
        upsert_locked_skill(&mut lock, info.into_locked());
    }

    prune_removed_entries(env, &mut lock, &plugin_specs, &skill_specs)?;

    // Per-backend native config rendering from the env's `[mcp.*]`
    // block. Both renderers are idempotent and side-effect-only on
    // their own config files (`<env>/.claude/settings.json` for claude,
    // `<env>/codex/config.toml` for codex), so unconditional emission
    // is fine even when an env is targeted at one backend only.
    write_mcp_servers(env, &manifest)?;
    crate::backend::codex::mcp_render::write_config_toml(env, &manifest)?;

    // Register every aenv-managed plugin in Claude Code's native
    // JSON registry. Without this, the materialized fanout dir is
    // invisible to claude — Claude Code 2.1.138's plugin discovery
    // (`cG()`) reads `installed_plugins.json` exclusively, no
    // directory walk. Skills (registered via wrapper plugins from
    // skills::install_skill_into_env) are added to the same call so
    // they show up under the same code path. The kept-set covers
    // both so prune-on-rerun knows which keys it owns.
    let kept_keys: std::collections::BTreeSet<String> = native_registrations
        .iter()
        .map(|r| r.plugin_key.clone())
        .collect();
    register_managed_plugins_in_native_json(env, &native_registrations, &kept_keys)?;

    if update_lock {
        lock.save_to(&env.lockfile_path())?;
    }
    Ok(report)
}

/// Apply the env's manifest to the local runtime immediately.
///
/// This is the path interactive mutators (`add`, `rm`, `ifl`) use so
/// the next `claude` launch sees the selected plugins, skills, and
/// MCPs without requiring a manual `aenv install`. The public
/// `aenv install` command remains useful as an explicit
/// share/reproduce step, but local UX should be "change manifest →
/// launch".
pub fn apply_live(env: &Env) -> Result<Report> {
    install(env, true)
}

/// Per-plugin info collected during `install()` and handed to
/// `register_managed_plugins_in_native_json` below. We carry the
/// minimum needed to populate Claude Code's three JSON files.
#[derive(Debug, Clone)]
struct NativeRegistration {
    /// `"<name>@<marketplace>"` key used in `installed_plugins.json`
    /// and `enabledPlugins`.
    plugin_key: String,
    /// Marketplace name (the part after `@`). None when the source
    /// shape didn't yield a marketplace (e.g. `npm:`, `file://`) —
    /// caller decides whether to skip the plugin or synthesize a
    /// fallback marketplace name.
    marketplace: Option<String>,
    /// Marketplace's source value as it should appear in
    /// `known_marketplaces.json::source`. Mirrors Claude Code's
    /// `discriminatedUnion("source")` shape (currently we emit
    /// only the github variant; other source kinds make this None
    /// and the plugin gets skipped from native registration).
    marketplace_source: Option<serde_json::Value>,
    /// Source URL we can hand to `store::fetch` to materialize the
    /// marketplace repo at its `installLocation`. For github-source
    /// plugins this equals `spec.source` (the marketplace IS the
    /// repo without subpath); for skill wrappers (synthetic
    /// `aenv-skills` marketplace) this is None — the marketplace
    /// has no remote, just a directory we own.
    marketplace_repo_url: Option<String>,
    /// Absolute on-disk path to the materialized plugin root. Goes
    /// into `installed_plugins.json::installPath`.
    install_path: String,
    /// `#<sha>` from a git source URL, when present. Goes into
    /// `installed_plugins.json::gitCommitSha`.
    git_commit_sha: Option<String>,
    /// Manifest-pinned semver, if any. Omitted when None.
    version: Option<String>,
}

impl NativeRegistration {
    fn from_plugin(spec: &PluginSpec, info: &InstallInfo, env: &Env) -> Self {
        let install_path = env
            .claude_dir()
            .join("plugins")
            .join(&spec.name)
            .display()
            .to_string();
        // Use the RESOLVED source from InstallInfo, not the manifest's
        // raw source field. `aenv add plugin foo@1.2.3` (no
        // `--source`) leaves `spec.source = None`, but install_plugin
        // resolves it to `npm:foo@1.2.3` and stores that in
        // info.source. If we inferred marketplace from spec.source we
        // would treat such plugins as "no marketplace inferable"
        // → skip native registration → claude blind to the plugin.
        let resolved = info.source.as_str();
        let (marketplace, marketplace_source) = infer_marketplace(Some(resolved));
        let plugin_key = match &marketplace {
            Some(mkt) => format!("{}@{}", spec.name, mkt),
            // No marketplace inferred → the bare name. Won't pass
            // Claude Code's `name@mkt` regex; plugin gets skipped
            // in `register_managed_plugins_in_native_json` and the
            // user sees a doctor warning later (Phase 2-F).
            None => spec.name.clone(),
        };
        let git_commit_sha = extract_git_sha(Some(resolved));
        // marketplace_repo_url drives the "fetch + copy whole
        // repo to installLocation" path. Only set it for github
        // marketplaces, where the source URL is the marketplace
        // repo itself. For the synthetic `aenv-local` bucket
        // (file://, npm:, etc.), leave it None — the source
        // points at a single plugin tree, not a marketplace
        // root, so cloning it would write the wrong shape. The
        // synthetic-marketplace.json writer handles aenv-local
        // marketplaces instead.
        let marketplace_repo_url = match marketplace.as_deref() {
            Some(m) if m != AENV_LOCAL_MARKETPLACE => Some(resolved.to_string()),
            _ => None,
        };
        Self {
            plugin_key,
            marketplace,
            marketplace_source,
            marketplace_repo_url,
            install_path,
            git_commit_sha,
            version: info.version.clone(),
        }
    }

    /// Skill wrapper-plugin registration. Skills install via
    /// `skills::install_skill_into_env` which generates a
    /// `<env>/.claude/plugins/skill-<name>/` wrapper plugin. The
    /// wrapper still needs to land in `installed_plugins.json` for
    /// Claude Code to discover and load it. We use a synthetic
    /// marketplace name (`aenv-skills`) with a `local` source
    /// shape — Claude Code's `cG()` reads our installPath
    /// regardless of source kind, so this keeps the discovery
    /// path simple without requiring a real marketplace
    /// registration.
    fn from_skill(spec: &SkillSpec, _info: &InstallInfo, env: &Env) -> Self {
        Self::skill_wrapper(&spec.name, env)
    }

    /// Lockfile-driven plugin registration. Used by `aenv sync`
    /// which works from `aenv.lock` (lock entries carry the
    /// resolved source) rather than from a fresh fetch+install.
    /// Mirrors `from_plugin` semantics so both paths converge on
    /// identical `installed_plugins.json` / `known_marketplaces.json`
    /// / `settings.json::enabledPlugins` writes.
    fn from_locked_plugin(lp: &crate::env::manifest::LockedPlugin, env: &Env) -> Self {
        let install_path = env
            .claude_dir()
            .join("plugins")
            .join(&lp.name)
            .display()
            .to_string();
        let resolved = lp.source.as_str();
        let (marketplace, marketplace_source) = infer_marketplace(Some(resolved));
        let plugin_key = match &marketplace {
            Some(mkt) => format!("{}@{}", lp.name, mkt),
            None => lp.name.clone(),
        };
        let git_commit_sha = extract_git_sha(Some(resolved));
        let marketplace_repo_url = match marketplace.as_deref() {
            Some(m) if m != AENV_LOCAL_MARKETPLACE => Some(resolved.to_string()),
            _ => None,
        };
        Self {
            plugin_key,
            marketplace,
            marketplace_source,
            marketplace_repo_url,
            install_path,
            git_commit_sha,
            version: Some(lp.version.clone()).filter(|v| !v.is_empty()),
        }
    }

    /// Lockfile-driven skill wrapper registration — synthetic
    /// `aenv-skills` marketplace, same shape as `from_skill`.
    fn from_locked_skill(ls: &crate::env::manifest::LockedSkill, env: &Env) -> Self {
        Self::skill_wrapper(&ls.name, env)
    }

    fn skill_wrapper(name: &str, env: &Env) -> Self {
        let wrapper_name = format!("skill-{name}");
        let install_path = env
            .claude_dir()
            .join("plugins")
            .join(&wrapper_name)
            .display()
            .to_string();
        Self {
            plugin_key: format!("{}@{}", wrapper_name, AENV_SKILLS_MARKETPLACE),
            marketplace: Some(AENV_SKILLS_MARKETPLACE.to_string()),
            // Synthetic local source — Claude Code only consults
            // `installPath` for discovery, so the marketplace
            // registry just needs to *exist* under this name. We
            // use `source: local` (an unknown shape from Claude
            // Code's POV but it round-trips through our raw-Value
            // store) to make the entry self-describing for doctor
            // and `aenv ifl` introspection.
            marketplace_source: Some(serde_json::json!({
                "source": "local",
                "kind": "aenv-skills-synthetic",
            })),
            marketplace_repo_url: None,
            install_path,
            git_commit_sha: None,
            version: None,
        }
    }
}

/// Synthetic marketplace name aenv uses to register skill wrapper
/// plugins. Stable so doctor / status / ifl can recognize the
/// pattern (`*@aenv-skills`) when surfacing aenv-managed entries
/// to the user.
const AENV_SKILLS_MARKETPLACE: &str = "aenv-skills";

/// Synthetic marketplace name for plugins that don't map to a
/// real marketplace shape — local `file://` paths, `npm:`, etc.
/// Without bucketing these into a synthetic marketplace, Claude
/// Code's `cG()` discovery would skip them (it reads
/// `installed_plugins.json` keyed by `name@marketplace`), so the
/// fanout dir on disk would be invisible at runtime. aenv ships a
/// synthesized `marketplace.json` at the install_location listing
/// each `aenv-local` plugin by its `installPath` so the resolver
/// has something to walk.
const AENV_LOCAL_MARKETPLACE: &str = "aenv-local";

/// Map a plugin spec's `source` URL to (marketplace_name, source
/// JSON for known_marketplaces.json). Three branches:
///   1. github URL → real marketplace name = repo's last path
///      segment; source is `{source: "github", repo}`.
///   2. `file://` (or bare local path that resolves) → synthetic
///      `aenv-local` marketplace, source records the
///      original URL for round-tripping. Crucial for `aenv ifl`
///      imports of `/plugin install <path>`-style plugins, which
///      have no real marketplace.
///   3. Anything else (`npm:`, `https://*.tar.gz`, etc.) →
///      same `aenv-local` bucket. The synthetic marketplace.json
///      we materialize sidesteps the marketplace-shape
///      requirement; the plugin still loads from `installPath`.
fn infer_marketplace(source: Option<&str>) -> (Option<String>, Option<serde_json::Value>) {
    let Some(s) = source else {
        return (None, None);
    };
    if let Some(value) =
        crate::backend::claude::known_marketplaces::MarketplaceSource::github_from_url(s)
    {
        // Marketplace name = repo's last path segment, matching
        // anthropics' convention (`anthropics/claude-plugins-official`
        // → marketplace `claude-plugins-official`). This is the same
        // segment Claude Code's own `/plugin marketplace add` would
        // pick.
        let repo = value.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        let name = repo.rsplit('/').next().unwrap_or(repo);
        if name.is_empty() {
            return (None, None);
        }
        return (Some(name.to_string()), Some(value));
    }
    // Non-github → synthetic local marketplace. The source field
    // records the original URL so doctor / round-trip imports can
    // surface what aenv knows; `cG()` doesn't read this field so
    // any shape is safe.
    (
        Some(AENV_LOCAL_MARKETPLACE.to_string()),
        Some(serde_json::json!({
            "source": "local",
            "kind": "aenv-managed-local",
            "originalUrl": s,
        })),
    )
}

/// Pull the `#<sha>` fragment out of a git+ source URL, if any.
fn extract_git_sha(source: Option<&str>) -> Option<String> {
    let s = source?.strip_prefix("git+").unwrap_or(source?);
    let (_, frag) = s.rsplit_once('#')?;
    if frag.is_empty() {
        None
    } else {
        Some(frag.to_string())
    }
}

/// Upsert every aenv-managed plugin into the env's native JSON
/// registry (`installed_plugins.json`, `known_marketplaces.json`,
/// `settings.json::enabledPlugins`) and prune any aenv-managed
/// entries whose `plugin_key` isn't in `kept_keys` — that's the
/// "manifest is single source of truth" contract.
///
/// User-added entries (Claude Code's `/plugin install`) are
/// preserved: we identify aenv-managed entries by an `_aenv` flag
/// in the JSON's `extra` (set on every write), and only prune
/// those that flag plus aren't in kept_keys. Foreign entries are
/// never touched.
fn register_managed_plugins_in_native_json(
    env: &Env,
    registrations: &[NativeRegistration],
    kept_keys: &std::collections::BTreeSet<String>,
) -> Result<()> {
    use crate::backend::claude::installed_plugins::{self, PluginEntry};
    use crate::backend::claude::known_marketplaces::{self, MarketplaceEntry};

    let installed_path = installed_plugins::path_for(env);
    let marketplaces_path = known_marketplaces::path_for(env);
    let settings_path = env.claude_dir().join("settings.json");

    let mut installed = installed_plugins::read(&installed_path)?;
    let mut marketplaces = known_marketplaces::read(&marketplaces_path)?;

    // Defend against the user happening to have a foreign
    // (= non-aenv-tagged) marketplace with the same name as our
    // reserved synthetic buckets. Without this check, we'd write
    // `installed_plugins.json` entries like `foo@aenv-local`
    // keyed against the user's unrelated marketplace, and the
    // upsert-guard below would skip overwriting the marketplace
    // entry, so the plugin resolver would end up looking at the
    // user's installLocation (wrong tree) for our plugins.
    //
    // Scope: only check reserved names this apply *actually*
    // writes to. An unrelated `aenv add mcp ...` or `aenv rm`
    // on a github plugin doesn't touch the synthetic buckets,
    // so a foreign `aenv-local` entry mustn't block those.
    // Pruning is foreign-safe already — the prune branches
    // require `_aenv: true` to touch a row, so a user-owned
    // marketplace can't be accidentally pruned even if the
    // name collides.
    let synthetic_in_use: std::collections::BTreeSet<&str> = registrations
        .iter()
        .filter_map(|r| r.marketplace.as_deref())
        .filter(|m| matches!(*m, AENV_LOCAL_MARKETPLACE | AENV_SKILLS_MARKETPLACE))
        .collect();
    for reserved in synthetic_in_use {
        if let Some(entry) = marketplaces.marketplaces.get(reserved) {
            let owned_by_aenv = entry.extra.get("_aenv").and_then(|v| v.as_bool()) == Some(true);
            if !owned_by_aenv {
                bail!(
                    "marketplace name '{reserved}' is reserved by aenv \
                     for synthetic plugin registration, but \
                     known_marketplaces.json at {} already has a \
                     user-owned entry under that name. Rename the \
                     foreign marketplace to something else and rerun.",
                    marketplaces_path.display()
                );
            }
        }
    }

    let mut settings: serde_json::Value = if settings_path.is_file() {
        let body = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("read {}", settings_path.display()))?;
        if body.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&body).with_context(|| {
                format!(
                    "settings.json at {} is not valid JSON",
                    settings_path.display()
                )
            })?
        }
    } else {
        serde_json::json!({})
    };

    let now = chrono::Utc::now().to_rfc3339();

    // Prune aenv-managed entries no longer in kept_keys. Identified
    // by the `_aenv` flag we tag on every entry below — entries
    // without that flag are user-added (e.g. `/plugin install`
    // inside claude) and absolutely preserved.
    let stale_keys: Vec<String> = installed
        .plugins
        .iter()
        .filter(|(key, entries)| {
            !kept_keys.contains(*key)
                && entries
                    .first()
                    .is_some_and(|e| e.extra.get("_aenv").and_then(|v| v.as_bool()) == Some(true))
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in &stale_keys {
        installed_plugins::remove(&mut installed, key);
        if let Some(obj) = settings
            .get_mut("enabledPlugins")
            .and_then(|v| v.as_object_mut())
        {
            obj.remove(key);
        }
    }
    // Cleanup of the on-disk fanout dir for stale plugins —
    // "clean state on prune" per user decision. The dir name is
    // the bare plugin name (the part before `@`); we wrote it
    // there in `install_plugin`.
    for key in &stale_keys {
        let bare = key.split('@').next().unwrap_or(key);
        let dir = env.claude_dir().join("plugins").join(bare);
        if dir.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                eprintln!("aenv: warn: prune could not remove {}: {e}", dir.display());
            }
        }
    }

    // Upsert each kept registration.
    let enabled = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root is not an object"))?
        .entry("enabledPlugins".to_string())
        .or_insert(serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json's enabledPlugins is not an object"))?;
    let mut used_marketplaces: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    for reg in registrations {
        // Skip plugins whose source we can't map to Claude Code's
        // native shape (e.g. npm:, file://). These still live in
        // the manifest and on disk via the legacy fanout path,
        // just not in the native JSON. Doctor flags this in
        // Phase 2-F.
        let Some(mkt) = reg.marketplace.as_deref() else {
            continue;
        };
        let Some(mkt_source) = reg.marketplace_source.as_ref() else {
            continue;
        };
        used_marketplaces.insert(mkt.to_string());

        // installed_plugins.json upsert.
        let mut entry = PluginEntry::aenv_managed(reg.install_path.clone(), &now);
        entry.version = reg.version.clone();
        entry.git_commit_sha = reg.git_commit_sha.clone();
        // Tag with `_aenv` flag so prune can identify our entries
        // without touching user-added ones.
        entry
            .extra
            .insert("_aenv".to_string(), serde_json::Value::Bool(true));
        installed_plugins::upsert(&mut installed, reg.plugin_key.clone(), entry);

        // known_marketplaces.json upsert. Only refresh when we
        // either don't have it yet or it was previously aenv-tagged
        // — we never overwrite a user-added marketplace entry that
        // happens to share the same name. Claude Code's
        // verifyAndDemote uses `lastUpdated` for staleness checks;
        // we set it to `now` on each install so refreshes are clean.
        let prior_was_aenv = marketplaces
            .marketplaces
            .get(mkt)
            .map(|e| e.extra.get("_aenv").and_then(|v| v.as_bool()) == Some(true))
            .unwrap_or(true); // missing → treat as ours-to-write
        if prior_was_aenv {
            let install_location = known_marketplaces::default_install_location(env, mkt);
            let mut mkt_entry = MarketplaceEntry {
                source: mkt_source.clone(),
                install_location,
                last_updated: now.clone(),
                auto_update: None,
                extra: BTreeMap::new(),
            };
            mkt_entry
                .extra
                .insert("_aenv".to_string(), serde_json::Value::Bool(true));
            known_marketplaces::upsert(&mut marketplaces, mkt.to_string(), mkt_entry);
        }

        // settings.json::enabledPlugins upsert.
        enabled.insert(reg.plugin_key.clone(), serde_json::Value::Bool(true));
    }

    // Marketplace prune: drop aenv-tagged marketplaces no longer
    // referenced by any kept plugin. Foreign marketplaces stay.
    let stale_marketplaces: Vec<String> = marketplaces
        .marketplaces
        .iter()
        .filter(|(name, e)| {
            !used_marketplaces.contains(*name)
                && e.extra.get("_aenv").and_then(|v| v.as_bool()) == Some(true)
        })
        .map(|(k, _)| k.clone())
        .collect();
    for name in stale_marketplaces.iter() {
        known_marketplaces::remove(&mut marketplaces, name);
    }
    // Drop the materialized clone too — clean state per the same
    // policy as the plugin fanout dir. User-tagged marketplaces
    // are skipped (they survived the upsert above, so they're not
    // in `stale_marketplaces`).
    for name in &stale_marketplaces {
        let dir = env
            .claude_dir()
            .join("plugins")
            .join("marketplaces")
            .join(name);
        if dir.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                eprintln!(
                    "aenv: warn: prune could not remove marketplace clone {}: {e}",
                    dir.display()
                );
            }
        }
    }

    // Materialize each used marketplace's repo at its
    // `installLocation`. Without this, Claude Code reads the
    // entry from `installed_plugins.json`, looks up the
    // marketplace at `installLocation` to resolve the plugin's
    // metadata, and reports "Plugin not found in marketplace"
    // because the dir is empty. Two paths:
    //
    //   * `marketplace_repo_url = Some(url)` → real marketplace,
    //     re-fetch the github source (no subpath) and copy its
    //     full tree there, matching what `claude marketplace add
    //     github:owner/repo` produces natively.
    //   * `marketplace_repo_url = None` → synthetic marketplace
    //     (`aenv-local`, `aenv-skills`). We can't clone anything,
    //     so we write a minimal `.claude-plugin/marketplace.json`
    //     listing each plugin by bare name. Without this, Claude
    //     Code's resolver still reports "Plugin not found in
    //     marketplace" — the dir exists but has no manifest.
    //
    // Both paths honor the "don't overwrite user-tagged
    // marketplace" rule.
    let mut materialized_remote: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut synthetic_buckets: std::collections::BTreeMap<String, Vec<&NativeRegistration>> =
        std::collections::BTreeMap::new();
    for reg in registrations {
        let Some(mkt) = reg.marketplace.as_deref() else {
            continue;
        };
        if !used_marketplaces.contains(mkt) {
            continue;
        }
        let prior_was_aenv = marketplaces
            .marketplaces
            .get(mkt)
            .map(|e| e.extra.get("_aenv").and_then(|v| v.as_bool()) == Some(true))
            .unwrap_or(true);
        if !prior_was_aenv {
            continue;
        }
        match reg.marketplace_repo_url.as_deref() {
            Some(url) => {
                if materialized_remote.insert(mkt.to_string()) {
                    materialize_marketplace_at_install_location(env, mkt, url)?;
                }
            }
            None => {
                synthetic_buckets
                    .entry(mkt.to_string())
                    .or_default()
                    .push(reg);
            }
        }
    }
    for (mkt, regs) in synthetic_buckets {
        write_synthetic_marketplace_manifest(env, &mkt, &regs)?;
    }

    // Persist all three.
    installed_plugins::write(&installed_path, &installed)?;
    known_marketplaces::write(&marketplaces_path, &marketplaces)?;
    let body = serde_json::to_string_pretty(&settings)?;
    crate::paths::write_atomic(&settings_path, body.as_bytes())?;
    crate::paths::lock_down_file(&settings_path)?;
    Ok(())
}

/// Clone the marketplace repo's full tree into
/// `<env>/.claude/plugins/marketplaces/<name>/` so Claude Code can
/// read its `.claude-plugin/marketplace.json` and resolve plugin
/// names registered in `installed_plugins.json` to actual
/// metadata. Without this, the entry is shown as broken with
/// "Plugin not found in marketplace".
///
/// Refresh-on-each-install: we replace the directory atomically
/// (remove + copy) so a marketplace that moved upstream gets
/// picked up. This costs one git clone per marketplace per
/// install, which matches how Claude Code's native
/// `claude marketplace add` behaves.
fn materialize_marketplace_at_install_location(
    env: &Env,
    marketplace_name: &str,
    source_url: &str,
) -> Result<()> {
    let install_location = env
        .claude_dir()
        .join("plugins")
        .join("marketplaces")
        .join(marketplace_name);
    let fetched = store::fetch(source_url).with_context(|| {
        format!(
            "fetch marketplace '{marketplace_name}' from {source_url} \
             (needed to populate {})",
            install_location.display()
        )
    })?;
    if install_location.exists() {
        std::fs::remove_dir_all(&install_location).with_context(|| {
            format!(
                "remove existing marketplace clone at {}",
                install_location.display()
            )
        })?;
    }
    if let Some(parent) = install_location.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    crate::env::copy_tree(&fetched.dir, &install_location)
        .with_context(|| format!("copy marketplace tree to {}", install_location.display()))?;
    Ok(())
}

/// Write a minimal `.claude-plugin/marketplace.json` at the
/// install_location for a synthetic marketplace
/// (`aenv-local`, `aenv-skills`). Required so Claude Code's
/// resolver can look up plugin entries from `installed_plugins.json`
/// against an existing manifest — without this file, even though
/// the plugin's installPath is correct, the resolver bails with
/// "Plugin not found in marketplace".
///
/// Each plugin entry's `source` is its absolute installPath so a
/// future Claude Code revision that decides to resolve via the
/// marketplace.json source field instead of the installed_plugins
/// entry still works. Today the installPath field on the
/// installed_plugins entry is the load path; marketplace.json
/// existence is the validation gate.
fn write_synthetic_marketplace_manifest(
    env: &Env,
    marketplace_name: &str,
    regs: &[&NativeRegistration],
) -> Result<()> {
    let install_location = env
        .claude_dir()
        .join("plugins")
        .join("marketplaces")
        .join(marketplace_name);
    let claude_plugin_dir = install_location.join(".claude-plugin");
    crate::paths::ensure_dir(&claude_plugin_dir)?;

    let plugins: Vec<serde_json::Value> = regs
        .iter()
        .map(|r| {
            // bare name = part before `@` in plugin_key
            let bare = r
                .plugin_key
                .split('@')
                .next()
                .unwrap_or(&r.plugin_key)
                .to_string();
            let mut entry = serde_json::Map::new();
            entry.insert("name".to_string(), serde_json::Value::String(bare));
            entry.insert(
                "source".to_string(),
                serde_json::Value::String(r.install_path.clone()),
            );
            if let Some(v) = r.version.as_ref() {
                entry.insert("version".to_string(), serde_json::Value::String(v.clone()));
            }
            serde_json::Value::Object(entry)
        })
        .collect();
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "name": marketplace_name,
        "plugins": plugins,
    }))?;
    crate::paths::write_atomic(&claude_plugin_dir.join("marketplace.json"), body.as_bytes())?;
    Ok(())
}

/// Remove from the lockfile and on-disk env directory any plugins/skills that
/// were dropped from the manifest. Keeps unmanaged (user-installed) plugins
/// in place — only `aenv-managed` directories are removed.
fn prune_removed_entries(
    env: &Env,
    lock: &mut Lockfile,
    plugin_specs: &[PluginSpec],
    skill_specs: &[SkillSpec],
) -> Result<()> {
    use std::collections::HashSet;
    let kept_plugins: HashSet<&str> = plugin_specs.iter().map(|s| s.name.as_str()).collect();
    let kept_skills: HashSet<&str> = skill_specs.iter().map(|s| s.name.as_str()).collect();

    // Lockfile pruning is straightforward — drop unreferenced entries.
    lock.plugins
        .retain(|p| kept_plugins.contains(p.name.as_str()));
    lock.skills
        .retain(|s| kept_skills.contains(s.name.as_str()));

    // On-disk pruning: anything carrying the `.aenv-managed` marker was
    // created by us — safe to remove when the manifest no longer references
    // it. Skill wrappers use the `skill-` prefix; regular plugins use their
    // bare name. User-installed plugins (no marker) are left alone.
    let plugins_root = env.claude_dir().join("plugins");
    if let Ok(rd) = std::fs::read_dir(&plugins_root) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if !crate::skills::is_managed(&path) {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let keep = if let Some(skill) = name_str.strip_prefix("skill-") {
                kept_skills.contains(skill)
            } else {
                kept_plugins.contains(name_str.as_ref())
            };
            if !keep {
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    eprintln!(
                        "aenv: warn: could not prune managed dir {}: {e}",
                        path.display()
                    );
                }
            }
        }
    }
    Ok(())
}

/// Render the `[mcp.<name>]` declarations into the env's
/// `.claude/settings.json` `mcpServers` section.
///
/// **Merge semantics**, not wholesale overwrite. Manifest entries are
/// tagged with `_aenv: true` (mirroring the plugin pattern in
/// `installed_plugins.json`); ad-hoc entries the user added with
/// `/mcp add` inside claude lack that flag and are preserved
/// byte-identically across every aenv mutation. Manifest entries
/// that fall out of `aenv.toml` (`aenv rm mcp foo`) are removed
/// from settings.json by detecting the `_aenv: true` flag — only
/// aenv-managed entries get pruned, ad-hoc ones are immortal as
/// far as aenv is concerned.
///
/// The pre-pivot wholesale-overwrite path made `aenv add`,
/// `aenv install`, `aenv ifl` all silent destroyers of `/mcp add`
/// entries. The drift prompt that guarded against this is now
/// obsolete — merge handles it cleanly without user intervention.
///
/// Env values containing `${secret:KEY}` and `${env:VAR}` are translated to
/// claude-native env-var references (`${AENV_<env>_<KEY>}` and `${VAR}`
/// respectively). The shim exports the matching values from the OS keyring
/// at launch time (`backend::claude::shim::claude_secret_env_vars`), so no
/// plaintext lands on disk in `aenv.toml` (settings.json gets references,
/// not resolved values).
fn write_mcp_servers(env: &Env, manifest: &Manifest) -> Result<()> {
    let settings_path = env.claude_dir().join("settings.json");
    let mut settings: Value = if settings_path.is_file() {
        let body = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("read {}", settings_path.display()))?;
        // Don't silently nuke a user-edited settings.json that happens to have
        // a syntax error — that would lose customizations. Surface the error
        // so the user can fix it.
        if body.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&body).with_context(|| {
                format!(
                    "settings.json at {} is not valid JSON. \
                     Refusing to overwrite — fix it manually first.",
                    settings_path.display()
                )
            })?
        }
    } else {
        serde_json::json!({})
    };

    // Existing mcpServers — partition into aenv-managed (`_aenv: true`)
    // and ad-hoc. Ad-hoc entries survive the merge unchanged; aenv
    // entries are rebuilt from the manifest below.
    let existing: Map<String, Value> = settings
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut merged: Map<String, Value> = Map::new();
    for (name, val) in existing {
        let is_aenv = val.get("_aenv").and_then(|v| v.as_bool()).unwrap_or(false);
        if !is_aenv {
            merged.insert(name, val);
        }
    }

    // Now layer manifest entries on top, tagging each with
    // `_aenv: true` so the next merge knows it owns them.
    let manifest_entries = build_mcp_servers_map(&env.name, manifest);
    for (name, mut val) in manifest_entries {
        if let Some(obj) = val.as_object_mut() {
            obj.insert("_aenv".into(), Value::Bool(true));
        }
        // Manifest takes precedence on name collision — if the user
        // both `aenv add mcp foo` and `/mcp add foo`-ed inside
        // claude, the manifest wins (otherwise reproducibility is
        // broken).
        merged.insert(name, val);
    }

    let root = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root is not an object"))?;
    if merged.is_empty() {
        root.remove("mcpServers");
    } else {
        root.insert("mcpServers".into(), Value::Object(merged));
    }

    let body = serde_json::to_string_pretty(&settings)?;
    crate::paths::write_atomic(&settings_path, body.as_bytes())?;
    crate::paths::lock_down_file(&settings_path)?;
    Ok(())
}

/// Build the `mcpServers` object value from a manifest, rewriting
/// `${secret:K}` / `${env:V}` placeholders into env-var refs the shim
/// exports at launch. Public so future render paths (codex's
/// `config.toml`, third-party backends, the legacy overlay path now
/// removed) can reuse the rewrite without duplicating it.
pub fn build_mcp_servers_map(env_name: &str, manifest: &Manifest) -> Map<String, Value> {
    let mut servers = Map::new();
    for (name, spec) in &manifest.mcp {
        let mut entry = Map::new();
        // Transport: emit `type` field if explicitly set. Mirrors the
        // claude_desktop_config.json schema (stdio/http/sse).
        if let Some(t) = &spec.transport {
            entry.insert("type".into(), Value::String(t.clone()));
        }
        if let Some(cmd) = &spec.command {
            entry.insert("command".into(), Value::String(cmd.clone()));
        }
        if !spec.args.is_empty() {
            entry.insert(
                "args".into(),
                Value::Array(spec.args.iter().cloned().map(Value::String).collect()),
            );
        }
        if !spec.env.is_empty() {
            let mut env_map = Map::new();
            for (k, v) in &spec.env {
                env_map.insert(k.clone(), Value::String(rewrite_placeholders(env_name, v)));
            }
            entry.insert("env".into(), Value::Object(env_map));
        }
        if let Some(url) = &spec.url {
            entry.insert("url".into(), Value::String(url.clone()));
        }
        if !spec.headers.is_empty() {
            let mut hdr_map = Map::new();
            for (k, v) in &spec.headers {
                hdr_map.insert(k.clone(), Value::String(rewrite_placeholders(env_name, v)));
            }
            entry.insert("headers".into(), Value::Object(hdr_map));
        }
        servers.insert(name.clone(), Value::Object(entry));
    }
    servers
}

/// Translate aenv-specific placeholders to env-var refs claude code reads
/// natively at MCP launch time.
///   `${secret:KEY}` → `${AENV_<env>_<KEY>}` — shim exports value from keyring
///   `${env:VAR}`    → `${VAR}` — claude inherits from the shim's env
///   anything else   → kept verbatim
fn rewrite_placeholders(env_name: &str, input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = input[i + 2..].find('}') {
                let inner = &input[i + 2..i + 2 + end];
                let replacement = if let Some(key) = inner.strip_prefix("secret:") {
                    format!("${{{}}}", aenv_secret_var_name(env_name, key))
                } else if let Some(var) = inner.strip_prefix("env:") {
                    format!("${{{var}}}")
                } else {
                    format!("${{{inner}}}")
                };
                out.push_str(&replacement);
                i = i + 2 + end + 1;
                continue;
            }
        }
        let c = input[i..].chars().next().expect("loop bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Env-var name the shim uses to bridge a keyring-stored secret into the
/// MCP process via claude's settings.json `${VAR}` substitution.
///
/// POSIX env-var names must match `[A-Z_][A-Z0-9_]*`. We uppercase, replace
/// any non-alphanumeric char with `_`, and prefix-pad an underscore if the
/// resulting name would start with a digit. This makes the function total —
/// any `(env_name, key)` pair (validated or not) yields a usable identifier.
pub fn aenv_secret_var_name(env_name: &str, key: &str) -> String {
    let env_part = sanitize_for_env_var(env_name);
    let key_part = sanitize_for_env_var(key);
    format!("AENV_{env_part}_{key_part}")
}

fn sanitize_for_env_var(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        return "_".to_string();
    }
    // POSIX disallows a leading digit.
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_secret_placeholder() {
        let out = rewrite_placeholders("dev", "${secret:gh-token}");
        assert_eq!(out, "${AENV_DEV_GH_TOKEN}");
    }

    #[test]
    fn infer_marketplace_github_source() {
        // The github URL → marketplace=last-path-segment rule. Same
        // segment Claude Code's own `/plugin marketplace add` picks.
        let (mkt, source) = infer_marketplace(Some(
            "git+https://github.com/anthropics/claude-plugins-official#abc",
        ));
        assert_eq!(mkt.as_deref(), Some("claude-plugins-official"));
        let s = source.expect("github source value present");
        assert_eq!(s["source"], "github");
        assert_eq!(s["repo"], "anthropics/claude-plugins-official");
    }

    #[test]
    fn from_plugin_uses_resolved_info_source_not_manifest_field() {
        // Regression: `aenv add plugin foo@1.2.3` (no `--source`)
        // leaves `spec.source = None`, but install_plugin resolves
        // it to `npm:foo@1.2.3` in `info.source`. NativeRegistration
        // must use info.source for marketplace inference — otherwise
        // npm-fallback plugins skip native registration and Claude
        // Code never sees them after a live apply.
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let spec = PluginSpec {
            name: "foo".into(),
            version: Some("1.2.3".into()),
            source: None, // manifest didn't specify; install_plugin resolved npm:foo@1.2.3
            subpath: None,
            sha256: None,
            release_url: None,
            target_map: BTreeMap::new(),
        };
        let info = InstallInfo {
            name: "foo".into(),
            version: Some("1.2.3".into()),
            sha256: "0".repeat(64),
            source: "npm:foo@1.2.3".into(),
            subpath: None,
            requested: None,
            fresh: true,
        };
        let reg = NativeRegistration::from_plugin(&spec, &info, &env);
        assert_eq!(
            reg.marketplace.as_deref(),
            Some(AENV_LOCAL_MARKETPLACE),
            "npm-resolved plugin must bucket into aenv-local, not bare name"
        );
        assert_eq!(reg.plugin_key, "foo@aenv-local");
    }

    #[test]
    fn register_refuses_foreign_aenv_local_marketplace_when_writing_to_it() {
        // Name collision: if the env already has a non-aenv-tagged
        // `aenv-local` marketplace AND this apply targets that
        // bucket, bail. Without it we'd write installed_plugins
        // entries keyed against the user's unrelated marketplace.
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        std::fs::write(
            tmp.path()
                .join(".claude/plugins/known_marketplaces.json"),
            r#"{"aenv-local":{"source":{"source":"github","repo":"user/their-repo"},"installLocation":"/x","lastUpdated":"2026-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let reg = NativeRegistration {
            plugin_key: "sample@aenv-local".into(),
            marketplace: Some(AENV_LOCAL_MARKETPLACE.into()),
            marketplace_source: Some(serde_json::json!({
                "source": "local",
                "kind": "aenv-managed-local",
            })),
            marketplace_repo_url: None,
            install_path: "/some/installPath".into(),
            git_commit_sha: None,
            version: None,
        };
        let mut kept = std::collections::BTreeSet::new();
        kept.insert("sample@aenv-local".to_string());
        let err = register_managed_plugins_in_native_json(&env, std::slice::from_ref(&reg), &kept)
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("reserved by aenv") && msg.contains("aenv-local"),
            "expected reservation error, got: {msg}"
        );
    }

    #[test]
    fn register_lets_unrelated_apply_succeed_when_foreign_aenv_local_exists() {
        // Companion: if this apply doesn't write to aenv-local at
        // all (e.g. no registrations, or only github-marketplace
        // registrations), a foreign `aenv-local` entry mustn't
        // block the operation. Without scope, a single weird user
        // marketplace would brick every subsequent `aenv add mcp`
        // / `aenv rm github-plugin` etc.
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        std::fs::write(
            tmp.path()
                .join(".claude/plugins/known_marketplaces.json"),
            r#"{"aenv-local":{"source":{"source":"github","repo":"user/their-repo"},"installLocation":"/x","lastUpdated":"2026-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        register_managed_plugins_in_native_json(&env, &[], &std::collections::BTreeSet::new())
            .expect("unrelated apply must not fail on foreign aenv-local");
        // Foreign entry preserved (prune branches require `_aenv`).
        let mkts: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".claude/plugins/known_marketplaces.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            mkts["aenv-local"]["source"]["repo"], "user/their-repo",
            "foreign marketplace must be preserved untouched"
        );
    }

    #[test]
    fn infer_marketplace_non_github_falls_into_aenv_local() {
        // Sources we can't normalize to a real github marketplace
        // (npm:, file://, generic https tarballs, gitlab) bucket
        // into the synthetic `aenv-local` marketplace. Without
        // this, the plugin's fanout dir is on disk but invisible
        // to Claude Code's `cG()` discovery (which requires a
        // `name@marketplace` key in installed_plugins.json), so
        // `aenv ifl` would silently produce a non-functional
        // import.
        for input in [
            "npm:foo@1.0.0",
            "file:///tmp/plugin",
            "https://example.com/plugin.tar.gz",
            "git+https://gitlab.com/foo/bar",
        ] {
            let (mkt, source) = infer_marketplace(Some(input));
            assert_eq!(
                mkt.as_deref(),
                Some(AENV_LOCAL_MARKETPLACE),
                "expected aenv-local for {input}"
            );
            let s = source.expect("synthetic source value present");
            assert_eq!(s["source"], "local");
            assert_eq!(s["originalUrl"], input);
        }
    }

    #[test]
    fn extract_git_sha_returns_fragment_when_present() {
        assert_eq!(
            extract_git_sha(Some("git+https://github.com/owner/repo#abc123")).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            extract_git_sha(Some("https://github.com/owner/repo#abc123")).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            extract_git_sha(Some("git+https://github.com/owner/repo")),
            None
        );
        assert_eq!(extract_git_sha(None), None);
    }

    /// Build a fake Env at a temp dir so we can exercise the JSON
    /// writer without going through fetch + lockfile + manifest.
    /// The Env::manifest() call from the writer isn't invoked here
    /// — we feed registrations directly.
    fn fake_env(tmp: &std::path::Path) -> Env {
        let claude = tmp.join(".claude");
        std::fs::create_dir_all(claude.join("plugins")).unwrap();
        Env {
            name: "test".to_string(),
            root: tmp.to_path_buf(),
            manifest_override: None,
        }
    }

    #[test]
    fn register_managed_plugins_writes_all_three_json_files() {
        // The decisive contract: given one NativeRegistration with
        // a github source, register_managed_plugins_in_native_json
        // produces (1) installed_plugins.json schema-v2 entry,
        // (2) known_marketplaces.json github entry, (3)
        // settings.json::enabledPlugins[name@mkt] = true. Without
        // any of these, Claude Code's `cG()` discovery wouldn't
        // see the plugin.
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        let reg = NativeRegistration {
            plugin_key: "code-review@claude-plugins-official".into(),
            marketplace: Some("claude-plugins-official".into()),
            marketplace_source: Some(serde_json::json!({
                "source": "github",
                "repo": "anthropics/claude-plugins-official",
            })),
            // None here keeps the JSON-write test offline — the
            // marketplace materialization step (which would
            // git-clone the source URL) is skipped on None. A
            // separate test exercises the materialization path
            // with a local source.
            marketplace_repo_url: None,
            install_path: "/abs/plugin/path".into(),
            git_commit_sha: Some("abc123".into()),
            version: None,
        };
        let mut kept = std::collections::BTreeSet::new();
        kept.insert("code-review@claude-plugins-official".to_string());

        register_managed_plugins_in_native_json(&env, &[reg], &kept).unwrap();

        // installed_plugins.json.
        let ip: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".claude/plugins/installed_plugins.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ip["version"], 2);
        let entry = &ip["plugins"]["code-review@claude-plugins-official"][0];
        assert_eq!(entry["scope"], "user");
        assert_eq!(entry["installPath"], "/abs/plugin/path");
        assert_eq!(entry["gitCommitSha"], "abc123");
        assert_eq!(entry["_aenv"], true);
        assert!(entry["version"].is_null() || entry.get("version").is_none());

        // known_marketplaces.json.
        let mkts: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".claude/plugins/known_marketplaces.json"))
                .unwrap(),
        )
        .unwrap();
        let mkt = &mkts["claude-plugins-official"];
        assert_eq!(mkt["source"]["source"], "github");
        assert_eq!(mkt["source"]["repo"], "anthropics/claude-plugins-official");
        assert!(mkt["installLocation"]
            .as_str()
            .unwrap()
            .contains("marketplaces/claude-plugins-official"));
        assert_eq!(mkt["_aenv"], true);

        // settings.json::enabledPlugins.
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["enabledPlugins"]["code-review@claude-plugins-official"],
            true
        );
    }

    #[test]
    fn materialize_marketplace_copies_repo_into_install_location() {
        // The decisive Phase 2-G contract: after registration,
        // `<env>/.claude/plugins/marketplaces/<mkt>/` must contain
        // the marketplace's full repo (specifically its
        // `.claude-plugin/marketplace.json`) so Claude Code can
        // resolve plugin names listed in installed_plugins.json.
        // Without this, the e2e symptom is
        // "Plugin not found in marketplace".
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());

        // Build a fake "marketplace repo" — a directory containing
        // .claude-plugin/marketplace.json. Use it as a local source
        // so we don't hit the network from a unit test.
        let fake_repo = tmp.path().join("fake-mkt-repo");
        std::fs::create_dir_all(fake_repo.join(".claude-plugin")).unwrap();
        std::fs::write(
            fake_repo.join(".claude-plugin/marketplace.json"),
            r#"{"name":"fake-mkt","plugins":[{"name":"foo"}]}"#,
        )
        .unwrap();
        let source = format!("file://{}", fake_repo.display());

        materialize_marketplace_at_install_location(&env, "fake-mkt", &source).unwrap();

        let dst = env
            .claude_dir()
            .join("plugins/marketplaces/fake-mkt/.claude-plugin/marketplace.json");
        assert!(
            dst.is_file(),
            "marketplace.json must be at installLocation: {}",
            dst.display()
        );
        let body = std::fs::read_to_string(&dst).unwrap();
        assert!(body.contains("fake-mkt"), "{body}");
    }

    #[test]
    fn materialize_marketplace_replaces_stale_clone() {
        // Refresh-on-each-install: a pre-existing clone with stale
        // contents must be replaced. We create a marketplace dir
        // with bogus contents, then materialize a fresh source
        // over it, and verify the bogus file is gone and the new
        // marketplace.json is present.
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());

        // Pre-existing stale clone.
        let dst_root = env.claude_dir().join("plugins/marketplaces/mkt");
        std::fs::create_dir_all(&dst_root).unwrap();
        std::fs::write(dst_root.join("stale.txt"), "old content").unwrap();

        // Fresh source.
        let fresh = tmp.path().join("fresh-mkt");
        std::fs::create_dir_all(fresh.join(".claude-plugin")).unwrap();
        std::fs::write(
            fresh.join(".claude-plugin/marketplace.json"),
            r#"{"name":"mkt"}"#,
        )
        .unwrap();
        let source = format!("file://{}", fresh.display());

        materialize_marketplace_at_install_location(&env, "mkt", &source).unwrap();

        assert!(
            !dst_root.join("stale.txt").exists(),
            "stale file must be cleared on refresh"
        );
        assert!(
            dst_root.join(".claude-plugin/marketplace.json").is_file(),
            "fresh marketplace.json must be present"
        );
    }

    #[test]
    fn register_managed_plugins_preserves_user_added_entries() {
        // The other half of "single source of truth" — aenv-managed
        // entries we prune must NOT take user-added entries with
        // them. User entries lack the `_aenv` flag we tag; the
        // prune branch only removes flagged ones.
        let tmp = tempfile::tempdir().unwrap();
        let env = fake_env(tmp.path());
        // Pre-seed installed_plugins.json with a user-added entry
        // (no `_aenv` flag) AND an aenv-managed entry that should
        // get pruned (not in kept_keys below).
        std::fs::write(
            tmp.path().join(".claude/plugins/installed_plugins.json"),
            r#"{
                "version": 2,
                "plugins": {
                    "user-foo@claude-plugins-official": [{
                        "scope": "user",
                        "installPath": "/user/plugin",
                        "version": "1.0.0"
                    }],
                    "stale-aenv@claude-plugins-official": [{
                        "scope": "user",
                        "installPath": "/stale/plugin",
                        "_aenv": true
                    }]
                }
            }"#,
        )
        .unwrap();
        // Pre-seed the stale plugin's fanout dir so prune cleans it.
        let stale_dir = tmp.path().join(".claude/plugins/stale-aenv");
        std::fs::create_dir_all(&stale_dir).unwrap();
        // No registrations this run; kept_keys empty → all aenv
        // entries pruned.
        let kept = std::collections::BTreeSet::new();
        register_managed_plugins_in_native_json(&env, &[], &kept).unwrap();

        let ip: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join(".claude/plugins/installed_plugins.json"))
                .unwrap(),
        )
        .unwrap();
        assert!(
            ip["plugins"]
                .get("user-foo@claude-plugins-official")
                .is_some(),
            "user-added entry must survive: {ip}"
        );
        assert!(
            ip["plugins"]
                .get("stale-aenv@claude-plugins-official")
                .is_none(),
            "aenv-tagged stale entry must be pruned: {ip}"
        );
        // Stale fanout dir also gone.
        assert!(!stale_dir.is_dir(), "stale aenv fanout dir must be removed");
    }

    #[test]
    fn rewrite_env_placeholder_strips_namespace() {
        let out = rewrite_placeholders("dev", "${env:HOME}");
        assert_eq!(out, "${HOME}");
    }

    #[test]
    fn rewrite_unknown_namespace_kept_verbatim() {
        let out = rewrite_placeholders("dev", "${literal:foo}");
        assert_eq!(out, "${literal:foo}");
    }

    #[test]
    fn rewrite_handles_mixed_text() {
        let out = rewrite_placeholders("dev", "prefix-${secret:k}-${env:V}-end");
        assert_eq!(out, "prefix-${AENV_DEV_K}-${V}-end");
    }

    #[test]
    fn rewrite_no_placeholder_pass_through() {
        let out = rewrite_placeholders("dev", "plain");
        assert_eq!(out, "plain");
    }

    #[test]
    fn rewrite_unterminated_placeholder_kept() {
        let out = rewrite_placeholders("dev", "$ {env:incomplete");
        assert!(out.contains("incomplete"));
    }

    #[test]
    fn aenv_secret_var_name_uppercase_underscores() {
        assert_eq!(
            aenv_secret_var_name("my-env", "gh.token"),
            "AENV_MY_ENV_GH_TOKEN"
        );
    }

    #[test]
    fn aenv_secret_var_name_handles_arbitrary_chars() {
        // Spaces, slashes, unicode-like — all become _.
        let v = aenv_secret_var_name("hi there", "weird/key:1");
        assert_eq!(v, "AENV_HI_THERE_WEIRD_KEY_1");
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn aenv_secret_var_name_pads_leading_digit() {
        // POSIX disallows env vars starting with a digit. Even after the
        // AENV_ prefix the *parts* should be safe — defense in depth.
        let v = sanitize_for_env_var("9live");
        assert!(!v.starts_with(char::is_numeric));
        assert_eq!(v, "_9LIVE");
    }

    #[test]
    fn aenv_secret_var_name_empty_components_safe() {
        // Empty inputs must not panic and must yield a valid POSIX env-var name.
        let v = aenv_secret_var_name("", "");
        assert_eq!(v, "AENV____");
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }
}

/// Resolve hashes for every manifest entry and rewrite `aenv.lock` *without*
/// touching the env's `.claude/` tree or `settings.json`. `aenv lock` is the
/// command equivalent of `cargo generate-lockfile`: it can populate the
/// content-addressed store as a side effect of fetching, but the env's own
/// directories are left untouched.
pub fn lock_only(env: &Env) -> Result<()> {
    let manifest = env.manifest()?;
    let mut lock = Lockfile::load_from(&env.lockfile_path())?;
    lock.schema_version = crate::env::manifest::LOCKFILE_SCHEMA_VERSION.to_string();
    lock.generated = Some(chrono::Utc::now());

    let plugin_specs = manifest.plugin_specs()?;
    let skill_specs = manifest.skill_specs()?;

    for spec in &plugin_specs {
        let info = resolve_plugin_hash(spec, &lock)?;
        upsert_locked_plugin(&mut lock, info);
    }
    for spec in &skill_specs {
        let info = resolve_skill_hash(spec, &lock)?;
        upsert_locked_skill(&mut lock, info);
    }

    use std::collections::HashSet;
    let kept_p: HashSet<&str> = plugin_specs.iter().map(|s| s.name.as_str()).collect();
    let kept_s: HashSet<&str> = skill_specs.iter().map(|s| s.name.as_str()).collect();
    lock.plugins.retain(|p| kept_p.contains(p.name.as_str()));
    lock.skills.retain(|s| kept_s.contains(s.name.as_str()));

    lock.save_to(&env.lockfile_path())?;
    Ok(())
}

/// Hash-only resolution path: fetches and inserts into the store, but does
/// not materialize into the env. Reused by `lock_only`.
fn resolve_plugin_hash(spec: &PluginSpec, lock: &Lockfile) -> Result<LockedAny> {
    if let Some(locked) = lock.find_plugin(&spec.name) {
        let want_source = resolve_source(spec).ok();
        let source_matches = want_source.as_deref() == Some(locked.source.as_str());
        let subpath_matches = locked_subpath_matches(spec, locked.subpath.as_deref());
        if source_matches
            && subpath_matches
            && version_matches(spec.version.as_deref(), Some(&locked.version))
            && store::store_path_for(&locked.sha256)?.exists()
        {
            return Ok(LockedAny {
                name: spec.name.clone(),
                version: Some(locked.version.clone()),
                sha256: locked.sha256.clone(),
                source: locked.source.clone(),
                subpath: locked.subpath.clone(),
                requested: locked.requested.clone(),
            });
        }
    }
    let source = resolve_source(spec)?;
    let fetched = store::fetch(&source)
        .with_context(|| format!("fetch plugin '{}' from {source}", spec.name))?;
    let (plugin_root, effective_subpath) =
        plugin_root(&fetched.dir, &spec.name, spec.subpath.as_deref())?;
    validate_plugin_dir(&plugin_root, &spec.name)?;
    let (sha, _) = store::insert(&plugin_root)?;
    let requested = extract_requested_ref(spec.version.as_deref(), Some(&source));
    Ok(LockedAny {
        name: spec.name.clone(),
        version: spec.version.clone(),
        sha256: sha,
        source,
        subpath: effective_subpath,
        requested,
    })
}

/// Reject a plugin source that doesn't carry the required
/// `.claude-plugin/plugin.json` manifest. Without this check, `aenv
/// install` silently accepted any directory the user pointed it at —
/// missing files looked like a successful install until something
/// else (a CI roundtrip, a hook that referenced a non-existent
/// command) failed downstream with a much harder-to-trace error.
///
/// Per the Claude Code plugin spec, `.claude-plugin/plugin.json` is
/// the entry point and must exist at the plugin root. We additionally
/// parse it as JSON so a malformed file fails loudly here, not when
/// claude tries to load the plugin at runtime.
fn validate_plugin_dir(dir: &std::path::Path, plugin_name: &str) -> Result<()> {
    let manifest_path = dir.join(".claude-plugin").join("plugin.json");
    if !manifest_path.is_file() {
        bail!(
            "plugin '{plugin_name}': missing required manifest at \
             {}. Plugin sources must contain a \
             `.claude-plugin/plugin.json` file at their root.",
            manifest_path.display()
        );
    }
    let body = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    serde_json::from_str::<serde_json::Value>(&body)
        .with_context(|| format!("parse plugin manifest {}", manifest_path.display()))?;
    Ok(())
}

fn locked_subpath_matches(spec: &PluginSpec, locked: Option<&str>) -> bool {
    match spec.subpath.as_deref() {
        Some(want) => Some(want) == locked,
        None => true,
    }
}

fn plugin_root(
    source_root: &std::path::Path,
    plugin_name: &str,
    subpath: Option<&str>,
) -> Result<(std::path::PathBuf, Option<String>)> {
    if let Some(subpath) = subpath {
        crate::env::validate_relative_subpath("plugin", subpath)?;
        let root = source_root.join(subpath);
        if !root.is_dir() {
            bail!(
                "plugin subpath '{}' does not exist under {}",
                subpath,
                source_root.display()
            );
        }
        return Ok((root, Some(subpath.to_string())));
    }

    if source_root
        .join(".claude-plugin")
        .join("plugin.json")
        .is_file()
    {
        return Ok((source_root.to_path_buf(), None));
    }

    let inferred = format!("plugins/{plugin_name}");
    let inferred_root = source_root.join(&inferred);
    if inferred_root
        .join(".claude-plugin")
        .join("plugin.json")
        .is_file()
    {
        return Ok((inferred_root, Some(inferred)));
    }

    Ok((source_root.to_path_buf(), None))
}

fn resolve_skill_hash(spec: &SkillSpec, lock: &Lockfile) -> Result<LockedAny> {
    if let Some(locked) = lock.find_skill(&spec.name) {
        let source_matches = spec.source.as_deref() == Some(locked.source.as_str());
        if source_matches && store::store_path_for(&locked.sha256)?.exists() {
            return Ok(LockedAny {
                name: spec.name.clone(),
                version: None,
                sha256: locked.sha256.clone(),
                source: locked.source.clone(),
                subpath: None,
                requested: locked.requested.clone(),
            });
        }
    }
    let source = spec
        .source
        .clone()
        .ok_or_else(|| anyhow!("skill '{}' has no source", spec.name))?;
    let fetched = store::fetch(&source)
        .with_context(|| format!("fetch skill '{}' from {source}", spec.name))?;
    let (sha, _) = store::insert(&fetched.dir)?;
    let requested = extract_requested_ref(None, Some(&source));
    Ok(LockedAny {
        name: spec.name.clone(),
        version: None,
        sha256: sha,
        source,
        subpath: None,
        requested,
    })
}

/// Re-materialize from existing `aenv.lock` (no fetching new versions).
/// True "match the lockfile" semantics: also removes managed plugin dirs and
/// lockfile-absent skill wrappers in the env that aren't referenced by the
/// lockfile. User-installed (non-managed) plugin dirs are preserved via the
/// `.aenv-managed` marker and ad-hoc MCPs via the `_aenv: true` flag in
/// `settings.json::mcpServers`.
pub fn sync(env: &Env) -> Result<Report> {
    let lock = Lockfile::load_from(&env.lockfile_path())?;
    if lock.plugins.is_empty() && lock.skills.is_empty() {
        bail!("aenv.lock is empty — run `aenv install` first");
    }
    // Merge semantics on settings.json::mcpServers preserve ad-hoc
    // /mcp add entries automatically — see write_mcp_servers's
    // `_aenv: true` tagging. No more drift prompt or --force.
    let manifest = env.manifest().ok();

    let mut report = Report::default();
    let plugins_root = env.claude_dir().join("plugins");
    crate::paths::ensure_dir(&plugins_root)?;
    let mut native_registrations: Vec<NativeRegistration> = Vec::new();
    for lp in &lock.plugins {
        let dst = plugins_root.join(&lp.name);
        store::materialize(&lp.sha256, &dst)
            .with_context(|| format!("materialize plugin '{}'", lp.name))?;
        mark_managed(&dst);
        native_registrations.push(NativeRegistration::from_locked_plugin(lp, env));
        report.plugins_installed.push(lp.name.clone());
    }
    for ls in &lock.skills {
        crate::skills::install_skill_into_env(env, &ls.name, &ls.sha256)
            .with_context(|| format!("install skill '{}'", ls.name))?;
        native_registrations.push(NativeRegistration::from_locked_skill(ls, env));
        report.skills_installed.push(ls.name.clone());
    }
    // Remove managed dirs no longer in the lockfile so the env truly matches.
    prune_to_lockfile(env, &lock)?;
    if let Some(m) = &manifest {
        write_mcp_servers(env, m)?;
    }
    // Register every materialized plugin/skill in Claude Code's
    // native JSON files. Without this, sync's hardlink fanout is
    // invisible to claude — same gap install used to have. The
    // kept_keys set drives prune so aenv-tagged stale entries
    // (entries from a previous install that fell out of the
    // lockfile) get cleaned up.
    let kept_keys: std::collections::BTreeSet<String> = native_registrations
        .iter()
        .map(|r| r.plugin_key.clone())
        .collect();
    register_managed_plugins_in_native_json(env, &native_registrations, &kept_keys)?;
    Ok(report)
}

/// Remove `.aenv-managed` plugin/skill-wrapper dirs that aren't referenced
/// by the lockfile. User-installed plugins (no marker) are left alone.
fn prune_to_lockfile(env: &Env, lock: &Lockfile) -> Result<()> {
    use std::collections::HashSet;
    let kept_p: HashSet<&str> = lock.plugins.iter().map(|p| p.name.as_str()).collect();
    let kept_s: HashSet<&str> = lock.skills.iter().map(|s| s.name.as_str()).collect();
    let plugins_root = env.claude_dir().join("plugins");
    let Ok(rd) = std::fs::read_dir(&plugins_root) else {
        return Ok(());
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let is_managed = crate::skills::is_managed(&path);
        let keep = if let Some(skill) = name_str.strip_prefix("skill-") {
            kept_s.contains(skill)
        } else {
            kept_p.contains(name_str.as_ref())
        };
        if !keep && is_managed {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                eprintln!("aenv: warn: sync could not prune {}: {e}", path.display());
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct InstallInfo {
    name: String,
    version: Option<String>,
    sha256: String,
    source: String,
    subpath: Option<String>,
    requested: Option<String>,
    fresh: bool,
}

impl InstallInfo {
    fn into_locked(self) -> LockedAny {
        LockedAny {
            name: self.name,
            version: self.version,
            sha256: self.sha256,
            source: self.source,
            subpath: self.subpath,
            requested: self.requested,
        }
    }
}

/// Extract the user-meaningful "requested" ref from version + source.
/// For Nix-flake-style auditability: this is what the user *asked for*
/// (e.g. "v1.2.0", "main", "abc123"); the sha256 is the resolved
/// content hash. Either alone is insufficient (a tag can be force-pushed,
/// a sha tells a reviewer nothing about the upstream change history).
fn extract_requested_ref(version: Option<&str>, source: Option<&str>) -> Option<String> {
    if let Some(v) = version {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    if let Some(s) = source {
        // git+<url>#ref or git+<url>@ref (legacy)
        if let Some(stripped) = s.strip_prefix("git+") {
            if let Some((_, r)) = stripped.rsplit_once('#') {
                return Some(r.to_string());
            }
            if let Some((_, r)) = stripped.rsplit_once('@') {
                if !r.contains('/') {
                    return Some(r.to_string());
                }
            }
        }
        // npm:pkg@version
        if let Some(stripped) = s.strip_prefix("npm:") {
            if let Some((_, ver)) = stripped.rsplit_once('@') {
                return Some(ver.to_string());
            }
        }
    }
    None
}

#[derive(Debug)]
struct LockedAny {
    name: String,
    version: Option<String>,
    sha256: String,
    source: String,
    subpath: Option<String>,
    /// User-requested ref (tag/branch/commit). Recorded alongside
    /// `sha256` per Nix flake.lock pattern.
    requested: Option<String>,
}

fn install_plugin(env: &Env, spec: &PluginSpec, lock: &Lockfile) -> Result<InstallInfo> {
    let dst = env.claude_dir().join("plugins").join(&spec.name);

    // Cache hit only when version AND source match the lockfile entry —
    // otherwise `aenv add foo --source new && aenv install` would silently
    // keep the old content. Source defaults to npm:<name>@<version> when
    // unspecified, so compute via resolve_source on both sides for stable
    // comparison.
    if let Some(locked) = lock.find_plugin(&spec.name) {
        let want_source = resolve_source(spec).ok();
        let source_matches = want_source.as_deref() == Some(locked.source.as_str());
        let subpath_matches = locked_subpath_matches(spec, locked.subpath.as_deref());
        if source_matches
            && subpath_matches
            && version_matches(spec.version.as_deref(), Some(&locked.version))
            && store::store_path_for(&locked.sha256)?.exists()
        {
            store::materialize(&locked.sha256, &dst)?;
            mark_managed(&dst);
            return Ok(InstallInfo {
                name: spec.name.clone(),
                version: Some(locked.version.clone()),
                sha256: locked.sha256.clone(),
                source: locked.source.clone(),
                subpath: locked.subpath.clone(),
                requested: locked.requested.clone(),
                fresh: false,
            });
        }
    }

    // 2. Need to fetch.
    let source = resolve_source(spec)?;
    let fetched = store::fetch(&source)
        .with_context(|| format!("fetch plugin '{}' from {source}", spec.name))?;
    let (plugin_root, effective_subpath) =
        plugin_root(&fetched.dir, &spec.name, spec.subpath.as_deref())?;
    validate_plugin_dir(&plugin_root, &spec.name)?;
    let (sha, _) = store::insert(&plugin_root)?;
    store::materialize(&sha, &dst)?;
    mark_managed(&dst);
    let requested = extract_requested_ref(spec.version.as_deref(), Some(&source));
    Ok(InstallInfo {
        name: spec.name.clone(),
        version: spec.version.clone(),
        sha256: sha,
        source,
        subpath: effective_subpath,
        requested,
        fresh: true,
    })
}

/// Drop a `.aenv-managed` marker file inside a freshly materialized plugin
/// dir so `prune_to_lockfile` knows it was aenv-installed (and may be
/// pruned when removed from the lockfile). Best-effort — failures don't
/// abort install; the plugin just won't be auto-pruned.
fn mark_managed(plugin_dir: &std::path::Path) {
    let marker = plugin_dir.join(crate::skills::MANAGED_MARKER);
    let _ = std::fs::write(&marker, b"aenv-managed\n");
}

fn install_skill(env: &Env, spec: &SkillSpec, lock: &Lockfile) -> Result<InstallInfo> {
    // Skills are dedup'd in the global store + activated per-env via a generated
    // wrapper plugin (see crate::skills). The store entry is the single source
    // of truth; envs hold only hardlinks via the wrapper.
    //
    // Cache hit only when source matches — otherwise `aenv add skill foo
    // --source new && aenv install` would silently keep the old content.
    if let Some(locked) = lock.find_skill(&spec.name) {
        let source_matches = spec.source.as_deref() == Some(locked.source.as_str());
        if source_matches && store::store_path_for(&locked.sha256)?.exists() {
            crate::skills::install_skill_into_env(env, &spec.name, &locked.sha256)?;
            return Ok(InstallInfo {
                name: spec.name.clone(),
                version: None,
                sha256: locked.sha256.clone(),
                source: locked.source.clone(),
                subpath: None,
                requested: locked.requested.clone(),
                fresh: false,
            });
        }
    }

    let source = spec
        .source
        .clone()
        .ok_or_else(|| anyhow!("skill '{}' has no source", spec.name))?;
    let fetched = store::fetch(&source)
        .with_context(|| format!("fetch skill '{}' from {source}", spec.name))?;
    let (sha, _) = store::insert(&fetched.dir)?;
    crate::skills::install_skill_into_env(env, &spec.name, &sha)?;
    let requested = extract_requested_ref(None, Some(&source));
    Ok(InstallInfo {
        name: spec.name.clone(),
        version: None,
        sha256: sha,
        source,
        subpath: None,
        requested,
        fresh: true,
    })
}

fn resolve_source(spec: &PluginSpec) -> Result<String> {
    if let Some(s) = &spec.source {
        return Ok(s.clone());
    }
    // Fallback heuristic: bare-name plugin → npm package.
    let pkg = &spec.name;
    let version = spec
        .version
        .as_deref()
        .ok_or_else(|| anyhow!("plugin '{pkg}' has no source and no version"))?;
    Ok(format!("npm:{pkg}@{version}"))
}

fn version_matches(want: Option<&str>, got: Option<&str>) -> bool {
    match (want, got) {
        (None, _) => true,
        (Some(w), Some(g)) => w == g,
        _ => false,
    }
}

fn upsert_locked_plugin(lock: &mut Lockfile, x: LockedAny) {
    lock.plugins.retain(|p| p.name != x.name);
    lock.plugins.push(LockedPlugin {
        name: x.name,
        version: x.version.unwrap_or_default(),
        sha256: x.sha256,
        source: x.source,
        subpath: x.subpath,
        requested: x.requested,
        platforms: std::collections::BTreeMap::new(),
    });
    lock.plugins.sort_by(|a, b| a.name.cmp(&b.name));
}

fn upsert_locked_skill(lock: &mut Lockfile, x: LockedAny) {
    lock.skills.retain(|s| s.name != x.name);
    lock.skills.push(LockedSkill {
        name: x.name,
        sha256: x.sha256,
        source: x.source,
        requested: x.requested,
        platforms: std::collections::BTreeMap::new(),
    });
    lock.skills.sort_by(|a, b| a.name.cmp(&b.name));
}
