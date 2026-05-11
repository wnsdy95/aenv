//! `aenv ifl` — Import From List.
//!
//! Interactive multi-select TUI: navigate other envs, drill into each,
//! pick plugins/skills/MCPs, and import the union into the current env.
//! Non-interactive `--from <env> --plugin/--skill/--mcp` form for CI.
//!
//! Design references (each cited verbatim in the relevant function):
//!   * gh CLI's `IOStreams.CanPrompt() == IsStdinTTY() && IsStdoutTTY()`
//!     — https://github.com/cli/cli/blob/trunk/pkg/iostreams/iostreams.go
//!   * rustup's "Unable to run interactively" error wording
//!     — rustup-init.sh
//!   * lazygit's drill-in pane model + space-toggle keybindings
//!     — https://github.com/jesseduffield/lazygit/blob/master/docs/keybindings/Keybindings_en.md
//!   * NO_COLOR / CI / GH_FORCE_TTY / GH_PROMPT_DISABLED env-var conventions
//!     — https://no-color.org/ and gh's help_topic.go
//!
//! Dedup rule: when the same item name is checked across multiple source
//! envs, the FIRST checked wins (per user spec). Conflicts with the
//! TARGET env's existing items are skipped + warned (we don't clobber).

mod tui;

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;

use anyhow::{anyhow, bail, Result};

use crate::cli::IflArgs;
use crate::env::manifest::{Manifest, McpSpec, PluginRef, SkillRef};
use crate::env::Env;

pub fn run(args: IflArgs) -> Result<u8> {
    // The outer GlobalLock used to live here, but ifl now ends in
    // `tx::with_tx` (inside `apply`) which acquires the flock
    // itself; holding it both places would deadlock
    // `Transaction::begin` in the same process. The TUI / flag
    // path does only read-only env enumeration up to the apply
    // step, so we don't need to serialize before then.
    let target = crate::env::open_or_active(args.env.as_deref())?;
    let target_name = target.name.clone();

    // Source envs = every env on disk except the target. Path-hashed
    // project slots are included; the user may want to copy from a
    // local clone of the same repo.
    let sources = list_source_envs(&target_name)?;
    if sources.is_empty() {
        eprintln!(
            "aenv ifl: no other envs to import from. \
             Create one with `aenv new <name>` first."
        );
        return Ok(0);
    }

    // Non-interactive form: when any of --from / --plugin / --skill /
    // --mcp is set, skip the TUI entirely and apply directly. Mirrors
    // kubectl's flag-driven `-i/-t` model and uv's CI-aware refusal.
    let has_flag_input = !args.from_env.is_empty()
        || !args.plugins.is_empty()
        || !args.skills.is_empty()
        || !args.mcps.is_empty();
    if has_flag_input {
        let plan = build_plan_from_flags(&args, &sources)?;
        return apply(&target, plan, &sources);
    }

    // TUI requires stdin AND stdout to be TTYs, per gh's CanPrompt:
    //   "return s.IsStdinTTY() && s.IsStdoutTTY()"
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let ci = std::env::var_os("CI")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);
    let dumb = std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false);
    let no_prompt = std::env::var_os("AENV_NO_PROMPT").is_some();

    if no_prompt || dumb || (!tty && !args.force_tty) || (ci && !args.force_tty) {
        bail!(no_tty_message(tty, ci, dumb, no_prompt));
    }

    // Snapshot the target's current manifest items so the TUI can
    // pre-check them and so unchecking emits removes. Cheaply
    // recovered from the just-loaded target manifest below.
    let target_state = build_target_state(&target);
    let plan = tui::run_tui(&sources, target_state)?;
    if plan.is_empty() {
        eprintln!("aenv ifl: no changes.");
        return Ok(0);
    }
    apply(&target, plan, &sources)
}

/// Read the target env's manifest into a `TargetState` (just the
/// names). Errors are swallowed: a missing/broken manifest means
/// "nothing pinned yet", which is a valid TUI starting state.
fn build_target_state(target: &Env) -> tui::TargetState {
    let mut state = tui::TargetState::default();
    let Ok(m) = target.manifest() else {
        return state;
    };
    if let Ok(specs) = m.plugin_specs() {
        for s in specs {
            state.plugins.insert(s.name);
        }
    }
    if let Ok(specs) = m.skill_specs() {
        for s in specs {
            state.skills.insert(s.name);
        }
    }
    for k in m.mcp.keys() {
        state.mcps.insert(k.clone());
    }
    state
}

/// User-facing error message when the TUI can't run. Adapted from
/// rustup-init.sh's "Unable to run interactively" wording.
fn no_tty_message(tty: bool, ci: bool, dumb: bool, no_prompt: bool) -> String {
    let cause = if no_prompt {
        "AENV_NO_PROMPT is set"
    } else if dumb {
        "TERM=dumb cannot render a TUI"
    } else if ci {
        "detected CI environment (CI=true)"
    } else if !tty {
        "stdin or stdout is not a TTY"
    } else {
        "interactive mode is unavailable"
    };
    format!(
        "aenv ifl: cannot run interactively ({cause}).\n\
         Use the non-interactive form:\n  \
           aenv ifl --from <env> [--plugin <name>]... [--skill <name>]... [--mcp <name>]...\n\
         Or pass --force-tty to override (CI / non-TTY only)."
    )
}

// =====================================================================
//   Core: source enumeration
// =====================================================================

/// One source env's exportable inventory, used by both the TUI and
/// the non-interactive flag-builder.
#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub manifest: Manifest,
}

impl Source {
    pub fn plugin_names(&self) -> Vec<String> {
        self.manifest
            .plugin_specs()
            .map(|specs| specs.into_iter().map(|s| s.name).collect())
            .unwrap_or_default()
    }
    pub fn skill_names(&self) -> Vec<String> {
        self.manifest
            .skill_specs()
            .map(|specs| specs.into_iter().map(|s| s.name).collect())
            .unwrap_or_default()
    }
    pub fn mcp_names(&self) -> Vec<String> {
        self.manifest.mcp.keys().cloned().collect()
    }
}

fn list_source_envs(target_name: &str) -> Result<Vec<Source>> {
    let mut out: Vec<Source> = Vec::new();
    // Synthetic "(global)" source first — covers Claude Code's own
    // ~/.claude state (plugins from installed_plugins.json + MCPs from
    // ~/.claude.json). Listed first so users see it before their own
    // envs in the picker.
    if let Some(global) = build_global_source() {
        out.push(global);
    }
    for summary in Env::list()? {
        if summary.name == target_name || summary.broken {
            continue;
        }
        let env = match Env::open(&summary.name) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let manifest = match env.manifest() {
            Ok(m) => m,
            Err(_) => continue, // skip envs with unparseable manifests
        };
        out.push(Source {
            name: summary.name,
            manifest,
        });
    }
    Ok(out)
}

/// Synthesize a Source from `~/.claude/` state so users can cherry-pick
/// items directly from Claude Code's own install. Currently covers:
///   * **Plugins** — joined from `~/.claude/plugins/installed_plugins.json`
///     (per-plugin `installPath`, `version`, `gitCommitSha`) and
///     `~/.claude/plugins/known_marketplaces.json` (per-marketplace
///     `{source: github, repo: "owner/repo"}`). Reconstructs an
///     authoritative `git+https://github.com/<owner>/<repo>#<sha>` source,
///     falling back to the entry's `installPath` as a `file://` local
///     source when the marketplace isn't github-shaped (covers
///     `/plugin install` from a local path or non-github marketplace).
///   * **MCPs** — `mcpServers` from both `~/.claude/settings.json`
///     (modern Claude Code 2.x — what `/mcp add` writes) and legacy
///     `~/.claude.json` (top-level + per-project). settings.json wins
///     on name collision.
///   * **Skills** — discovered from every layout we ship to:
///     `~/.claude/skills/<name>/SKILL.md` (top-level user skills),
///     `~/.claude/plugins/**/skills/<name>/SKILL.md` (plugin-wrapped),
///     `~/.codex/skills/{,.system}/<name>/SKILL.md`, and
///     `~/.codex/plugins/**/skills/<name>/SKILL.md` (mirror of
///     the claude plugin-wrapped layout).
///
/// Returns None if the user's global Claude/Codex state has nothing
/// importable (so we don't show an empty "(global)" entry in the picker).
fn build_global_source() -> Option<Source> {
    use crate::env::manifest::{
        Manifest, McpSpec, PlatformsBlock, PluginRef, PluginSpec, PluginsBlock, SkillRef,
        SkillsBlock,
    };
    let home = dirs::home_dir()?;
    let claude_dir = home.join(".claude");

    let mut plugins: Vec<PluginRef> = Vec::new();
    let mut skills: BTreeMap<String, SkillRef> = BTreeMap::new();
    let mut mcp: BTreeMap<String, McpSpec> = BTreeMap::new();

    // --- Plugins ---
    if claude_dir.is_dir() {
        let installed_path = claude_dir.join("plugins").join("installed_plugins.json");
        let marketplaces_path = claude_dir.join("plugins").join("known_marketplaces.json");
        if let (Ok(installed), Ok(marketplaces)) = (
            std::fs::read_to_string(&installed_path),
            std::fs::read_to_string(&marketplaces_path),
        ) {
            if let (Ok(installed), Ok(marketplaces)) = (
                serde_json::from_str::<serde_json::Value>(&installed),
                serde_json::from_str::<serde_json::Value>(&marketplaces),
            ) {
                if let Some(plugin_map) = installed.get("plugins").and_then(|v| v.as_object()) {
                    for (key, entries) in plugin_map {
                        // key = "name@marketplace" — split, lookup the
                        // marketplace's repo, attach the gitCommitSha.
                        let (name, marketplace) = match key.split_once('@') {
                            Some((n, m)) => (n, m),
                            None => continue,
                        };
                        let entry = entries
                            .as_array()
                            .and_then(|arr| arr.first())
                            .and_then(|e| e.as_object())?;
                        // Drop non-semver versions (claude code writes "unknown"
                        // when it can't determine a plugin's version, which
                        // would later fail manifest validation's semver
                        // parse). Treat as "no pinned version" — gitCommitSha
                        // still pins the actual content via source URL.
                        let version = entry
                            .get("version")
                            .and_then(|v| v.as_str())
                            .filter(|v| semver::Version::parse(v).is_ok())
                            .map(str::to_string);
                        let git_sha = entry.get("gitCommitSha").and_then(|v| v.as_str());
                        let install_path = entry.get("installPath").and_then(|v| v.as_str());
                        let repo = marketplaces
                            .get(marketplace)
                            .and_then(|m| m.get("source"))
                            .and_then(|s| s.get("repo"))
                            .and_then(|r| r.as_str());
                        let mut subpath = None;
                        // Source restoration order:
                        //   1. marketplace.json → authoritative
                        //      source field (handles git-subdir,
                        //      explicit URL, etc).
                        //   2. github fallback from known_marketplaces
                        //      + installed_plugins.json's
                        //      gitCommitSha — works for plain github
                        //      marketplaces even if marketplace.json
                        //      isn't checked out locally.
                        //   3. installPath as a `file://` local
                        //      source — last resort for plugins
                        //      that came from `/plugin install` of
                        //      a non-github marketplace, or a local
                        //      file path. Without this, ifl would
                        //      surface the plugin with `source = None`
                        //      and apply_live would later reject it.
                        let source = marketplace_plugin_source(
                            &claude_dir,
                            marketplace,
                            name,
                            repo,
                            git_sha,
                            &mut subpath,
                        )
                        .or_else(|| match (repo, git_sha) {
                            (Some(repo), Some(sha)) => {
                                Some(format!("git+https://github.com/{repo}#{sha}"))
                            }
                            (Some(repo), None) => Some(format!("git+https://github.com/{repo}")),
                            _ => None,
                        })
                        .or_else(|| {
                            install_path
                                .filter(|p| std::path::Path::new(p).is_dir())
                                .map(|p| format!("file://{p}"))
                        });
                        if crate::env::validate_resource_name("plugin", name).is_ok() {
                            plugins.push(PluginRef::Detailed(PluginSpec {
                                name: name.to_string(),
                                version,
                                source,
                                subpath,
                                sha256: None,
                                release_url: None,
                                target_map: BTreeMap::new(),
                            }));
                        }
                    }
                }
            }
        }

        // Plugin-wrapped skills under each installed plugin
        // (`~/.claude/plugins/<name@mkt>/<sha>/skills/<name>/SKILL.md`
        // or just `<name>/skills/<skill>/SKILL.md`).
        collect_skill_dirs(
            &claude_dir.join("plugins"),
            5,
            &mut skills,
            SkillDiscovery::ClaudePlugin,
        );
        // Claude Code 2.x ships top-level user skills at
        // `~/.claude/skills/<name>/SKILL.md` (separate from
        // plugin-wrapped skills). They're how /skill install lands
        // by default and a frequent ifl source.
        collect_skill_dirs(
            &claude_dir.join("skills"),
            1,
            &mut skills,
            SkillDiscovery::Plain,
        );
    }

    // --- Skills (codex side) ---
    let codex_dir = home.join(".codex");
    let codex_skills = codex_dir.join("skills");
    collect_skill_dirs(&codex_skills, 1, &mut skills, SkillDiscovery::Plain);
    collect_skill_dirs(
        &codex_skills.join(".system"),
        1,
        &mut skills,
        SkillDiscovery::Plain,
    );
    // Codex mirrors claude's plugin-wrapped skill layout under
    // `~/.codex/plugins/<plugin>/skills/<name>/SKILL.md`. Without
    // this, skills the user activated via a codex marketplace
    // plugin are invisible to (global).
    collect_skill_dirs(
        &codex_dir.join("plugins"),
        5,
        &mut skills,
        SkillDiscovery::ClaudePlugin,
    );

    // --- MCPs ---
    // Source-of-truth order matters here. Modern Claude Code routes
    // `/mcp add` through `<CLAUDE_CONFIG_DIR>/settings.json::mcpServers`,
    // so global = `~/.claude/settings.json`. Legacy `~/.claude.json`
    // still has top-level and per-project `mcpServers` blocks from
    // older installs and is what `aenv add mcp --from claude-code`
    // historically read. We surface both — settings.json first so
    // the modern shape wins on name collisions, legacy filling in
    // anything settings.json doesn't already define.
    let settings_json = claude_dir.join("settings.json");
    if let Ok(body) = std::fs::read_to_string(&settings_json) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            collect_mcp_from_value(v.get("mcpServers"), &mut mcp);
        }
    }
    let claude_json = home.join(".claude.json");
    if let Ok(body) = std::fs::read_to_string(&claude_json) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            // Top-level OR per-project — we union both because
            // claude-code stores per-project config under
            // .projects.<path>.mcpServers as well as top-level.
            // `collect_mcp_from_value` is insert-or-skip, so
            // settings.json entries collected above already won.
            collect_mcp_from_value(v.get("mcpServers"), &mut mcp);
            if let Some(projects) = v.get("projects").and_then(|p| p.as_object()) {
                for proj in projects.values() {
                    collect_mcp_from_value(proj.get("mcpServers"), &mut mcp);
                }
            }
        }
    }

    if plugins.is_empty() && skills.is_empty() && mcp.is_empty() {
        return None;
    }

    let manifest = Manifest {
        schema_version: crate::env::manifest::SCHEMA_VERSION.to_string(),
        env: crate::env::manifest::EnvMeta {
            name: "global".to_string(),
            description: Some("Claude Code's ~/.claude state".to_string()),
            compat: std::collections::BTreeMap::new(),
            created: None,
        },
        platforms: PlatformsBlock::default(),
        mcp,
        plugins: PluginsBlock { enabled: plugins },
        skills: SkillsBlock {
            enabled: skills.into_values().collect(),
        },
        hooks: crate::env::manifest::Hooks::default(),
    };
    Some(Source {
        // Parens distinguish this from a user-named env. Resource-name
        // validation is bypassed because we never write this name to disk.
        name: "(global)".to_string(),
        manifest,
    })
}

#[derive(Debug, Clone, Copy)]
enum SkillDiscovery {
    /// Direct `<root>/<name>/SKILL.md` layout.
    Plain,
    /// Plugin-wrapped layout: any `.../skills/<name>/SKILL.md`
    /// under `root`, regardless of nesting depth. Matches both
    /// `<plugin>/skills/<name>` and `<name@mkt>/<sha>/skills/<name>`.
    ClaudePlugin,
}

fn collect_skill_dirs(
    root: &std::path::Path,
    max_depth: usize,
    out: &mut BTreeMap<String, crate::env::manifest::SkillRef>,
    layout: SkillDiscovery,
) {
    use crate::env::manifest::{SkillRef, SkillSpec};
    if !root.is_dir() {
        return;
    }
    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            match layout {
                SkillDiscovery::Plain => e.depth() == 1 || !name.starts_with('.'),
                SkillDiscovery::ClaudePlugin => name != "node_modules" && name != ".git",
            }
        });
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_dir() {
            continue;
        }
        let skill_dir = entry.path();
        if !skill_dir.join("SKILL.md").is_file() {
            continue;
        }
        if matches!(layout, SkillDiscovery::ClaudePlugin) {
            // Suffix check, not a fixed position. Claude's plugin
            // fanout dirs come in two shapes:
            //   `<plugin>/skills/<skill>/SKILL.md` (legacy)
            //   `<name@mkt>/<sha>/skills/<skill>/SKILL.md` (current,
            //   what `aenv install` writes — sha-pinned per
            //   marketplace plugin).
            // Anchoring on "skills/<skill>" being the trailing two
            // components matches both without re-listing layouts.
            let parent_is_skills = skill_dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("skills");
            if !parent_is_skills {
                continue;
            }
        }
        let Some(name) = skill_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if crate::env::validate_resource_name("skill", name).is_err() {
            continue;
        }
        out.entry(name.to_string()).or_insert_with(|| {
            SkillRef::Detailed(SkillSpec {
                name: name.to_string(),
                source: Some(skill_dir.display().to_string()),
                sha256: None,
                release_url: None,
                target_map: BTreeMap::new(),
            })
        });
    }
}

fn collect_mcp_from_value(
    v: Option<&serde_json::Value>,
    out: &mut BTreeMap<String, crate::env::manifest::McpSpec>,
) {
    use crate::env::manifest::McpSpec;
    let Some(map) = v.and_then(|m| m.as_object()) else {
        return;
    };
    for (name, spec) in map {
        if out.contains_key(name) {
            continue; // first wins (top-level beats per-project)
        }
        if crate::env::validate_resource_name("mcp", name).is_err() {
            continue;
        }
        let s = spec.as_object();
        let mcp = McpSpec {
            transport: s
                .and_then(|o| o.get("type"))
                .and_then(|t| t.as_str())
                .map(str::to_string),
            command: s
                .and_then(|o| o.get("command"))
                .and_then(|c| c.as_str())
                .map(str::to_string),
            args: s
                .and_then(|o| o.get("args"))
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            env: s
                .and_then(|o| o.get("env"))
                .and_then(|e| e.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            url: s
                .and_then(|o| o.get("url"))
                .and_then(|u| u.as_str())
                .map(str::to_string),
            headers: s
                .and_then(|o| o.get("headers"))
                .and_then(|h| h.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            version: None,
        };
        out.insert(name.clone(), mcp);
    }
}

fn marketplace_plugin_source(
    claude_dir: &std::path::Path,
    marketplace: &str,
    plugin_name: &str,
    marketplace_repo: Option<&str>,
    installed_sha: Option<&str>,
    subpath_out: &mut Option<String>,
) -> Option<String> {
    let manifest = claude_dir
        .join("plugins")
        .join("marketplaces")
        .join(marketplace)
        .join(".claude-plugin")
        .join("marketplace.json");
    let body = std::fs::read_to_string(manifest).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let plugin = v
        .get("plugins")
        .and_then(|p| p.as_array())?
        .iter()
        .find(|p| p.get("name").and_then(|n| n.as_str()) == Some(plugin_name))?;
    let source = plugin.get("source")?;
    marketplace_source_to_spec(source, marketplace_repo, installed_sha, subpath_out)
}

fn marketplace_source_to_spec(
    source: &serde_json::Value,
    marketplace_repo: Option<&str>,
    installed_sha: Option<&str>,
    subpath_out: &mut Option<String>,
) -> Option<String> {
    if let Some(s) = source.as_str() {
        if let Some(path) = s.strip_prefix("./") {
            if crate::env::validate_relative_subpath("plugin", path).is_ok() {
                *subpath_out = Some(path.to_string());
            }
            return marketplace_repo.map(|repo| match installed_sha {
                Some(sha) => format!("git+https://github.com/{repo}#{sha}"),
                None => format!("git+https://github.com/{repo}"),
            });
        }
        return normalize_plugin_source_url(s, installed_sha);
    }

    let obj = source.as_object()?;
    let kind = obj.get("source").and_then(|v| v.as_str());
    match kind {
        Some("git-subdir") => {
            let url = obj.get("url").and_then(|v| v.as_str())?;
            if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
                if crate::env::validate_relative_subpath("plugin", path).is_ok() {
                    *subpath_out = Some(path.to_string());
                }
            }
            let rev = obj
                .get("sha")
                .or_else(|| obj.get("commit"))
                .or_else(|| obj.get("ref"))
                .and_then(|v| v.as_str())
                .or(installed_sha);
            normalize_plugin_source_url(url, rev)
        }
        Some("github") => {
            let repo = obj.get("repo").and_then(|v| v.as_str())?;
            let rev = obj
                .get("sha")
                .or_else(|| obj.get("commit"))
                .or_else(|| obj.get("ref"))
                .and_then(|v| v.as_str())
                .or(installed_sha);
            Some(match rev {
                Some(rev) => format!("git+https://github.com/{repo}#{rev}"),
                None => format!("git+https://github.com/{repo}"),
            })
        }
        Some("url") => {
            let url = obj.get("url").and_then(|v| v.as_str())?;
            let rev = obj
                .get("sha")
                .or_else(|| obj.get("commit"))
                .or_else(|| obj.get("ref"))
                .and_then(|v| v.as_str())
                .or(installed_sha);
            normalize_plugin_source_url(url, rev)
        }
        _ => None,
    }
}

fn normalize_plugin_source_url(url: &str, rev: Option<&str>) -> Option<String> {
    let base = if url.starts_with("git+") {
        url.to_string()
    } else if url.ends_with(".git") || url.starts_with("https://github.com/") {
        format!("git+{url}")
    } else {
        url.to_string()
    };
    Some(match rev {
        Some(rev) if !base.contains('#') => format!("{base}#{rev}"),
        _ => base,
    })
}

// =====================================================================
//   Plan building
// =====================================================================

/// What the user wants applied to the target env's manifest.
/// `plugins` / `skills` / `mcps` keys are item names; values are the
/// canonical source env to copy from (FIRST checked across screens
/// wins). The TUI also surfaces "remove this from target" as a
/// negative selection — items the user *unchecked* that are present
/// in the target's manifest. Both directions land in the same Plan
/// so `apply` can do a single transactional save.
#[derive(Debug, Default, Clone)]
pub struct Plan {
    pub plugins: BTreeMap<String, String>, // item-name → source-env-name
    pub skills: BTreeMap<String, String>,
    pub mcps: BTreeMap<String, String>,
    /// Item names the user explicitly unchecked from the target's
    /// existing manifest. Only the TUI populates these — non-
    /// interactive `--from` form is add-only by design (remove is
    /// `aenv rm <kind> <name>`, which has stronger validation).
    pub remove_plugins: BTreeSet<String>,
    pub remove_skills: BTreeSet<String>,
    pub remove_mcps: BTreeSet<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
            && self.skills.is_empty()
            && self.mcps.is_empty()
            && self.remove_plugins.is_empty()
            && self.remove_skills.is_empty()
            && self.remove_mcps.is_empty()
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn total(&self) -> usize {
        self.plugins.len() + self.skills.len() + self.mcps.len()
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn total_removes(&self) -> usize {
        self.remove_plugins.len() + self.remove_skills.len() + self.remove_mcps.len()
    }

    /// First-write-wins insert. Idempotent: re-checking the same item
    /// in the same source is a no-op; checking a duplicate in a
    /// different source is silently ignored (per user spec: "first
    /// checked wins, later checks are ignored").
    pub fn add_plugin(&mut self, name: String, source: &str) {
        self.plugins
            .entry(name)
            .or_insert_with(|| source.to_string());
    }
    pub fn add_skill(&mut self, name: String, source: &str) {
        self.skills
            .entry(name)
            .or_insert_with(|| source.to_string());
    }
    pub fn add_mcp(&mut self, name: String, source: &str) {
        self.mcps.entry(name).or_insert_with(|| source.to_string());
    }
    pub fn remove_plugin(&mut self, name: String) {
        self.remove_plugins.insert(name);
    }
    pub fn remove_skill(&mut self, name: String) {
        self.remove_skills.insert(name);
    }
    pub fn remove_mcp(&mut self, name: String) {
        self.remove_mcps.insert(name);
    }
}

/// Build a Plan from the non-interactive flag form. `--from <env>` is
/// repeatable; each `--plugin/--skill/--mcp` belongs to the most
/// recently seen `--from`. If no item flags are given for a `--from`,
/// every item from that source is imported. (We can't reconstruct the
/// per-`--from` ordering from clap's flat Vecs alone, so the policy
/// simplifies: each item name is matched against ALL listed sources,
/// taking the first source that has it — same first-wins rule the
/// TUI applies.)
fn build_plan_from_flags(args: &IflArgs, sources: &[Source]) -> Result<Plan> {
    if args.from_env.is_empty() {
        bail!(
            "non-interactive ifl needs --from <env>. \
             Example: `aenv ifl --from default --plugin code-review --mcp github`"
        );
    }
    let mut plan = Plan::default();
    let mut seen_any = false;
    for from in &args.from_env {
        let src = sources
            .iter()
            .find(|s| s.name == *from)
            .ok_or_else(|| anyhow!("--from env '{from}' not found"))?;
        let select_all = args.plugins.is_empty() && args.skills.is_empty() && args.mcps.is_empty();
        for name in src.plugin_names() {
            if select_all || args.plugins.iter().any(|n| n == &name) {
                plan.add_plugin(name, &src.name);
                seen_any = true;
            }
        }
        for name in src.skill_names() {
            if select_all || args.skills.iter().any(|n| n == &name) {
                plan.add_skill(name, &src.name);
                seen_any = true;
            }
        }
        for name in src.mcp_names() {
            if select_all || args.mcps.iter().any(|n| n == &name) {
                plan.add_mcp(name, &src.name);
                seen_any = true;
            }
        }
    }
    if !seen_any {
        bail!("no items matched. Check --plugin/--skill/--mcp names.");
    }
    Ok(plan)
}

// =====================================================================
//   Apply
// =====================================================================

/// Execute the plan against the target env. Both directions land in
/// a single manifest save:
///   - `plan.{plugins,skills,mcps}`        → add (skip if already present)
///   - `plan.remove_{plugins,skills,mcps}` → remove (skip if absent)
///
/// Why both in one apply: the TUI surfaces "uncheck → remove" the
/// same way it surfaces "check → add", so a single submit must
/// commit both atomically. Splitting them into two saves would risk
/// half-applied state on a panic / signal between writes.
fn apply(target: &Env, plan: Plan, sources: &[Source]) -> Result<u8> {
    // Look up source manifest by name from the in-memory list. The
    // `(global)` source is synthesized in `build_global_source` and
    // doesn't exist on disk — looking up by name in `sources` works
    // for both real envs and the synthetic global entry. Cloning
    // the manifest pins the source view to TUI-render time, which
    // is what we want — the user picked entries from that snapshot.
    let load = |name: &str| -> Result<Manifest> {
        sources
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.manifest.clone())
            .ok_or_else(|| anyhow!("source env '{name}' not found"))
    };

    // Manifest save + apply_live wrapped in a single tx so a
    // failure during materialization (network, source resolve,
    // settings.json parse) rolls the manifest back to its
    // pre-submit state. Without this envelope the user can be
    // left with aenv.toml mutated but the env unchanged.
    //
    // Re-reading the target manifest INSIDE the closure (= under
    // the global flock) is what defends against a concurrent
    // `aenv add/rm/ifl` clobbering our save with a stale snapshot.
    // The plan's "add" / "remove" semantics absorb concurrent
    // changes safely — already-present adds skip, already-absent
    // removes skip, and unrelated entries the other writer added
    // are preserved because we only touch the (name, kind) pairs
    // the plan explicitly names.
    let env_name = target.name.clone();
    let manifest_path = target.manifest_write_path();
    let captures = vec![
        manifest_path.clone(),
        target.claude_dir().join("plugins"),
        target.claude_dir().join("skills"),
        target.claude_dir().join("settings.json"),
        target.lockfile_path(),
    ];
    let counters = std::cell::Cell::new((0usize, 0usize, 0usize));
    crate::tx::with_tx(
        "ifl",
        Some(&env_name),
        &captures,
        Some(format!("aenv ifl env={env_name}")),
        || {
            let mut manifest = target.manifest()?;
            let existing_plugins: BTreeSet<String> = manifest
                .plugin_specs()
                .map(|specs| specs.into_iter().map(|s| s.name).collect())
                .unwrap_or_default();
            let existing_skills: BTreeSet<String> = manifest
                .skill_specs()
                .map(|specs| specs.into_iter().map(|s| s.name).collect())
                .unwrap_or_default();
            let existing_mcps: BTreeSet<String> = manifest.mcp.keys().cloned().collect();

            let mut added = 0usize;
            let mut skipped = 0usize;
            let mut removed = 0usize;

            for (name, src_name) in &plan.plugins {
                if existing_plugins.contains(name) {
                    skipped += 1;
                    continue;
                }
                let src = load(src_name)?;
                let spec = src
                    .plugin_specs()?
                    .into_iter()
                    .find(|s| &s.name == name)
                    .ok_or_else(|| anyhow!("source env '{src_name}' has no plugin '{name}'"))?;
                manifest.plugins.enabled.push(PluginRef::Detailed(spec));
                added += 1;
            }
            for (name, src_name) in &plan.skills {
                if existing_skills.contains(name) {
                    skipped += 1;
                    continue;
                }
                let src = load(src_name)?;
                let spec = src
                    .skill_specs()?
                    .into_iter()
                    .find(|s| &s.name == name)
                    .ok_or_else(|| anyhow!("source env '{src_name}' has no skill '{name}'"))?;
                manifest.skills.enabled.push(SkillRef::Detailed(spec));
                added += 1;
            }
            for (name, src_name) in &plan.mcps {
                if existing_mcps.contains(name) {
                    skipped += 1;
                    continue;
                }
                let src = load(src_name)?;
                let spec: McpSpec = src
                    .mcp
                    .get(name)
                    .cloned()
                    .ok_or_else(|| anyhow!("source env '{src_name}' has no mcp '{name}'"))?;
                manifest.mcp.insert(name.clone(), spec);
                added += 1;
            }

            for name in &plan.remove_plugins {
                let before = manifest.plugins.enabled.len();
                manifest.plugins.enabled.retain(|p| match p {
                    PluginRef::Detailed(s) => &s.name != name,
                    PluginRef::Short(s) => s.split('@').next() != Some(name.as_str()),
                });
                if manifest.plugins.enabled.len() < before {
                    removed += 1;
                }
            }
            for name in &plan.remove_skills {
                let before = manifest.skills.enabled.len();
                manifest.skills.enabled.retain(|s| match s {
                    SkillRef::Detailed(x) => &x.name != name,
                    SkillRef::Short(s) => s != name,
                });
                if manifest.skills.enabled.len() < before {
                    removed += 1;
                }
            }
            for name in &plan.remove_mcps {
                if manifest.mcp.remove(name).is_some() {
                    removed += 1;
                }
            }

            manifest.save_to(&manifest_path)?;
            crate::install::apply_live(target)?;
            counters.set((added, skipped, removed));
            Ok(())
        },
    )?;
    let (added, skipped, removed) = counters.get();

    let mut summary = format!("aenv ifl: imported {added} item(s) into '{}'", target.name);
    if removed > 0 {
        summary.push_str(&format!(", removed {removed}"));
    }
    if skipped > 0 {
        summary.push_str(&format!(" ({skipped} already present, skipped)"));
    }
    summary.push('.');
    println!("{summary}");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_source(name: &str) -> Source {
        Source {
            name: name.to_string(),
            manifest: Manifest::default_for(name),
        }
    }

    #[test]
    fn plan_first_check_wins_across_sources() {
        let mut plan = Plan::default();
        plan.add_plugin("foo".into(), "alpha");
        plan.add_plugin("foo".into(), "beta"); // duplicate, ignored
        assert_eq!(plan.plugins.get("foo").unwrap(), "alpha");
        assert_eq!(plan.plugins.len(), 1);
    }

    #[test]
    fn plan_total_counts_across_kinds() {
        let mut plan = Plan::default();
        plan.add_plugin("p1".into(), "a");
        plan.add_skill("s1".into(), "a");
        plan.add_mcp("m1".into(), "a");
        plan.add_mcp("m2".into(), "b");
        assert_eq!(plan.total(), 4);
        assert!(!plan.is_empty());
    }

    #[test]
    fn plan_remove_paths_count_in_emptiness_and_total_removes() {
        // A plan that *only* removes is still a meaningful submit —
        // is_empty must reflect removes too, otherwise the TUI would
        // bail "no changes" when the user only unchecked items.
        let mut plan = Plan::default();
        plan.remove_plugin("old".into());
        assert!(!plan.is_empty(), "remove-only plan must not be empty");
        assert_eq!(plan.total(), 0);
        assert_eq!(plan.total_removes(), 1);
    }

    #[test]
    fn plan_round_trip_add_then_remove_same_name_keeps_both_intents() {
        // The Plan structure doesn't try to dedupe across add and
        // remove of the same name — that would silently drop the
        // user's intent. apply() handles each direction in isolation;
        // tests above keep both lists populated and let apply
        // serialize the final state.
        let mut plan = Plan::default();
        plan.add_plugin("foo".into(), "src");
        plan.remove_plugin("foo".into());
        assert!(plan.plugins.contains_key("foo"));
        assert!(plan.remove_plugins.contains("foo"));
    }

    #[test]
    fn build_plan_from_flags_requires_from_env() {
        let args = IflArgs {
            env: None,
            from_env: Vec::new(),
            plugins: vec!["foo".into()],
            skills: Vec::new(),
            mcps: Vec::new(),
            force_tty: false,
        };
        let err = build_plan_from_flags(&args, &[]).unwrap_err();
        assert!(format!("{err}").contains("--from"));
    }

    #[test]
    fn build_plan_from_flags_unknown_env_errors() {
        let args = IflArgs {
            env: None,
            from_env: vec!["ghost".into()],
            plugins: Vec::new(),
            skills: Vec::new(),
            mcps: Vec::new(),
            force_tty: false,
        };
        let sources = vec![empty_source("alpha")];
        let err = build_plan_from_flags(&args, &sources).unwrap_err();
        assert!(format!("{err}").contains("ghost"));
    }
}
