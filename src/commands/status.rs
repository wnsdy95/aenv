use anyhow::Result;
use serde::Serialize;

use crate::cli::StatusArgs;
use crate::env::{open_or_active, Env};

#[derive(Serialize)]
struct Status {
    name: String,
    root: std::path::PathBuf,
    /// Per-tool config dir the shim points the backend at. For claude,
    /// `<env>/.claude/` (= `CLAUDE_CONFIG_DIR`); for codex, `<env>/codex/`
    /// (= `CODEX_HOME`). Reported alongside the env root so users can
    /// confirm at a glance that isolation lands where they expect.
    claude_config_dir: std::path::PathBuf,
    codex_home: std::path::PathBuf,
    /// Manifest-pinned plugins (the reproducible set). `aenv install`
    /// is what makes these visible to claude.
    plugins_pinned: Vec<String>,
    /// Plugins present in `installed_plugins.json` (= what Claude
    /// Code actually sees) but NOT pinned in `aenv.toml`. These came
    /// from the user running `/plugin install` inside claude. Won't
    /// reproduce on a teammate's `aenv install` — promote with
    /// `aenv ifl` if intended.
    plugins_adhoc: Vec<String>,
    mcp: Vec<String>,
    skills: Vec<String>,
    /// `$AENV` set in the current shell — true when this shell is
    /// already inside a launched aenv-managed CLI session.
    active_in_shell: bool,
}

pub fn run(args: StatusArgs) -> Result<u8> {
    let env: Env = open_or_active(args.env.as_deref())?;
    let manifest_result = env.manifest();
    if let Err(e) = &manifest_result {
        eprintln!("aenv: manifest error: {e}");
    }
    let manifest = manifest_result.ok();

    let plugins_pinned: Vec<String> = manifest
        .as_ref()
        .map(|m| {
            m.plugin_specs()
                .map(|v| v.into_iter().map(|s| s.name).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let plugins_adhoc: Vec<String> = collect_adhoc_plugins(&env, &plugins_pinned);

    let s = Status {
        name: env.name.clone(),
        root: env.root.clone(),
        claude_config_dir: env.claude_dir(),
        codex_home: crate::backend::codex::codex_home_for(&env),
        plugins_pinned,
        plugins_adhoc,
        mcp: manifest
            .as_ref()
            .map(|m| m.mcp.keys().cloned().collect())
            .unwrap_or_default(),
        skills: manifest
            .as_ref()
            .map(|m| {
                m.skill_specs()
                    .map(|v| v.into_iter().map(|s| s.name).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default(),
        active_in_shell: std::env::var_os("AENV_ACTIVE").is_some(),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        println!("env:               {}", s.name);
        println!("root:              {}", s.root.display());
        println!(
            "CLAUDE_CONFIG_DIR: {} (claude routes here)",
            s.claude_config_dir.display()
        );
        println!(
            "CODEX_HOME:        {} (codex routes here)",
            s.codex_home.display()
        );
        println!("active in shell:   {}", s.active_in_shell);
        println!(
            "plugins pinned ({}):  {}",
            s.plugins_pinned.len(),
            s.plugins_pinned.join(", ")
        );
        if !s.plugins_adhoc.is_empty() {
            println!(
                "plugins ad-hoc ({}): {}    ← /plugin install in claude, not in aenv.toml",
                s.plugins_adhoc.len(),
                s.plugins_adhoc.join(", ")
            );
        }
        println!("mcp ({}):            {}", s.mcp.len(), s.mcp.join(", "));
        println!(
            "skills ({}):         {}",
            s.skills.len(),
            s.skills.join(", ")
        );
    }
    Ok(0)
}

/// Read `<env>/.claude/plugins/installed_plugins.json` and return
/// the names of plugins that are NOT in the manifest's pinned set.
/// These are the user's `/plugin install` additions — surfaced so
/// the user can decide whether to promote (`aenv ifl`) or accept
/// them as env-local.
fn collect_adhoc_plugins(env: &Env, pinned: &[String]) -> Vec<String> {
    let path = crate::backend::claude::installed_plugins::path_for(env);
    let Ok(doc) = crate::backend::claude::installed_plugins::read(&path) else {
        return Vec::new();
    };
    let pinned_bare: std::collections::HashSet<&str> = pinned.iter().map(String::as_str).collect();
    doc.plugins
        .keys()
        .filter(|key| {
            // Drop aenv's synthetic skill marketplace; those are
            // surfaced under the `skills` line, not `plugins`.
            !key.ends_with("@aenv-skills") && {
                let bare = key.split('@').next().unwrap_or(key);
                !pinned_bare.contains(bare)
            }
        })
        .cloned()
        .collect()
}
