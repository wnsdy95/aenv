use anyhow::Result;
use serde::Serialize;

use crate::backend::claude::shim;
use crate::cli::DoctorArgs;
use crate::env::{open_or_active, Env};
use crate::paths;

#[derive(Default, Serialize)]
struct Report {
    home: Option<std::path::PathBuf>,
    shim_path: std::path::PathBuf,
    shim_ok: bool,
    path_order_ok: bool,
    real_claude: Option<std::path::PathBuf>,
    real_claude_version: Option<String>,
    env: Option<EnvReport>,
    checks: Vec<Check>,
    warnings: u32,
    errors: u32,
}

#[derive(Serialize)]
struct EnvReport {
    name: String,
    root: std::path::PathBuf,
    /// Required range from `[env].compat.claude` (renamed from the
    /// pre-pivot `cc_compatible` field). Field name stays `cc_*` in the
    /// JSON report so `aenv doctor --json` consumers don't break across
    /// the 0.3.0 cut — only the manifest source moved.
    cc_compatible: Option<String>,
    cc_version: Option<String>,
    cc_compatible_ok: Option<bool>,
    /// Counts of plugins by origin: aenv-generated wrappers vs user-installed.
    plugins_managed: usize,
    plugins_user: usize,
}

#[derive(Serialize)]
struct Check {
    level: &'static str,
    message: String,
}

pub fn run(args: DoctorArgs) -> Result<u8> {
    let mut r = Report::default();
    let push_ok = |r: &mut Report, m: String| {
        r.checks.push(Check {
            level: "ok",
            message: m,
        });
    };
    let push_warn = |r: &mut Report, m: String| {
        r.warnings += 1;
        r.checks.push(Check {
            level: "warn",
            message: m,
        });
    };
    let push_err = |r: &mut Report, m: String| {
        r.errors += 1;
        r.checks.push(Check {
            level: "err",
            message: m,
        });
    };

    let home = paths::aenv_home()?;
    if home.is_dir() {
        push_ok(&mut r, format!("aenv home: {}", home.display()));
        r.home = Some(home.clone());
    } else {
        push_err(&mut r, "aenv home missing: run `aenv init`".into());
    }

    // Surface pending transactions — these mean an `aenv install` (or similar)
    // was killed mid-operation and disk state may be partially mutated. The
    // user can recover with `aenv rollback --pending` or accept the state.
    let pending = crate::tx::count_pending();
    if pending > 0 {
        push_warn(
            &mut r,
            format!(
                "{pending} pending transaction(s) found — likely from a killed \
                 process. Recover with `aenv rollback --pending` or inspect via \
                 `aenv history`."
            ),
        );
    }

    // On Windows the shim is `claude.exe`; on Unix it's `claude`.
    // Hardcoding `"claude"` here made `aenv doctor` report the shim as
    // missing on every Windows install even when init had succeeded.
    let shim_path = paths::shims_dir()?.join(shim::claude_binary_name());
    r.shim_path = shim_path.clone();
    if shim_path.exists() {
        let exe = std::env::current_exe()?;
        #[cfg(unix)]
        {
            match std::fs::read_link(&shim_path) {
                Ok(t) if t == exe => {
                    r.shim_ok = true;
                    push_ok(&mut r, format!("shim symlink ok: {}", shim_path.display()));
                }
                Ok(t) => push_warn(
                    &mut r,
                    format!(
                        "shim points to {} (this binary is {})",
                        t.display(),
                        exe.display()
                    ),
                ),
                Err(_) => push_warn(
                    &mut r,
                    format!("shim is not a symlink: {}", shim_path.display()),
                ),
            }
        }
        #[cfg(windows)]
        {
            // Windows can't symlink without admin/dev-mode, so the shim
            // is a copy of this binary. We can't readlink — compare
            // size + mtime instead. Mismatch likely means the user
            // upgraded `aenv` and forgot to rerun `aenv init --force`.
            let sm = std::fs::metadata(&shim_path).ok();
            let em = std::fs::metadata(&exe).ok();
            match (sm, em) {
                (Some(s), Some(e)) if s.len() == e.len() => {
                    r.shim_ok = true;
                    push_ok(&mut r, format!("shim copy ok: {}", shim_path.display()));
                }
                (Some(_), Some(_)) => push_warn(
                    &mut r,
                    format!(
                        "shim at {} differs in size from this binary {} — \
                         rerun `aenv init --force` after upgrading aenv",
                        shim_path.display(),
                        exe.display()
                    ),
                ),
                _ => push_warn(
                    &mut r,
                    format!(
                        "could not stat shim or current exe at {}",
                        shim_path.display()
                    ),
                ),
            }
        }
    } else {
        push_err(
            &mut r,
            format!("shim missing at {}; run `aenv init`", shim_path.display()),
        );
    }

    if let Some(path) = std::env::var_os("PATH") {
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        let shim_idx = dirs
            .iter()
            .position(|d| d == &paths::shims_dir().unwrap_or_default());
        let real = shim::locate_real_claude().ok();
        r.real_claude = real.clone();
        let real_idx = real
            .as_ref()
            .and_then(|p| p.parent())
            .and_then(|d| dirs.iter().position(|x| x == d));
        match (shim_idx, real_idx) {
            (Some(s), Some(re)) if s < re => {
                r.path_order_ok = true;
                push_ok(&mut r, "PATH order: shim before real claude".into());
            }
            (Some(_), Some(_)) => push_warn(
                &mut r,
                "PATH has shim AFTER real claude; eval shell-init in your rc".into(),
            ),
            (None, _) => push_warn(&mut r, "aenv shims dir not in PATH; eval shell-init".into()),
            // Real claude was located but its parent dir isn't in
            // PATH — only happens via the cache fallback in
            // `locate_real_claude`. Common, harmless case for users
            // who installed claude via Anthropic's macOS app (binary
            // lives in ~/Library/Application Support/...). Emit OK +
            // path so the user can confirm.
            (Some(_), None) if r.real_claude.is_some() => {
                r.path_order_ok = true;
                let display = r
                    .real_claude
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                push_ok(
                    &mut r,
                    format!(
                        "real claude resolved via cached path: {display} \
                         (binary's parent dir is outside $PATH — normal for \
                         macOS Library install)"
                    ),
                );
            }
            (Some(_), None) => push_warn(&mut r, "real claude not found in PATH".into()),
        }
    }

    // Stale-cache detection: compare what's in `~/.aenv/config.toml`
    // (`real_claude` field) against a fresh PATH walk. If they differ,
    // the user upgraded claude (or installed a newer one in a different
    // dir) and cache is now pointing at an old binary that's still
    // technically valid but probably not what the user expects to run.
    // Cache-only (no PATH match) is fine — that's the macOS Library
    // install case. PATH-only (no cache yet) is fine — `aenv init`
    // hasn't run since install. Both-present-and-different is the
    // problem case.
    if let Some(cached) = crate::resolve::GlobalConfig::load()
        .ok()
        .and_then(|c| c.real_claude)
    {
        let path_walked = walk_path_for_claude();
        match path_walked {
            Some(walk) if !same_path(&walk, &cached) => {
                push_warn(
                    &mut r,
                    format!(
                        "config.toml's real_claude ({}) differs from what $PATH \
                         resolves to ({}). Run `aenv init --force` to refresh \
                         the cache, or remove the entry from {} if you want \
                         PATH-only resolution.",
                        cached.display(),
                        walk.display(),
                        paths::config_path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ),
                );
            }
            _ => {}
        }
    }

    if let Some(claude_bin) = &r.real_claude {
        r.real_claude_version = detect_version(claude_bin);
    }

    let target_name = args.env.clone();
    let env: Option<Env> = match target_name {
        Some(n) => Some(Env::open(&n)?),
        None => open_or_active(None).ok(),
    };
    if let Some(env) = env {
        let mut er = EnvReport {
            name: env.name.clone(),
            root: env.root.clone(),
            cc_compatible: None,
            cc_version: r.real_claude_version.clone(),
            cc_compatible_ok: None,
            plugins_managed: 0,
            plugins_user: 0,
        };
        let plugins_dir = env.claude_dir().join("plugins");
        if let Ok(rd) = std::fs::read_dir(&plugins_dir) {
            for entry in rd.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    if crate::skills::is_managed(&entry.path()) {
                        er.plugins_managed += 1;
                    } else {
                        er.plugins_user += 1;
                    }
                }
            }
            push_ok(
                &mut r,
                format!(
                    "plugins: {} managed (aenv), {} user",
                    er.plugins_managed, er.plugins_user
                ),
            );
        }
        push_ok(
            &mut r,
            format!("env: {} ({})", env.name, env.root.display()),
        );

        for (label, p) in [
            ("CLAUDE_CONFIG_DIR", env.claude_dir()),
            ("XDG_CONFIG_HOME", env.xdg(paths::XdgKind::Config)),
            ("XDG_DATA_HOME", env.xdg(paths::XdgKind::Data)),
            ("XDG_STATE_HOME", env.xdg(paths::XdgKind::State)),
            ("XDG_CACHE_HOME", env.xdg(paths::XdgKind::Cache)),
        ] {
            if p.is_dir() {
                push_ok(&mut r, format!("{label}: {}", p.display()));
            } else {
                push_warn(&mut r, format!("{label} dir missing: {}", p.display()));
            }
        }

        match env.manifest() {
            Ok(m) => {
                push_ok(&mut r, "manifest parseable + valid".into());
                er.cc_compatible = m.env.compat.get("claude").cloned();
                if let (Some(range_str), Some(ver_str)) = (&er.cc_compatible, &er.cc_version) {
                    if let (Ok(range), Ok(ver)) = (
                        semver::VersionReq::parse(range_str),
                        semver::Version::parse(ver_str.trim_start_matches('v')),
                    ) {
                        let ok = range.matches(&ver);
                        er.cc_compatible_ok = Some(ok);
                        if ok {
                            push_ok(
                                &mut r,
                                format!("compat.claude '{range_str}' matches Claude Code {ver}"),
                            );
                        } else {
                            push_warn(
                                &mut r,
                                format!(
                                    "compat.claude '{range_str}' does NOT match Claude Code {ver}"
                                ),
                            );
                        }
                    }
                }

                // installed_plugins.json schema check + drift surface.
                // Claude Code 2.1.138 reads this file as the single
                // source of truth for plugin discovery (`cG()`); a
                // schema mismatch or a manifest-pinned plugin missing
                // from this file means claude won't see the plugin.
                check_installed_plugins_drift(&mut r, &env, &m);
            }
            Err(e) => push_warn(&mut r, format!("manifest: {e}")),
        }

        r.env = Some(er);
    } else {
        push_warn(&mut r, "no env active for doctor checks".into());
    }

    // Cross-platform sharing checks: only run when we're inside a
    // project-mode workspace (cwd has aenv.toml). These warn about
    // configurations that produce different bytes on macOS/Linux/
    // Windows checkouts and would break aenv.lock's sha verification.
    let cwd = std::env::current_dir().ok();
    let project_manifest = cwd.as_deref().and_then(crate::paths::find_project_manifest);
    if let (Some(manifest_path), Some(cwd_path)) = (project_manifest, cwd) {
        run_cross_platform_checks(&mut r, &manifest_path, &cwd_path);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        for c in &r.checks {
            let tag = match c.level {
                "ok" => "[ok]  ",
                "warn" => "[warn]",
                "err" => "[err] ",
                _ => "[?]   ",
            };
            println!("{tag} {}", c.message);
        }
        println!();
        println!("doctor: {} warnings, {} errors", r.warnings, r.errors);
    }
    Ok(if r.errors > 0 { 1 } else { 0 })
}

/// PATH-only walk for claude (no cache fallback) — used by the
/// stale-cache detector so we can compare cache against PATH.
fn walk_path_for_claude() -> Option<std::path::PathBuf> {
    let shims = paths::shims_dir().ok();
    let path = std::env::var_os("PATH")?;
    let target = shim::claude_binary_name();
    for dir in std::env::split_paths(&path) {
        if let Some(s) = &shims {
            if dir == *s {
                continue;
            }
        }
        let candidate = dir.join(target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Cross-platform sharing checks. These run inside project-mode
/// workspaces (those with `aenv.toml` in cwd or an ancestor) and warn
/// about configurations that produce different bytes on different OSes
/// — which break aenv.lock's sha256 verification round-trip.
///
/// Sources for what to check:
///   * Git's own gitattributes spec — https://git-scm.com/docs/gitattributes
///   * GitHub's cross-platform line-endings guide — https://docs.github.com/en/get-started/getting-started-with-git/configuring-git-to-handle-line-endings
///   * pre-commit-hooks repo (mixed-line-ending, fix-byte-order-marker,
///     check-symlinks) — https://github.com/pre-commit/pre-commit-hooks
///   * Real-world `.gitattributes` patterns from Go (`* -text` whole-
///     repo) and Node.js / Rust / VS Code.
fn run_cross_platform_checks(
    r: &mut Report,
    manifest_path: &std::path::Path,
    cwd: &std::path::Path,
) {
    fn warn(r: &mut Report, msg: String) {
        r.warnings += 1;
        r.checks.push(Check {
            level: "warn",
            message: msg,
        });
    }
    fn err(r: &mut Report, msg: String) {
        r.errors += 1;
        r.checks.push(Check {
            level: "err",
            message: msg,
        });
    }

    // 1. core.autocrlf detection. On Windows, default is `true` and
    //    will rewrite LF → CRLF on checkout — breaks sha verification
    //    unless `aenv.lock` is marked `-text` in `.gitattributes`.
    let autocrlf = std::process::Command::new("git")
        .args(["config", "--get", "core.autocrlf"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let host_is_windows = cfg!(target_os = "windows");
    if autocrlf == "true" && !host_is_windows {
        warn(
            r,
            "git core.autocrlf=true on macOS/Linux: rewrites LF→CRLF on checkin. \
             Per GitHub's guide, set to `input` instead: \
             `git config --global core.autocrlf input`."
                .into(),
        );
    }

    // 2. .gitattributes presence + coverage of aenv files. The single
    //    most important fix — same pattern Go's repo uses for ALL files.
    let gitattrs = cwd.join(".gitattributes");
    let attrs_body = std::fs::read_to_string(&gitattrs).unwrap_or_default();
    let aenv_lock_protected = attrs_body.contains("aenv.lock")
        && (attrs_body.contains("-text") || attrs_body.contains("binary"));
    let aenv_toml_protected = attrs_body.contains("aenv.toml")
        && (attrs_body.contains("-text") || attrs_body.contains("binary"));
    if !attrs_body.is_empty() {
        if !aenv_lock_protected || !aenv_toml_protected {
            warn(
                r,
                format!(
                    "{} exists but doesn't mark aenv.toml/aenv.lock as `-text`. \
                 A Windows checkout with core.autocrlf=true will rewrite line \
                 endings and break sha verification. Add:\n\
                 \taenv.toml   -text\n\
                 \taenv.lock   -text",
                    gitattrs.display()
                ),
            );
        }
    } else {
        warn(
            r,
            format!(
                "{} missing. Run `aenv init --here` to (re)generate, or add:\n\
             \taenv.toml   -text\n\
             \taenv.lock   -text",
                gitattrs.display()
            ),
        );
    }

    // 3. Manifest source portability. file:// or absolute paths can't
    //    survive a `git clone` to another machine.
    if let Ok(body) = std::fs::read_to_string(manifest_path) {
        for (lineno, line) in body.lines().enumerate() {
            let lower = line.trim_start();
            if !lower.starts_with("source") && !lower.starts_with("release_url") {
                continue;
            }
            // Cheap heuristic: extract the "..." value. Avoids pulling
            // in toml::Value just for this check.
            if line.contains("\"file://") {
                err(
                    r,
                    format!(
                        "{}:{}: source uses file:// — non-portable. Use git+https://, \
                     https://, or npm:.",
                        manifest_path.display(),
                        lineno + 1
                    ),
                );
            } else if line.contains("\"/") || line.contains("\"C:\\\\") || line.contains("\"C:/") {
                err(
                    r,
                    format!(
                        "{}:{}: source uses absolute path — non-portable across machines.",
                        manifest_path.display(),
                        lineno + 1
                    ),
                );
            }
        }
    }

    // 4. Lockfile leak: absolute host paths (from a `file://` source
    //    that got resolved). If present, the lockfile is platform-bound.
    let lock_path = manifest_path.with_file_name("aenv.lock");
    if let Ok(body) = std::fs::read_to_string(&lock_path) {
        for needle in ["/Users/", "/home/", "C:\\\\Users\\\\", "C:/Users/"] {
            if body.contains(needle) {
                err(
                    r,
                    format!(
                        "{} contains absolute host path '{needle}' — leaked from a \
                     file:// source. Re-lock from a portable source.",
                        lock_path.display()
                    ),
                );
                break;
            }
        }
    }

    // 5. Windows symlink support — only meaningful on Windows.
    #[cfg(target_os = "windows")]
    {
        let symlinks = std::process::Command::new("git")
            .args(["config", "--get", "core.symlinks"])
            .current_dir(cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if symlinks == "false" {
            warn(
                r,
                "git core.symlinks=false on Windows: symlinks in cloned repos \
                 will be checked out as plain text files containing the link \
                 target. Enable Developer Mode + `git config core.symlinks true`."
                    .into(),
            );
        }
    }

    // 6. UTF-8 BOM in aenv.toml. Some TOML parsers reject it; in any
    //    case the BOM bytes change the sha if computed pre-parse.
    if let Ok(bytes) = std::fs::read(manifest_path) {
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            warn(
                r,
                format!(
                    "{} starts with a UTF-8 BOM. Re-save as UTF-8 without BOM \
                 (some Windows editors add it silently).",
                    manifest_path.display()
                ),
            );
        }
    }

    // 7. CRLF inside committed shell scripts. Bash on Linux/macOS
    //    fails with `bad interpreter` when shebang lines have CRLF.
    let mut crlf_scripts = Vec::new();
    if let Ok(rd) = std::fs::read_dir(cwd) {
        for entry in rd.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !name.ends_with(".sh") && !name.ends_with(".bash") {
                continue;
            }
            if let Ok(b) = std::fs::read(&path) {
                if b.windows(2).any(|w| w == b"\r\n") {
                    crlf_scripts.push(name);
                }
            }
        }
    }
    if !crlf_scripts.is_empty() {
        err(
            r,
            format!(
                "shell scripts contain CRLF: {}. Will fail with `bad interpreter` on \
             Linux/macOS. Re-save as LF; pin via `.gitattributes`: \
             `*.sh text eol=lf`.",
                crlf_scripts.join(", ")
            ),
        );
    }
}

/// Verify `installed_plugins.json` schema and surface drift between
/// the manifest's plugins and what Claude Code's plugin discovery
/// (`cG()`, reads only this file) will actually see at launch.
///
/// Three drift signals:
///   1. **Schema version mismatch** — Claude Code expects v2;
///      anything else is a hard error (the file got rewritten by a
///      future Claude Code we haven't validated against).
///   2. **Manifest entry missing from JSON** — the user pinned a
///      plugin in `aenv.toml` but never ran `aenv install`, or the
///      plugin had a non-github source so native registration was
///      skipped. Either way it's invisible to claude.
///   3. **JSON entry not in manifest (and not aenv-tagged)** — a
///      plugin the user added inside claude with `/plugin install`.
///      Surface so the user knows it's not part of the reproducible
///      manifest (suggest `aenv ifl` to promote).
fn check_installed_plugins_drift(
    r: &mut Report,
    env: &Env,
    manifest: &crate::env::manifest::Manifest,
) {
    let push_ok = |r: &mut Report, m: String| {
        r.checks.push(Check {
            level: "ok",
            message: m,
        });
    };
    let push_warn = |r: &mut Report, m: String| {
        r.warnings += 1;
        r.checks.push(Check {
            level: "warn",
            message: m,
        });
    };
    let push_err = |r: &mut Report, m: String| {
        r.errors += 1;
        r.checks.push(Check {
            level: "err",
            message: m,
        });
    };

    let path = crate::backend::claude::installed_plugins::path_for(env);
    let doc = match crate::backend::claude::installed_plugins::read(&path) {
        Ok(d) => d,
        Err(e) => {
            push_err(
                r,
                format!(
                    "installed_plugins.json unreadable at {}: {e}. Claude Code's plugin \
                     discovery (cG()) will fail to load. Re-run `aenv install` to \
                     regenerate.",
                    path.display()
                ),
            );
            return;
        }
    };

    if doc.version != 2 {
        push_err(
            r,
            format!(
                "installed_plugins.json schema is v{}, expected v2. aenv was \
                 verified against Claude Code 2.1.138 schema v2; if Claude Code \
                 bumped the version, aenv needs an update before plugins will \
                 load.",
                doc.version
            ),
        );
        // Don't try to diff with an unknown schema.
        return;
    }

    // Manifest-side: which plugin keys (name@marketplace) does the
    // manifest declare? Bare-name entries map to the same name
    // they'd appear under in installed_plugins.json (we don't infer
    // marketplaces here — drift detection is pessimistic on
    // ambiguity).
    let manifest_keys: std::collections::BTreeSet<String> = manifest
        .plugin_specs()
        .map(|specs| {
            specs
                .into_iter()
                .map(|s| s.name)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();

    // JSON-side: split into aenv-tagged (we own) and ad-hoc (user
    // added via Claude Code's `/plugin install`).
    let mut aenv_keys: Vec<String> = Vec::new();
    let mut adhoc_keys: Vec<String> = Vec::new();
    for (key, entries) in &doc.plugins {
        let is_aenv = entries
            .first()
            .map(|e| e.extra.get("_aenv").and_then(|v| v.as_bool()) == Some(true))
            .unwrap_or(false);
        if is_aenv {
            aenv_keys.push(key.clone());
        } else {
            adhoc_keys.push(key.clone());
        }
    }

    // Drift 1: manifest entry missing from JSON. Match by bare name
    // — `code-review` in manifest matches `code-review@<any>` in
    // JSON. If neither bare nor any qualified form is registered,
    // it's a real miss.
    let mut missing_from_json: Vec<String> = Vec::new();
    for name in &manifest_keys {
        let registered = aenv_keys.iter().any(|k| {
            let bare = k.split('@').next().unwrap_or(k);
            bare == name
        });
        if !registered {
            missing_from_json.push(name.clone());
        }
    }
    if !missing_from_json.is_empty() {
        push_warn(
            r,
            format!(
                "manifest plugins NOT registered in installed_plugins.json: {}. \
                 Claude Code won't see them at launch. Causes (most common first): \
                 (a) the env is fresh-cloned and `aenv install` hasn't run yet, \
                 (b) the plugin's source isn't a recognized native shape \
                 (e.g. `npm:`, `file://`) so the live-apply step couldn't register \
                 it, (c) a previous apply failed mid-way and rolled back the JSON \
                 but not the manifest. Run `aenv install` to repair.",
                missing_from_json.join(", ")
            ),
        );
    }

    // Drift 2: ad-hoc plugins not in manifest. Friendly hint, not
    // an error — these are user choices not (yet) committed to git.
    if !adhoc_keys.is_empty() {
        push_warn(
            r,
            format!(
                "ad-hoc plugins in installed_plugins.json (added via /plugin \
                 install, not in aenv.toml): {}. Run `aenv ifl` to promote, or \
                 leave as env-local.",
                adhoc_keys.join(", ")
            ),
        );
    }

    if missing_from_json.is_empty() && adhoc_keys.is_empty() {
        push_ok(
            r,
            format!(
                "installed_plugins.json: schema v2, {} aenv-managed entries, no drift",
                aenv_keys.len()
            ),
        );
    }
}

fn detect_version(claude_bin: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new(claude_bin)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // Output like "claude-code/2.1.121 ..." or "2.1.121 (Claude Code)"
    let token = s.split_whitespace().find(|t| t.contains('.'))?;
    Some(
        token
            .trim_start_matches("claude-code/")
            .trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .next()
            .unwrap_or("")
            .to_string(),
    )
}
