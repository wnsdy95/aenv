use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};

use crate::cli::AddArgs;
use crate::env::manifest::{McpSpec, PluginRef, PluginSpec, SkillRef, SkillSpec};
use crate::env::open_or_active;

pub fn run(args: AddArgs) -> Result<u8> {
    // No outer GlobalLock — `tx::with_tx` below owns the global
    // flock for the whole save+apply window, and `Transaction::begin`
    // re-acquiring it would deadlock against an outer hold from
    // this same process. To avoid a lost-update race against a
    // concurrent `aenv add/rm/ifl`, the manifest read-modify-write
    // happens INSIDE the tx closure (= under the flock), not
    // outside. Reading earlier would let two adds both see the
    // pre-state, with whichever saves second silently overwriting
    // the other.
    let env = open_or_active(args.env.as_deref())?;
    if crate::env::is_global(&env.name) {
        bail!(
            "'global' is the reserved alias for the user's real ~/.claude — it \
             has no aenv.toml to mutate. Create or pick another env: `aenv new <name> && aenv add ...`."
        );
    }
    if !matches!(args.kind.as_str(), "mcp" | "plugin" | "skill") {
        bail!("unknown kind '{}' (mcp | plugin | skill)", args.kind);
    }
    let kind_label = args.kind.clone();
    let name_label = args.name.clone();

    let env_name = env.name.clone();
    let manifest_path = env.manifest_write_path();
    let captures = vec![
        manifest_path.clone(),
        env.claude_dir().join("plugins"),
        env.claude_dir().join("skills"),
        env.claude_dir().join("settings.json"),
        env.lockfile_path(),
    ];
    let audit = if name_label.is_empty() {
        format!("aenv add {kind_label} env={env_name}")
    } else {
        format!("aenv add {kind_label} {name_label} env={env_name}")
    };
    crate::tx::with_tx("add", Some(&env_name), &captures, Some(audit), move || {
        let mut manifest = env.manifest()?;
        match args.kind.as_str() {
            "mcp" => add_mcp(&mut manifest, args)?,
            "plugin" => add_plugin(&mut manifest, args)?,
            "skill" => add_skill(&mut manifest, args)?,
            // Kind validated above the closure; unreachable here
            // unless a future caller wires this differently.
            other => bail!("unknown kind '{other}' (mcp | plugin | skill)"),
        }
        manifest.save_to(&manifest_path)?;
        crate::install::apply_live(&env)?;
        Ok(())
    })?;
    Ok(0)
}

fn add_mcp(manifest: &mut crate::env::Manifest, args: AddArgs) -> Result<()> {
    // Phase 2: bulk import from existing config / deeplink. This branch
    // ignores the positional `name` and the per-server flags — each
    // imported entry brings its own.
    if let Some(source) = args.from.as_deref() {
        let entries = crate::mcp_import::import_from(source).with_context(|| {
            format!(
                "import mcp servers from '{source}' \
                 (try: claude-desktop | claude-code | cursor | \
                 cursor-deeplink:<url> | vscode-deeplink:<url> | <path-to-json>)"
            )
        })?;
        if entries.is_empty() {
            eprintln!("aenv: no servers found at '{source}'");
            return Ok(());
        }
        for (name, spec) in entries {
            crate::env::validate_resource_name("mcp", &name)?;
            manifest.mcp.insert(name.clone(), spec);
            println!("added mcp.{name}");
        }
        return Ok(());
    }

    // Phase 1: single-server add — three sub-grammars matching
    // `claude mcp add` / `claude mcp add-json`:
    //   1. `--json '<json>'`           inline claude-desktop-config-shape JSON
    //   2. `--transport http <url>`    HTTP/SSE transport
    //   3. trailing argv after `--`    stdio: `aenv add mcp <name> -- <cmd> [args...]`
    //   4. legacy `--command/--args`   kept for backward compat (deprecated)

    if args.name.is_empty() {
        bail!(
            "name is required for `aenv add mcp` (or pass `--from <source>` \
             for bulk import)"
        );
    }
    crate::env::validate_resource_name("mcp", &args.name)?;

    let env_map = parse_env_var_list(&args.env_var)?;
    let header_map = parse_header_list(&args.header)?;

    let spec = if let Some(json) = args.json.as_deref() {
        let mut spec = parse_mcp_json(json)
            .with_context(|| format!("parse --json for mcp '{}'", args.name))?;
        // Merge in any -e flags the user specified alongside --json.
        for (k, v) in env_map {
            spec.env.insert(k, v);
        }
        for (k, v) in header_map {
            spec.headers.insert(k, v);
        }
        spec
    } else if matches!(args.transport.as_deref(), Some("http") | Some("sse")) {
        // HTTP/SSE: positional after name (or --url) is the URL.
        let url = args
            .url
            .clone()
            .or_else(|| {
                // Treat the first trailing-argv element as the URL if --url
                // wasn't passed. Mirrors `claude mcp add --transport http
                // notion https://mcp.notion.com/mcp`.
                args.argv.first().cloned()
            })
            .ok_or_else(|| {
                anyhow!(
                    "--transport {} requires a URL (`--url <url>` or positional)",
                    args.transport.as_deref().unwrap_or("http")
                )
            })?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("http/sse URL must start with http:// or https://, got '{url}'");
        }
        McpSpec {
            transport: args.transport.clone(),
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some(url),
            headers: header_map,
            version: None,
        }
    } else if !args.argv.is_empty() {
        // Modern stdio form: `aenv add mcp <name> [-e K=V]... -- <cmd> [args...]`
        let mut argv = args.argv.into_iter();
        let command = argv.next().unwrap();
        let cmd_args: Vec<String> = argv.collect();
        McpSpec {
            transport: None, // stdio is the implicit default
            command: Some(command),
            args: cmd_args,
            env: env_map,
            url: None,
            headers: BTreeMap::new(),
            version: None,
        }
    } else if args.command.is_some() {
        // Legacy form: `--command X --arg=Y --arg=Z --env-var K=V`. Still
        // valid, but the `--` form above is preferred and is what
        // `aenv add mcp --help` documents.
        eprintln!(
            "aenv: hint: prefer `aenv add mcp {} -- {} ...` over `--command/--arg`",
            args.name,
            args.command.as_deref().unwrap_or("CMD")
        );
        McpSpec {
            transport: None,
            command: args.command,
            args: args.args,
            env: env_map,
            url: None,
            headers: BTreeMap::new(),
            version: None,
        }
    } else {
        bail!(
            "no command/url given. Examples:\n  \
             aenv add mcp {name} -- npx -y @scope/server-{name}\n  \
             aenv add mcp {name} --transport http https://example.com/mcp\n  \
             aenv add mcp {name} --json '{{\"command\":\"...\"}}'\n  \
             aenv add mcp --from claude-desktop",
            name = args.name
        );
    };

    manifest.mcp.insert(args.name.clone(), spec);
    println!("added mcp.{}", args.name);
    Ok(())
}

fn add_plugin(manifest: &mut crate::env::Manifest, args: AddArgs) -> Result<()> {
    let name_for_validation = args
        .name
        .rsplit_once('@')
        .map(|(n, _)| n)
        .unwrap_or(&args.name);
    crate::env::validate_resource_name("plugin", name_for_validation)?;
    let (name, version) = match args.name.rsplit_once('@') {
        Some((n, v)) => (n.to_string(), Some(v.to_string())),
        None => (args.name.clone(), None),
    };
    if let Some(subpath) = &args.subpath {
        crate::env::validate_relative_subpath("plugin", subpath)?;
    }
    let spec = PluginSpec {
        name: name.clone(),
        version,
        source: args.source,
        subpath: args.subpath,
        sha256: None,
        release_url: None,
        target_map: BTreeMap::new(),
    };
    manifest.plugins.enabled.retain(|p| match p {
        PluginRef::Detailed(s) => s.name != name,
        PluginRef::Short(s) => s.split('@').next() != Some(&name),
    });
    manifest.plugins.enabled.push(PluginRef::Detailed(spec));
    println!("added plugin {}", args.name);
    Ok(())
}

fn add_skill(manifest: &mut crate::env::Manifest, args: AddArgs) -> Result<()> {
    crate::env::validate_resource_name("skill", &args.name)?;
    let spec = SkillSpec {
        name: args.name.clone(),
        source: args.source,
        sha256: None,
        release_url: None,
        target_map: BTreeMap::new(),
    };
    manifest.skills.enabled.retain(|s| match s {
        SkillRef::Detailed(x) => x.name != args.name,
        SkillRef::Short(s) => s != &args.name,
    });
    manifest.skills.enabled.push(SkillRef::Detailed(spec));
    println!("added skill {}", args.name);
    Ok(())
}

fn parse_env_var_list(items: &[String]) -> Result<BTreeMap<String, String>> {
    items
        .iter()
        .map(|s| {
            // Split on FIRST `=` only — values can contain `=` (e.g. URLs,
            // base64). Mirrors `docker run -e K=V` and `kubectl run --env`.
            s.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| anyhow!("env var must be KEY=VALUE: {s}"))
        })
        .collect()
}

fn parse_header_list(items: &[String]) -> Result<BTreeMap<String, String>> {
    items
        .iter()
        .map(|s| {
            s.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .ok_or_else(|| anyhow!("header must be `Name: Value`: {s}"))
        })
        .collect()
}

/// Parse the JSON form of `claude mcp add-json`:
///   `{"type":"stdio","command":"...","args":[...],"env":{"K":"V"}}`
///   `{"type":"http","url":"...","headers":{"K":"V"}}`
fn parse_mcp_json(body: &str) -> Result<McpSpec> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    }
    let raw: Raw = serde_json::from_str(body).context("invalid JSON")?;
    Ok(McpSpec {
        transport: raw.kind,
        command: raw.command,
        args: raw.args,
        env: raw.env,
        url: raw.url,
        headers: raw.headers,
        version: None,
    })
}
