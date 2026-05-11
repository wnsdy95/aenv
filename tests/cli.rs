#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use assert_cmd::prelude::*;
use predicates::prelude::*;

mod common;

use common::{make_local_plugin, make_local_skill, Env};

#[test]
fn init_creates_layout_and_default() {
    let e = Env::new();
    e.aenv().arg("init").assert().success();
    assert!(e.home_path().join("shims/claude").exists());
    assert!(e.home_path().join("envs/default").is_dir());
    assert!(e.home_path().join("envs/default/.claude").is_dir());
    // Phase C: every env carries a codex/ dir alongside .claude/ so
    // CODEX_HOME has a stable target whether or not the user has
    // installed codex yet.
    assert!(e.home_path().join("envs/default/codex").is_dir());
    assert!(e.home_path().join("envs/default/aenv.toml").is_file());
}

#[test]
fn install_writes_codex_config_toml_with_mcp_servers() {
    // End-to-end: a manifest with `[mcp.<name>]` materializes into
    // codex's `<env_root>/codex/config.toml` under `mcp_servers.<name>`.
    // Proves the per-backend render_runtime() loop in install picks up
    // codex even when codex itself isn't on PATH.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let manifest_path = e.env_dir("x").join("aenv.toml");
    std::fs::write(
        &manifest_path,
        r#"aenv_schema_version = "2"
[env]
name = "x"
[mcp.github]
command = "npx"
args = ["-y", "@scope/server-github"]
"#,
    )
    .unwrap();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let cfg = e.env_dir("x").join("codex").join("config.toml");
    assert!(
        cfg.is_file(),
        "codex config.toml missing at {}",
        cfg.display()
    );
    let body = std::fs::read_to_string(&cfg).unwrap();
    assert!(body.contains("[mcp_servers.github]"), "{body}");
    assert!(body.contains("command = \"npx\""), "{body}");
}

#[test]
fn init_no_default_skips_default() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    assert!(!e.home_path().join("envs/default").exists());
}

#[test]
fn new_creates_env_with_xdg_dirs() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    let root = e.env_dir("alpha");
    assert!(root.is_dir());
    assert!(root.join(".claude/settings.json").is_file());
    for kind in ["config", "data", "state", "cache"] {
        assert!(root.join("xdg").join(kind).is_dir(), "xdg/{kind} missing");
    }
    // /aenv:use and /aenv:reload slash commands are NOT under the per-env
    // .claude/commands/ — claude in overlay mode reads ~/.claude/commands/,
    // and `aenv init` installs them there. See slash_commands.rs.
}

#[test]
fn list_marks_active_env() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "a"]).assert().success();
    e.aenv().args(["new", "b"]).assert().success();
    e.aenv()
        .args(["list", "-l"])
        .assert()
        .success()
        .stdout(predicate::str::contains("a"))
        .stdout(predicate::str::contains("b"));
}

#[test]
fn add_plugin_writes_manifest() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "sample", "hello");
    e.aenv()
        .args([
            "add",
            "plugin",
            "sample",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    let manifest = std::fs::read_to_string(e.env_dir("x").join("aenv.toml")).unwrap();
    assert!(manifest.contains("name = \"sample\""));
}

#[cfg(unix)] // inode-based hardlink check; Windows uses different file index API
#[test]
fn install_materializes_plugin_via_hardlink() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "sample", "hello");
    e.aenv()
        .args([
            "add",
            "plugin",
            "sample",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    // Lockfile exists.
    let lock = std::fs::read_to_string(e.env_dir("x").join("aenv.lock")).unwrap();
    assert!(lock.contains("sha256"));

    // Plugin dir materialized.
    let plug_readme = e.env_dir("x").join(".claude/plugins/sample/README.md");
    assert!(plug_readme.is_file());
    assert_eq!(std::fs::read_to_string(&plug_readme).unwrap(), "hello");

    // Verify hardlink via inode equality with store.
    let sha_line = lock
        .lines()
        .find(|l| l.contains("sha256"))
        .expect("sha256 line");
    let sha = sha_line.split('"').nth(1).unwrap();
    let store_readme = e
        .home_path()
        .join("store/objects")
        .join(&sha[..2])
        .join(sha)
        .join("README.md");
    assert!(store_readme.is_file());
    let s_meta = std::fs::metadata(&store_readme).unwrap();
    let p_meta = std::fs::metadata(&plug_readme).unwrap();
    assert_eq!(
        s_meta.ino(),
        p_meta.ino(),
        "plugin file should be hardlinked to store"
    );
}

#[test]
fn install_writes_installed_plugins_json_with_correct_schema_v2() {
    // The decisive contract: install ALWAYS produces a schema-v2
    // installed_plugins.json, and EVERY manifest plugin lands in
    // it — local-source plugins included, bucketed into the
    // synthetic `aenv-local` marketplace. Without this, Claude
    // Code's `cG()` discovery wouldn't see locally-sourced plugins
    // (only github-shaped ones make it past `infer_marketplace`),
    // and `aenv ifl` of a `/plugin install <path>` plugin would
    // import-but-not-show.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let local = make_local_plugin(e.home_path(), "sample", "hello");
    e.aenv()
        .args([
            "add",
            "plugin",
            "sample",
            "-E",
            "x",
            "--source",
            local.to_str().unwrap(),
        ])
        .assert()
        .success();

    // The fanout directory is on disk.
    let plug = e.env_dir("x").join(".claude/plugins/sample");
    assert!(plug.is_dir(), "fanout dir must exist");

    let ip_path = e
        .env_dir("x")
        .join(".claude/plugins/installed_plugins.json");
    let body = std::fs::read_to_string(&ip_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["version"], 2, "schema_version must be 2: {body}");
    let key = "sample@aenv-local";
    let entry = &parsed["plugins"][key];
    assert!(
        entry.is_array(),
        "local-source plugin must register under aenv-local: {body}"
    );
    let first = &entry[0];
    assert_eq!(first["scope"], "user");
    assert_eq!(first["_aenv"], true);

    // Synthetic marketplace.json was written so Claude Code's
    // resolver can validate the entry.
    let mkt_json = e
        .env_dir("x")
        .join(".claude/plugins/marketplaces/aenv-local/.claude-plugin/marketplace.json");
    assert!(
        mkt_json.is_file(),
        "synthetic aenv-local marketplace.json missing"
    );
    let mkt_body = std::fs::read_to_string(&mkt_json).unwrap();
    assert!(
        mkt_body.contains("\"sample\""),
        "marketplace.json must list the plugin by bare name: {mkt_body}"
    );

    // enabledPlugins flips on so claude actually loads it.
    let settings_body =
        std::fs::read_to_string(e.env_dir("x").join(".claude/settings.json")).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&settings_body).unwrap();
    assert_eq!(
        settings["enabledPlugins"][key], true,
        "enabledPlugins must include the local plugin: {settings_body}"
    );
}

#[test]
fn skill_install_registers_wrapper_plugin_in_native_json() {
    // Skill wrappers go into installed_plugins.json under a
    // synthetic `aenv-skills` marketplace, so Claude Code's `cG()`
    // discovery actually loads them. Without this, skills were
    // wrapped into plugin dirs but never visible to claude.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let local = make_local_plugin(e.home_path(), "lint-check", "skill content");
    e.aenv()
        .args([
            "add",
            "skill",
            "lint-check",
            "-E",
            "x",
            "--source",
            local.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    // Wrapper plugin dir on disk.
    let wrapper = e.env_dir("x").join(".claude/plugins/skill-lint-check");
    assert!(wrapper.is_dir(), "skill wrapper plugin dir must exist");

    // Registered in installed_plugins.json under aenv-skills marketplace.
    let ip: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            e.env_dir("x")
                .join(".claude/plugins/installed_plugins.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let key = "skill-lint-check@aenv-skills";
    let entry_arr = ip["plugins"][key]
        .as_array()
        .unwrap_or_else(|| panic!("missing key '{key}' in installed_plugins.json: {ip}"));
    let entry = &entry_arr[0];
    assert_eq!(entry["scope"], "user");
    assert!(entry["installPath"]
        .as_str()
        .unwrap_or("")
        .ends_with("plugins/skill-lint-check"));
    assert_eq!(entry["_aenv"], true);

    // Marketplace registered.
    let mkts: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            e.env_dir("x")
                .join(".claude/plugins/known_marketplaces.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        mkts.get("aenv-skills").is_some(),
        "aenv-skills synthetic marketplace must be registered: {mkts}"
    );

    // Enabled in settings.json.
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(e.env_dir("x").join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["enabledPlugins"][key], true);
}

#[test]
fn rm_then_install_prunes_native_json_and_cache_dir() {
    // Clean-state contract for the manifest-as-single-source-of-truth
    // model: rm a plugin from the manifest, then `aenv install`,
    // and (1) installed_plugins.json drops our entry, (2) the
    // settings.json::enabledPlugins entry is cleared, (3) the
    // legacy fanout dir for that plugin is removed. User-added
    // entries (no `_aenv` flag) survive — that branch is covered
    // by the unit tests; here we validate the install-time path
    // end-to-end.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let local = make_local_plugin(e.home_path(), "code-review", "hello");

    // First install with a plugin entry. Use a local source so the
    // fetcher stays offline. The native-JSON registration won't
    // trigger because local sources don't yield a marketplace, so
    // we simulate the registration manually after install.
    e.aenv()
        .args([
            "add",
            "plugin",
            "code-review",
            "-E",
            "x",
            "--source",
            local.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    // Manually plant an aenv-managed entry in the native JSON
    // (mirroring what a github-source install would do). Use the
    // same `_aenv: true` flag so prune recognizes it.
    let ip_path = e
        .env_dir("x")
        .join(".claude/plugins/installed_plugins.json");
    let body = std::fs::read_to_string(&ip_path).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&body).unwrap();
    json["plugins"]["code-review@claude-plugins-official"] = serde_json::json!([{
        "scope": "user",
        "installPath": e.env_dir("x").join(".claude/plugins/code-review").to_str().unwrap(),
        "_aenv": true
    }]);
    std::fs::write(&ip_path, serde_json::to_string(&json).unwrap()).unwrap();
    let stale_dir = e.env_dir("x").join(".claude/plugins/code-review");
    assert!(stale_dir.is_dir(), "fanout dir should exist post-install");

    // Now `aenv rm plugin code-review` and re-install. With no
    // matching manifest entry the kept_keys set is empty and the
    // `_aenv`-flagged entry should disappear, taking the fanout
    // dir with it (clean-state policy).
    e.aenv()
        .args(["rm", "plugin", "code-review", "-E", "x"])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&ip_path).unwrap()).unwrap();
    assert!(
        after["plugins"]
            .get("code-review@claude-plugins-official")
            .is_none(),
        "aenv-tagged entry must be pruned: {after}"
    );
    assert!(
        !stale_dir.is_dir(),
        "stale fanout dir must be removed (clean-state policy): {}",
        stale_dir.display()
    );
}

#[test]
fn rm_plugin_removes_from_manifest() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "sample", "hi");
    e.aenv()
        .args([
            "add",
            "plugin",
            "sample",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv()
        .args(["rm", "plugin", "sample", "-E", "x"])
        .assert()
        .success();
    let manifest = std::fs::read_to_string(e.env_dir("x").join("aenv.toml")).unwrap();
    assert!(!manifest.contains("name = \"sample\""));
}

#[test]
fn schema_999_is_rejected_by_status() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "bad"]).assert().success();
    std::fs::write(
        e.env_dir("bad").join("aenv.toml"),
        "aenv_schema_version = \"999\"\n[env]\nname = \"bad\"\n",
    )
    .unwrap();
    e.aenv()
        .args(["status", "-E", "bad"])
        .assert()
        .stderr(predicate::str::contains("999"));
}

#[test]
fn schema_v1_manifest_rejected_clean_break() {
    // 0.3.0 dropped v1 → v2 migration. A v1 manifest written by an
    // older aenv (or hand-written from old docs) must fail with a
    // pointer to the v2 shape, not silently load.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "old"]).assert().success();
    std::fs::write(
        e.env_dir("old").join("aenv.toml"),
        "aenv_schema_version = \"1\"\n[env]\nname = \"old\"\n",
    )
    .unwrap();
    e.aenv()
        .args(["status", "-E", "old"])
        .assert()
        .stderr(predicate::str::contains("schema").and(predicate::str::contains("'1'")));
}

#[test]
fn export_then_import_roundtrip() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "src"]).assert().success();
    let p = make_local_plugin(e.home_path(), "rp", "round");
    e.aenv()
        .args([
            "add",
            "plugin",
            "rp",
            "-E",
            "src",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "src"]).assert().success();

    let bundle = e.home_path().join("src.aenv.tar.gz");
    e.aenv()
        .args([
            "export-profile",
            "-E",
            "src",
            "-o",
            bundle.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(bundle.is_file());

    e.aenv()
        .args(["import-profile", bundle.to_str().unwrap(), "--name", "dst"])
        .assert()
        .success();

    // Lockfile carried over → install should work without fetching (store has it).
    e.aenv().args(["install", "-E", "dst"]).assert().success();
    assert!(e
        .env_dir("dst")
        .join(".claude/plugins/rp/README.md")
        .is_file());
}

#[test]
fn rollback_restores_env_after_a_committed_add_txn() {
    // Auto-apply rolled the materialize step into `aenv add`
    // itself, so the meaningful tx now wraps `add` rather than a
    // separate `install`. Rollback after add must therefore
    // restore the env to its pre-add state (empty plugins dir),
    // not just the manifest.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "sample", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "sample",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(e
        .env_dir("x")
        .join(".claude/plugins/sample/README.md")
        .is_file());

    // Roll back the add — plugins dir restored to pre-add state (empty).
    e.aenv().arg("rollback").assert().success();
    assert!(!e
        .env_dir("x")
        .join(".claude/plugins/sample/README.md")
        .is_file());
}

#[test]
fn doctor_json_has_required_fields() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let out = e.aenv().args(["doctor", "x", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor --json output is JSON");
    assert!(v.get("checks").is_some());
    assert!(v.get("warnings").is_some());
    assert!(v.get("errors").is_some());
    assert!(v.get("env").is_some());
}

#[test]
fn shell_init_emits_path_prepend_for_zsh() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv()
        .args(["shell-init", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("aenv shell init (zsh)"))
        .stdout(predicate::str::contains("export PATH="));
}

#[test]
fn current_explain_shows_source() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "z"]).assert().success();
    e.aenv()
        .args(["current", "--explain"])
        .env("AENV_OVERRIDE", "z")
        .assert()
        .success()
        .stdout(predicate::str::contains("Override"))
        .stdout(predicate::str::contains("z"));
}

#[test]
fn add_unknown_kind_errors() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    e.aenv()
        .args(["add", "weirdkind", "foo", "-E", "x"])
        .assert()
        .failure();
}

#[test]
fn install_preserves_adhoc_mcps_via_merge_semantics() {
    // The pivot: aenv no longer clobbers ad-hoc /mcp add entries, no
    // drift prompt, no --force. Manifest entries get tagged with
    // `_aenv: true` and refresh on every run; un-tagged entries
    // (= ad-hoc, what the user added inside claude) are preserved
    // byte-identically. Pinned by this test against the prior
    // wholesale-overwrite behavior.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let settings_path = e.env_dir("x").join(".claude").join("settings.json");
    std::fs::write(
        &settings_path,
        r#"{"mcpServers":{"adhoc":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();

    // Pin a *different* MCP through the manifest, then install.
    e.aenv()
        .args([
            "add", "mcp", "github", "-E", "x", "--", "true", "--port", "8080",
        ])
        .assert()
        .success();

    let after = std::fs::read_to_string(&settings_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
    let servers = parsed["mcpServers"].as_object().unwrap();
    // Ad-hoc entry survived, untagged.
    let adhoc = &servers["adhoc"];
    assert_eq!(adhoc["command"], "echo");
    assert!(
        adhoc.get("_aenv").is_none(),
        "ad-hoc entry must not be tagged: {adhoc}"
    );
    // Manifest entry rendered, tagged.
    let github = &servers["github"];
    assert_eq!(github["command"], "true");
    assert_eq!(
        github["_aenv"], true,
        "manifest entry must be tagged so prune knows it owns it"
    );
}

#[test]
fn rm_drops_aenv_managed_mcp_but_preserves_adhoc() {
    // Companion to the merge test: `aenv rm mcp` must drop aenv's
    // own row from settings.json::mcpServers without touching ad-hoc
    // siblings. This exercises the `_aenv: true` flag's role on
    // the prune side.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let settings_path = e.env_dir("x").join(".claude").join("settings.json");
    std::fs::write(
        &settings_path,
        r#"{"mcpServers":{"adhoc":{"command":"echo"}}}"#,
    )
    .unwrap();
    e.aenv()
        .args(["add", "mcp", "github", "-E", "x", "--", "true"])
        .assert()
        .success();
    e.aenv()
        .args(["rm", "mcp", "github", "-E", "x"])
        .assert()
        .success();
    let after = std::fs::read_to_string(&settings_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
    let servers = parsed["mcpServers"].as_object().unwrap();
    assert!(
        !servers.contains_key("github"),
        "aenv-tagged mcp must be dropped after rm: {after}"
    );
    assert!(
        servers.contains_key("adhoc"),
        "ad-hoc mcp must survive rm of an unrelated aenv entry: {after}"
    );
}

#[test]
fn install_writes_mcp_servers_with_rewritten_placeholders() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    e.aenv()
        .args([
            "add",
            "mcp",
            "github",
            "-E",
            "x",
            "--command",
            "npx",
            "--arg=-y",
            "--arg=@example/mcp-github",
            "--env-var",
            "GITHUB_TOKEN=${secret:gh_token}",
            "--env-var",
            "DEBUG=${env:DEBUG}",
            "--env-var",
            "LITERAL=verbatim",
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    let settings_body =
        std::fs::read_to_string(e.env_dir("x").join(".claude/settings.json")).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&settings_body).unwrap();
    let github = &settings["mcpServers"]["github"];
    assert_eq!(github["command"], "npx");
    assert_eq!(github["args"][0], "-y");
    let env = &github["env"];
    // ${secret:gh_token} -> ${AENV_X_GH_TOKEN}
    assert_eq!(env["GITHUB_TOKEN"], "${AENV_X_GH_TOKEN}");
    // ${env:DEBUG} -> ${DEBUG}
    assert_eq!(env["DEBUG"], "${DEBUG}");
    // literal preserved
    assert_eq!(env["LITERAL"], "verbatim");
}

#[test]
fn install_with_empty_manifest_keeps_adhoc_mcps_untouched() {
    // Replaces the pre-pivot test that asserted --force prunes
    // mcpServers wholesale. Under merge semantics, install with an
    // empty manifest is essentially a no-op for ad-hoc MCPs — they
    // stay because aenv only owns rows it explicitly tagged. Pinned
    // here so a future regression that re-introduces wholesale
    // prune gets caught.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    std::fs::write(
        e.env_dir("x").join(".claude/settings.json"),
        r#"{"mcpServers":{"adhoc":{"command":"x"}},"theme":"dark"}"#,
    )
    .unwrap();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let body = std::fs::read_to_string(e.env_dir("x").join(".claude/settings.json")).unwrap();
    let s: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        s["mcpServers"]["adhoc"].is_object(),
        "ad-hoc MCP must survive empty-manifest install: {body}"
    );
    assert_eq!(s["theme"], "dark", "unrelated keys preserved");
}

#[test]
fn shim_warns_on_missing_env_pin_instead_of_locking_out() {
    // .aenv-version pointing at a non-existent env should NOT fail the shim;
    // it should warn and run the real claude unisolated.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("proj");
    std::fs::create_dir(&proj).unwrap();
    std::fs::write(proj.join(".aenv-version"), "ghost\n").unwrap();
    // Invoke the shim by argv[0]: copy it into a temp dir so argv[0] stem == "claude".
    let shim = e.home_path().join("shims/claude");
    let mut cmd = std::process::Command::new(&shim);
    cmd.arg("--version")
        .current_dir(&proj)
        .env("AENV_HOME", e.home_path());
    // Strip parent-shell aenv breadcrumbs so the cwd-pinned `.aenv-version`
    // is what the resolver actually walks to. Without this, running
    // `cargo test` from a supervised aenv session leaks `AENV=<dev's env>`
    // and the resolver picks that up over the .aenv-version pin.
    for v in Env::scrub_vars_static() {
        cmd.env_remove(v);
    }
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found") && stderr.contains("ghost"),
        "expected fallback warning, got: {stderr}"
    );
}

#[test]
fn claude_shim_routes_config_dir_into_active_env() {
    // The new shim model: a single `CLAUDE_CONFIG_DIR=<env>/.claude`
    // env-var hands claude the per-env config root. Plugins, sessions,
    // installed_plugins.json, the keychain hash all follow it (verified
    // against Claude Code 2.1.138's bundled JS). This test pins that
    // contract by capturing the env vars the shim hands to claude.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.install_recording_claude();
    e.aenv()
        .args(["run", "--env", "alpha", "--", "noop"])
        .assert()
        .success();
    let cap = e.read_capture();
    let want_config = e.env_dir("alpha").join(".claude");
    assert!(
        cap.contains(&format!("env:CLAUDE_CONFIG_DIR={}", want_config.display())),
        "expected CLAUDE_CONFIG_DIR={} in capture, got:\n{cap}",
        want_config.display()
    );
    assert!(
        cap.contains("env:AENV=alpha"),
        "expected AENV=alpha breadcrumb, got:\n{cap}"
    );
}

#[test]
fn claude_shim_does_not_leak_inherited_claude_config_dir() {
    // If the parent shell has a stale CLAUDE_CONFIG_DIR set (from a
    // prior run, a different tool, or a test harness), the shim must
    // overwrite it with the active env's path. Same expectation the
    // legacy supervisor honored — the new shim path keeps it.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.install_recording_claude();
    let mut run = e.aenv();
    run.env("CLAUDE_CONFIG_DIR", "/should/be/overwritten")
        .args(["run", "--env", "alpha", "--", "noop"])
        .assert()
        .success();
    let cap = e.read_capture();
    let want_config = e.env_dir("alpha").join(".claude");
    assert!(
        cap.contains(&format!("env:CLAUDE_CONFIG_DIR={}", want_config.display())),
        "shim did not override inherited CLAUDE_CONFIG_DIR; capture:\n{cap}"
    );
    assert!(
        !cap.contains("CLAUDE_CONFIG_DIR=/should/be/overwritten"),
        "stale CLAUDE_CONFIG_DIR leaked through; capture:\n{cap}"
    );
}

#[test]
fn shim_runs_pre_activate_hook_with_aenv_breadcrumb_env_vars() {
    // The supervisor used to fire `[hooks].pre_activate` before each
    // launch with AENV_NAME and AENV_ROOT in the hook's env. The new
    // single-exec shim keeps that contract — committed user hooks
    // (project-mode aenv.toml in a teammate's repo, exported profile
    // bundles approved with --trust-hooks) run unchanged across the
    // pivot. This test pins both: the hook fires, and it sees the
    // expected breadcrumbs.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.install_recording_claude();
    // Plant a hook that writes AENV_NAME + AENV_ROOT into a marker
    // file. Marker lives outside the env tree to avoid the env-dir
    // 0700 lockdown stepping on the test fixture write.
    let marker = e.home_path().join("hook-fired");
    let manifest_path = e.env_dir("alpha").join("aenv.toml");
    let manifest_body = std::fs::read_to_string(&manifest_path).unwrap();
    // `Manifest::default_for` already emits an empty `[hooks]` section,
    // so we splice the pre_activate line under that header rather than
    // appending a second `[hooks]` (TOML rejects duplicate tables).
    let hook_cmd = format!(
        "echo $AENV_NAME > {marker_path}; echo $AENV_ROOT >> {marker_path}",
        marker_path = marker.display()
    );
    let inject = format!("[hooks]\npre_activate = '{hook_cmd}'");
    assert!(
        manifest_body.contains("[hooks]"),
        "manifest missing [hooks] section: {manifest_body}"
    );
    let with_hook = manifest_body.replace("[hooks]", &inject);
    std::fs::write(&manifest_path, with_hook).unwrap();

    let out = e
        .aenv()
        .args(["run", "--env", "alpha", "--", "noop"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "aenv run failed: stderr=\n{stderr}");

    let body = std::fs::read_to_string(&marker).unwrap_or_else(|e| {
        panic!("pre_activate hook did not run — marker missing: {e}\nstderr:\n{stderr}")
    });
    assert!(
        body.lines().next() == Some("alpha"),
        "AENV_NAME wrong; body:\n{body}"
    );
    let want_root = e.env_dir("alpha");
    let want_root_str = want_root.display().to_string();
    assert!(
        body.lines().nth(1) == Some(want_root_str.as_str()),
        "AENV_ROOT wrong; expected {want_root_str} body:\n{body}"
    );
}

#[test]
fn shim_aborts_launch_when_pre_activate_hook_exits_nonzero() {
    // The hook contract is exit-code authoritative: a manifest that
    // ships a `pre_activate` is by definition trusted (committed by
    // the user, reviewed in a PR, or imported with --trust-hooks), so
    // its exit code drives the launch decision. Non-zero → no claude.
    // Lets users park real preflight checks (network reachability,
    // required secrets, branch sanity) in the hook and refuse the env
    // by failing.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.install_recording_claude();
    let manifest_path = e.env_dir("alpha").join("aenv.toml");
    let body = std::fs::read_to_string(&manifest_path).unwrap();
    let with_hook = body.replace("[hooks]", "[hooks]\npre_activate = 'echo nope >&2; exit 7'");
    std::fs::write(&manifest_path, with_hook).unwrap();

    let out = e
        .aenv()
        .args(["run", "--env", "alpha", "--", "noop"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "shim should have aborted launch on hook failure; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pre_activate") && stderr.contains("exit status 7"),
        "expected pre_activate failure message with exit 7, got: {stderr}"
    );
    // The fake claude must not have been spawned — its capture file
    // should not exist (recording fake only writes when spawned).
    assert!(
        !e.argv_capture_path().is_file(),
        "fake claude was spawned despite hook refusal; capture present at {}",
        e.argv_capture_path().display()
    );
}

#[test]
fn global_env_always_listed_first() {
    // `global` is the reserved alias for ~/.claude / ~/.codex; it
    // must appear in `aenv list` regardless of what's on disk, so
    // users always have a known-good escape hatch.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let out = e.aenv().args(["list"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().next().is_some_and(|l| l.contains("global")),
        "global must be the first listed env; got:\n{stdout}"
    );
}

#[test]
fn export_profile_refuses_global_to_prevent_archiving_home() {
    // `global.root` is $HOME — walking it would tar up the user's
    // entire home directory (SSH keys, dotfiles, secrets). Hard
    // refuse before any disk walk happens.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let out = e
        .aenv()
        .args(["export-profile", "-E", "global"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "export-profile must refuse global; stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("global") && stderr.contains("aliases"),
        "expected refusal message naming global; got: {stderr}"
    );
    // No archive should have been written into cwd.
    let leaked = e.home_path().join("global.aenv.tar.gz");
    assert!(
        !leaked.exists(),
        "global archive leaked at {}",
        leaked.display()
    );
}

#[test]
fn cannot_create_or_remove_global_env() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv()
        .args(["new", "global"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("reserved"));
    e.aenv()
        .args(["remove", "global"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("reserved"));
}

#[test]
fn shim_under_global_does_not_set_claude_config_dir() {
    // The whole point of `global` is "use the user's real ~/.claude".
    // The shim must therefore unset CLAUDE_CONFIG_DIR (so any stale
    // inherited value is cleared too) — claude resolves its own
    // ~/.claude when the var is absent.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.install_recording_claude();
    let out = e
        .aenv()
        .env("CLAUDE_CONFIG_DIR", "/should/be/cleared")
        .args(["run", "--env", "global", "--", "noop"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "global run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cap = e.read_capture();
    assert!(
        cap.contains("env:CLAUDE_CONFIG_DIR=<unset>"),
        "global must leave CLAUDE_CONFIG_DIR unset; capture:\n{cap}"
    );
    assert!(
        cap.contains("env:AENV=global"),
        "AENV breadcrumb must say global; capture:\n{cap}"
    );
}

#[test]
fn unresolved_cwd_falls_back_to_global() {
    // No .aenv-version, no aenv.toml, no default_env in config.toml
    // → resolver must still return Some(global), not None. That's
    // what makes `aenv quit` a real escape: the next claude launch
    // always has a viable env to resolve.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let clean = e.home_path().join("clean");
    std::fs::create_dir_all(&clean).unwrap();
    let out = e
        .aenv()
        .args(["current"])
        .current_dir(&clean)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim() == "global",
        "expected current=global, got: {stdout:?}"
    );
}

#[test]
fn add_rejects_path_traversal_in_plugin_name() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    // Various malicious shapes — all must be rejected before manifest mutation.
    for bad in ["../escape", "a/b", ".hidden", "name with spaces", ""] {
        e.aenv()
            .args(["add", "plugin", bad, "-E", "x", "--source", "/tmp/x"])
            .assert()
            .failure();
    }
    // Manifest should still parse cleanly (no half-written entry).
    e.aenv().args(["status", "-E", "x"]).assert().success();
}

#[test]
fn add_rejects_path_traversal_in_mcp_name() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    for bad in ["../mcp", "a/b", ".hidden"] {
        e.aenv()
            .args(["add", "mcp", bad, "-E", "x", "--command", "true"])
            .assert()
            .failure();
    }
}

#[cfg(unix)] // mode bits — Windows lock_down_* is intentionally a no-op
#[test]
fn home_and_env_dirs_are_owner_only_after_init() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let home_mode = std::fs::metadata(e.home_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let env_mode = std::fs::metadata(e.env_dir("x"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    // Best-effort: if FS supports mode bits at all, "group" + "other" must be empty.
    assert_eq!(home_mode & 0o077, 0, "aenv_home leaks bits: {home_mode:o}");
    assert_eq!(env_mode & 0o077, 0, "env dir leaks bits: {env_mode:o}");
}

#[test]
fn manifest_with_traversal_in_name_field_rejected_at_load() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    // Hand-craft a malicious manifest. Validate must reject on load.
    std::fs::write(
        e.env_dir("x").join("aenv.toml"),
        r#"aenv_schema_version = "2"
[env]
name = "x"
[mcp."../escape"]
command = "true"
"#,
    )
    .unwrap();
    e.aenv()
        .args(["status", "-E", "x"])
        .assert()
        .stderr(predicate::str::contains("escape"));
}

#[test]
fn rm_then_install_prunes_lockfile_entry() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p1", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p1",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let lock_before = std::fs::read_to_string(e.env_dir("x").join("aenv.lock")).unwrap();
    assert!(lock_before.contains("name = \"p1\""));

    e.aenv()
        .args(["rm", "plugin", "p1", "-E", "x"])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let lock_after = std::fs::read_to_string(e.env_dir("x").join("aenv.lock")).unwrap();
    assert!(
        !lock_after.contains("name = \"p1\""),
        "removed plugin should not remain in lockfile: {lock_after}"
    );
}

#[test]
fn add_refuses_to_clobber_malformed_settings_json() {
    // Pivot: every aenv mutator now applies immediately, so a
    // malformed settings.json must abort the FIRST mutator that
    // would touch it (here, `aenv add mcp`) — not silently update
    // the manifest and lose the user's custom JSON later. The
    // file's malformed content stays put after the failure so the
    // user can fix it manually.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let settings = e.env_dir("x").join(".claude/settings.json");
    std::fs::write(&settings, "{\"theme\": broken-json").unwrap();
    let out = e
        .aenv()
        .args(["add", "mcp", "github", "-E", "x", "--", "true"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "add must abort on malformed settings.json: stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not valid JSON") || stderr.contains("settings.json"),
        "expected JSON error, got: {stderr}"
    );
    // settings.json must be left as-is, not silently overwritten.
    let body = std::fs::read_to_string(&settings).unwrap();
    assert!(
        body.contains("broken-json"),
        "settings.json was overwritten"
    );
}

#[test]
fn install_rejects_pre_v3_lockfile_clean_break() {
    // 0.3.0 dropped v1 → v2 → v3 lockfile migration entirely. A pre-v3
    // lockfile (e.g. one written by an older aenv) must fail with a
    // clear "delete the file" pointer rather than silently load.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p1", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p1",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    // Downgrade the lockfile to v2 to simulate the clean-break scenario.
    let lock_path = e.env_dir("x").join("aenv.lock");
    let body = std::fs::read_to_string(&lock_path).unwrap();
    let downgraded = body.replace(r#"schema_version = "3""#, r#"schema_version = "2""#);
    std::fs::write(&lock_path, &downgraded).unwrap();

    let out = e.aenv().args(["install", "-E", "x"]).output().unwrap();
    assert!(!out.status.success(), "expected failure on pre-v3 lockfile");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("schema_version") && stderr.contains("'3'"),
        "missing clean-break pointer in stderr: {stderr}"
    );
}

#[test]
fn add_plugin_rejects_dir_without_claude_plugin_manifest() {
    // Local plugin source missing `.claude-plugin/plugin.json` must
    // fail loudly the moment aenv tries to materialize — which is
    // now the `aenv add` step itself, not a deferred `install`.
    // Catches the CI-diagnosed case where an artifact stripped the
    // dot-prefixed dir and a deferred install would have reported
    // success against an incomplete tree (test name pre-pivot).
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let bad = e.home_path().join("bad-plugin");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("README.md"), "no manifest here").unwrap();
    let out = e
        .aenv()
        .args([
            "add",
            "plugin",
            "bad",
            "-E",
            "x",
            "--source",
            bad.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "add must fail without plugin.json");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(".claude-plugin/plugin.json")
            || stderr.contains("missing required manifest"),
        "expected manifest error, got: {stderr}"
    );
}

#[test]
fn add_plugin_rejects_dir_with_malformed_manifest_json() {
    // Plugin manifest exists but is broken JSON — `aenv add` (which
    // immediately materializes) must surface the parse error, not
    // silently update the manifest and defer the failure to a
    // never-run install.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let bad = e.home_path().join("bad-json-plugin");
    let cp = bad.join(".claude-plugin");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(cp.join("plugin.json"), "{not valid json").unwrap();
    let out = e
        .aenv()
        .args([
            "add",
            "plugin",
            "bad",
            "-E",
            "x",
            "--source",
            bad.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "add must fail on malformed json");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parse plugin manifest") || stderr.contains("plugin.json"),
        "expected parse error, got: {stderr}"
    );
}

#[test]
fn install_accepts_marketplace_repo_plugin_subdir() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();

    let repo = e.home_path().join("marketplace");
    std::fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
    std::fs::write(
        repo.join(".claude-plugin").join("marketplace.json"),
        r#"{"plugins":[{"name":"code-review","source":"./plugins/code-review"}]}"#,
    )
    .unwrap();
    let plugin = repo.join("plugins").join("code-review");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::write(
        plugin.join(".claude-plugin").join("plugin.json"),
        r#"{"name":"code-review","version":"0.1.0"}"#,
    )
    .unwrap();
    std::fs::write(plugin.join("README.md"), "review things").unwrap();

    e.aenv()
        .args([
            "add",
            "plugin",
            "code-review",
            "-E",
            "x",
            "--source",
            repo.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    assert!(e
        .env_dir("x")
        .join(".claude")
        .join("plugins")
        .join("code-review")
        .join(".claude-plugin")
        .join("plugin.json")
        .is_file());
    let lock = std::fs::read_to_string(e.env_dir("x").join("aenv.lock")).unwrap();
    assert!(
        lock.contains("subpath = \"plugins/code-review\""),
        "lockfile should record inferred marketplace subpath: {lock}"
    );
}

#[test]
fn add_plugin_rejects_unsafe_subpath() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    e.aenv()
        .args([
            "add",
            "plugin",
            "code-review",
            "-E",
            "x",
            "--source",
            "/tmp/source",
            "--subpath",
            "../code-review",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("subpath"));
}

#[test]
fn install_writes_atomically_no_partial_truncate() {
    // After a successful install, no `.tmp.<pid>.<nano>` files should remain
    // (they'd indicate a write that didn't finalize via rename).
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p1", "x");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p1",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    for entry in walkdir::WalkDir::new(e.env_dir("x")) {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with('.') || !name.contains(".tmp."),
            "stale atomic-write tmp file: {}",
            entry.path().display()
        );
    }
}

// Note on tar safety: we intentionally don't construct a `..`-path tarball as
// an integration test, because `tar::Header::set_path` rejects unsafe paths at
// write time — the same library cannot easily produce a malicious archive. The
// defense (skipping non-Regular entries, rejecting `..`/absolute paths,
// `target.starts_with(dst)` final check) is reviewable in `unpack_safe` and
// covered by tar's own test suite.

#[test]
fn rollback_pending_recovers_killed_install() {
    // Simulate a killed install by hand-writing a Pending tx manifest with a
    // captured snapshot. `aenv rollback --pending` should restore from it.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();

    // Capture state of plugins dir before "install".
    let plugins = e.env_dir("x").join(".claude/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let txn_dir = e.home_path().join("state/20260101T000000Z-1");
    std::fs::create_dir_all(&txn_dir).unwrap();
    let snap = txn_dir.join("snap-plugins");
    std::fs::create_dir_all(&snap).unwrap();
    // Snapshot: empty (matches before-state). Now corrupt the env to simulate
    // a partial mutation a killed process left behind.
    std::fs::write(plugins.join("zombie"), "left over").unwrap();
    let manifest = serde_json::json!({
        "id": "20260101T000000Z-1",
        "started": "2026-01-01T00:00:00Z",
        "finished": null,
        "kind": "install",
        "env": "x",
        "backed_up": [{
            "original": plugins.to_string_lossy(),
            "snapshot": snap.to_string_lossy(),
            "kind": "Dir"
        }],
        "status": "Pending",
        "note": null,
    });
    std::fs::write(
        txn_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // Without --pending: refuses.
    e.aenv()
        .arg("rollback")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--pending"));

    // With --pending: restores empty snapshot, removing the zombie file.
    e.aenv().args(["rollback", "--pending"]).assert().success();
    assert!(!plugins.join("zombie").exists(), "zombie not cleaned");
}

#[test]
fn doctor_warns_about_pending_transactions() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let txn_dir = e.home_path().join("state/20260101T000000Z-2");
    std::fs::create_dir_all(&txn_dir).unwrap();
    std::fs::write(
        txn_dir.join("manifest.json"),
        r#"{"id":"20260101T000000Z-2","started":"2026-01-01T00:00:00Z","finished":null,"kind":"install","env":"x","backed_up":[],"status":"Pending"}"#,
    )
    .unwrap();
    let out = e.aenv().args(["doctor", "x"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("pending") || stdout.contains("Pending"),
        "expected pending warning, got: {stdout}"
    );
}

#[test]
fn import_profile_strips_hooks_without_trust_flag() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "src"]).assert().success();
    // Hand-set a hooks.pre_activate in the source manifest.
    let manifest_path = e.env_dir("src").join("aenv.toml");
    let body = std::fs::read_to_string(&manifest_path).unwrap();
    let body = body.replace(
        "[hooks]\n",
        "[hooks]\npre_activate = \"echo PWNED > /tmp/aenv-hook-pwn\"\n",
    );
    std::fs::write(&manifest_path, body).unwrap();

    let bundle = e.home_path().join("src.aenv.tar.gz");
    e.aenv()
        .args([
            "export-profile",
            "-E",
            "src",
            "-o",
            bundle.to_str().unwrap(),
        ])
        .assert()
        .success();

    // Default import: hook stripped, with stderr warning.
    let out = e
        .aenv()
        .args(["import-profile", bundle.to_str().unwrap(), "--name", "dst"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("hooks.pre_activate"), "{stderr}");
    let dst_manifest = std::fs::read_to_string(e.env_dir("dst").join("aenv.toml")).unwrap();
    assert!(
        !dst_manifest.contains("PWNED"),
        "hook should be stripped: {dst_manifest}"
    );
}

#[test]
fn import_profile_keeps_hooks_with_trust_flag() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "src"]).assert().success();
    let manifest_path = e.env_dir("src").join("aenv.toml");
    let body = std::fs::read_to_string(&manifest_path).unwrap();
    let body = body.replace("[hooks]\n", "[hooks]\npre_activate = \"echo trusted\"\n");
    std::fs::write(&manifest_path, body).unwrap();

    let bundle = e.home_path().join("src.aenv.tar.gz");
    e.aenv()
        .args([
            "export-profile",
            "-E",
            "src",
            "-o",
            bundle.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "import-profile",
            bundle.to_str().unwrap(),
            "--name",
            "dst2",
            "--trust-hooks",
        ])
        .assert()
        .success();
    let dst_manifest = std::fs::read_to_string(e.env_dir("dst2").join("aenv.toml")).unwrap();
    assert!(
        dst_manifest.contains("echo trusted"),
        "hook should be kept with --trust-hooks: {dst_manifest}"
    );
}

#[test]
fn list_marks_broken_env() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "good"]).assert().success();
    // Manually create a broken env (dir but no manifest).
    let broken = e.env_dir("broken");
    std::fs::create_dir_all(&broken).unwrap();
    let out = e.aenv().arg("list").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("broken"));
    assert!(stdout.contains("good"));
    assert!(
        stdout.contains("(broken)") || stdout.contains("!"),
        "broken env should be marked: {stdout}"
    );
}

#[test]
fn import_profile_rejects_bundle_with_symlinks() {
    // Hand-craft a malicious bundle: a tar.gz containing a regular file
    // and a symlink. The safe extractor must skip the symlink entry, and
    // the post-extract walker must see no symlinks under the staged tree.
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();

    let archive_path = e.home_path().join("evil.aenv.tar.gz");
    {
        let f = std::fs::File::create(&archive_path).unwrap();
        let gz = GzEncoder::new(f, Compression::default());
        let mut tar = tar::Builder::new(gz);
        // Required manifest so import progresses past the parse step.
        let manifest_body = b"aenv_schema_version = \"2\"\n[env]\nname = \"src\"\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("aenv.toml").unwrap();
        header.set_size(manifest_body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &manifest_body[..]).unwrap();
        // The symlink entry — should be silently skipped by unpack_safe.
        let mut sl = tar::Header::new_gnu();
        sl.set_entry_type(tar::EntryType::Symlink);
        sl.set_path("escape").unwrap();
        sl.set_link_name("/tmp/aenv-escape-test").unwrap();
        sl.set_size(0);
        sl.set_mode(0o777);
        sl.set_cksum();
        tar.append(&sl, &[][..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }
    e.aenv()
        .args([
            "import-profile",
            archive_path.to_str().unwrap(),
            "--name",
            "imported",
        ])
        .assert()
        .success();
    // Imported env exists and contains no symlink at the escape path.
    assert!(e.env_dir("imported").join("aenv.toml").is_file());
    assert!(!e.env_dir("imported").join("escape").exists());
    // Defense-in-depth: the test target /tmp/aenv-escape-test must not have
    // been written through.
    assert!(!std::path::Path::new("/tmp/aenv-escape-test").exists());
}

#[test]
fn lock_command_does_not_materialize_into_env() {
    // `aenv lock` should rewrite aenv.lock only — env's plugins dir should
    // stay untouched. Contrast: `aenv install` materializes.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p1", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p1",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();

    let plugins_dir = e.env_dir("x").join(".claude/plugins");
    let count_before = std::fs::read_dir(&plugins_dir)
        .map(|rd| rd.count())
        .unwrap_or(0);

    e.aenv().args(["lock", "-E", "x"]).assert().success();

    let count_after = std::fs::read_dir(&plugins_dir)
        .map(|rd| rd.count())
        .unwrap_or(0);
    assert_eq!(
        count_before, count_after,
        "aenv lock should not materialize plugins into env"
    );
    // But aenv.lock must now be populated.
    let lock = std::fs::read_to_string(e.env_dir("x").join("aenv.lock")).unwrap();
    assert!(
        lock.contains("name = \"p1\""),
        "lockfile not populated: {lock}"
    );
}

#[test]
fn sync_prunes_managed_dirs_not_in_lockfile() {
    // After install, simulate a stale managed plugin dir by adding one
    // manually with the .aenv-managed marker. `aenv sync` must remove it
    // because it isn't in the lockfile.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "real", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "real",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    let stale_managed = e.env_dir("x").join(".claude/plugins/skill-zombie");
    std::fs::create_dir_all(&stale_managed).unwrap();
    std::fs::write(stale_managed.join(".aenv-managed"), b"aenv-managed\n").unwrap();
    let stale_user = e.env_dir("x").join(".claude/plugins/user-installed");
    std::fs::create_dir_all(&stale_user).unwrap();

    e.aenv().args(["sync", "-E", "x"]).assert().success();

    assert!(
        !stale_managed.exists(),
        "managed stale dir should be pruned"
    );
    assert!(
        stale_user.exists(),
        "user-installed dir should be preserved"
    );
    assert!(e
        .env_dir("x")
        .join(".claude/plugins/real/README.md")
        .is_file());
}

#[test]
fn lockfile_with_traversal_name_rejected_at_load() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let lock_path = e.env_dir("x").join("aenv.lock");
    let body = format!(
        r#"schema_version = "3"

[[plugins]]
name = "../escape"
version = "1.0.0"
sha256 = "{}"
source = "/tmp/whatever"
"#,
        "a".repeat(64)
    );
    std::fs::write(&lock_path, body).unwrap();
    let out = e.aenv().args(["sync", "-E", "x"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("escape") || stderr.contains("plugin name"),
        "expected name validation error, got: {stderr}"
    );
}

#[test]
fn lockfile_with_bad_sha256_rejected_at_load() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let lock_path = e.env_dir("x").join("aenv.lock");
    let body = r#"schema_version = "3"

[[plugins]]
name = "good"
version = "1.0.0"
sha256 = "../etc/passwd"
source = "/tmp/whatever"
"#;
    std::fs::write(&lock_path, body).unwrap();
    let out = e.aenv().args(["sync", "-E", "x"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sha256") || stderr.contains("hex"),
        "expected sha256 validation error, got: {stderr}"
    );
}

#[test]
fn secrets_add_rejects_collision_prone_keys() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    // gh-token would collide with gh.token / gh_token under the old
    // sanitization. Reject at add-time so the env var bridge stays 1:1.
    let out = e
        .aenv()
        .args(["secrets", "add", "gh-token", "--value", "x", "-E", "x"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("[A-Za-z") || stderr.contains("POSIX"));
}

#[test]
fn install_picks_up_source_change_via_re_add() {
    // After re-`aenv add` with a different source, install must refetch
    // (the cache predicate now includes source).
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p1 = make_local_plugin(e.home_path(), "v1src", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p",
            "-E",
            "x",
            "--source",
            p1.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let body1 =
        std::fs::read_to_string(e.env_dir("x").join(".claude/plugins/p/README.md")).unwrap();
    assert_eq!(body1, "v1");

    // Switch source to a different local dir.
    let p2 = make_local_plugin(e.home_path(), "v2src", "v2");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p",
            "-E",
            "x",
            "--source",
            p2.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let body2 =
        std::fs::read_to_string(e.env_dir("x").join(".claude/plugins/p/README.md")).unwrap();
    assert_eq!(body2, "v2", "install ignored manifest source change");
}

#[cfg(unix)] // mode bits — Windows lock_down_* is intentionally a no-op
#[test]
fn imported_env_dir_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "src"]).assert().success();
    let bundle = e.home_path().join("src.aenv.tar.gz");
    e.aenv()
        .args([
            "export-profile",
            "-E",
            "src",
            "-o",
            bundle.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "import-profile",
            bundle.to_str().unwrap(),
            "--name",
            "imported",
        ])
        .assert()
        .success();
    let mode = std::fs::metadata(e.env_dir("imported"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode & 0o077, 0, "imported env not locked down: {mode:o}");
    let claude_mode = std::fs::metadata(e.env_dir("imported").join(".claude"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        claude_mode & 0o077,
        0,
        "imported .claude leaks: {claude_mode:o}"
    );
}

#[test]
fn http_plaintext_source_is_rejected() {
    // Plain http:// is MITM-able. With auto-apply, `aenv add` is
    // the moment fetch runs, so the rejection surfaces here rather
    // than at a deferred install. Pinned so a future refactor that
    // ever defers fetch (or weakens transport validation) gets
    // caught at the entry point users actually use.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let out = e
        .aenv()
        .args([
            "add",
            "plugin",
            "evil",
            "-E",
            "x",
            "--source",
            "http://example.com/plugin.tgz",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plaintext") || stderr.contains("http"),
        "expected plaintext-http rejection, got: {stderr}"
    );
}

#[test]
fn export_into_env_root_is_rejected() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let inside = e.env_dir("x").join("self.tar.gz");
    let out = e
        .aenv()
        .args(["export-profile", "-E", "x", "-o", inside.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("inside the env root"),
        "expected env-root rejection, got: {stderr}"
    );
}

#[test]
fn add_settings_in_tx_capture_so_rollback_restores() {
    // Settings.json must be in the tx capture set so a rollback
    // after a mutator restores not just the manifest but also the
    // env-local rendered state. Auto-apply means `aenv add mcp` is
    // what writes mcpServers (no separate `install` step needed),
    // so the test exercises that path's tx envelope.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let settings = e.env_dir("x").join(".claude/settings.json");
    std::fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
    e.aenv()
        .args(["add", "mcp", "github", "-E", "x", "--", "true"])
        .assert()
        .success();
    let after = std::fs::read_to_string(&settings).unwrap();
    assert!(after.contains("mcpServers"));
    e.aenv().arg("rollback").assert().success();
    let restored = std::fs::read_to_string(&settings).unwrap();
    assert!(
        !restored.contains("mcpServers"),
        "rollback should restore settings.json: got {restored}"
    );
    assert!(restored.contains("dark"));
}

#[test]
fn sync_registers_plugins_and_skills_in_native_json() {
    // The decisive contract: `aenv sync` doesn't just hardlink
    // plugin dirs and skill wrappers — it ALSO writes the native
    // JSON files (installed_plugins.json, known_marketplaces.json,
    // settings.json::enabledPlugins) so Claude Code's `cG()`
    // discovery actually sees the materialized state. Pre-fix,
    // sync materialized dirs but skipped the native registration
    // path entirely, so a fresh `git clone && aenv sync` left
    // claude blind even though sync reported success.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p1", "v1");
    let sk = make_local_skill(e.home_path(), "s1", "# Skill\n");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p1",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "skill",
            "s1",
            "-E",
            "x",
            "--source",
            sk.to_str().unwrap(),
        ])
        .assert()
        .success();

    // Wipe the env's native JSON to simulate the
    // "lockfile committed, fresh checkout, sync-only" workflow.
    let plugins_dir = e.env_dir("x").join(".claude/plugins");
    let ip = plugins_dir.join("installed_plugins.json");
    let km = plugins_dir.join("known_marketplaces.json");
    let settings_path = e.env_dir("x").join(".claude/settings.json");
    let _ = std::fs::remove_file(&ip);
    let _ = std::fs::remove_file(&km);
    let _ = std::fs::remove_file(&settings_path);

    e.aenv().args(["sync", "-E", "x"]).assert().success();

    // Plugin landed under aenv-local (local file source).
    let ip_body =
        std::fs::read_to_string(&ip).expect("installed_plugins.json must exist post-sync");
    let parsed: serde_json::Value = serde_json::from_str(&ip_body).unwrap();
    assert_eq!(parsed["version"], 2);
    let plugin_key = "p1@aenv-local";
    assert!(
        parsed["plugins"][plugin_key].is_array(),
        "sync must register the locked plugin under aenv-local: {ip_body}"
    );
    // Skill wrapper landed under aenv-skills.
    let skill_key = "skill-s1@aenv-skills";
    assert!(
        parsed["plugins"][skill_key].is_array(),
        "sync must register the locked skill wrapper under aenv-skills: {ip_body}"
    );
    // Both are tagged _aenv: true so future prune knows they're ours.
    assert_eq!(parsed["plugins"][plugin_key][0]["_aenv"], true);
    assert_eq!(parsed["plugins"][skill_key][0]["_aenv"], true);

    // settings.json::enabledPlugins reflects both.
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["enabledPlugins"][plugin_key], true);
    assert_eq!(settings["enabledPlugins"][skill_key], true);

    // Synthetic marketplace manifests were written so claude's
    // resolver can validate the entries — without these, sync
    // success would still produce "Plugin not found in
    // marketplace" inside claude.
    assert!(
        plugins_dir
            .join("marketplaces/aenv-local/.claude-plugin/marketplace.json")
            .is_file(),
        "aenv-local synthetic marketplace.json missing post-sync"
    );
    assert!(
        plugins_dir
            .join("marketplaces/aenv-skills/.claude-plugin/marketplace.json")
            .is_file(),
        "aenv-skills synthetic marketplace.json missing post-sync"
    );
}

#[test]
fn sync_prunes_managed_regular_plugins_too() {
    // Install a plugin via aenv, drop it from the lockfile, run sync.
    // The plugin dir (with .aenv-managed marker we now write) must be
    // pruned, even though it's not a skill wrapper.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p1", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p1",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let p_dir = e.env_dir("x").join(".claude/plugins/p1");
    assert!(p_dir.join(".aenv-managed").is_file());

    // Drop p1 from manifest. Re-running install must prune the managed
    // dir (it now has the .aenv-managed marker, so prune_removed_entries
    // recognizes it).
    e.aenv()
        .args(["rm", "plugin", "p1", "-E", "x"])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    assert!(
        !p_dir.exists(),
        "managed regular plugin should be pruned after rm + install"
    );
}

#[test]
fn manifest_add_holds_global_lock() {
    // Smoke test: aenv add succeeds even when called serially. The lock
    // ensures no race; we don't try to construct a parallel race here
    // since shared keychain state would be flaky in CI.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p", "x");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "skill",
            "s",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
}

#[test]
fn corrupt_global_config_surfaces_error() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let cfg = e.home_path().join("config.toml");
    std::fs::write(&cfg, "this is = = not toml").unwrap();
    // Run from a clean cwd so .aenv-version walk-up doesn't short-circuit
    // global_default() resolution (the test process inherits cargo's cwd
    // which sits inside this repo and has its own .aenv-version).
    let clean = e.home_path().join("clean");
    std::fs::create_dir_all(&clean).unwrap();
    let out = e
        .aenv()
        .arg("current")
        .current_dir(&clean)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parse") || stderr.contains("config"),
        "expected parse-error surfacing, got: {stderr}"
    );
}

#[cfg(unix)] // mode bits — Windows lock_down_* is intentionally a no-op
#[test]
fn cloned_env_is_owner_only_and_cleaned_on_failure() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "src"]).assert().success();
    e.aenv()
        .args(["new", "dup", "--from", "src"])
        .assert()
        .success();
    let mode = std::fs::metadata(e.env_dir("dup"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode & 0o077, 0, "cloned env not locked down: {mode:o}");
}

#[test]
fn git_plaintext_transport_rejected() {
    // Plaintext git over HTTP is MITM-able; aenv refuses it. With
    // auto-apply, fetch happens during `aenv add` itself, so the
    // rejection now lands there rather than at a deferred install.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let out = e
        .aenv()
        .args([
            "add",
            "plugin",
            "evil",
            "-E",
            "x",
            "--source",
            "git+http://example.com/plugin.git",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("plaintext git"),
        "expected git plaintext rejection, got: {stderr}"
    );
}

// Cross-platform invariant: mode bits are intentionally outside the
// hash so a Mac dev's `aenv.lock` is verifiable on Windows / Linux
// without regenerating. End-to-end: install, chmod, re-install — sha
// must remain stable.
#[cfg(unix)]
#[test]
fn hash_stable_across_executable_bit_flip() {
    use std::os::unix::fs::PermissionsExt;
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p", "v1");
    let script = p.join("run.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    e.aenv()
        .args([
            "add",
            "plugin",
            "p",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let lock1 = std::fs::read_to_string(e.env_dir("x").join("aenv.lock")).unwrap();
    let sha1_line = lock1.lines().find(|l| l.contains("sha256")).unwrap();

    // chmod +x, force re-fetch (zero-out the locked sha so the cache
    // predicate misses), re-run install — without the cross-platform
    // fix, mode would be in the hash and we'd see a different sha;
    // with the fix, the sha is content-only and must match.
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    let lock_path = e.env_dir("x").join("aenv.lock");
    let lock_contents = std::fs::read_to_string(&lock_path).unwrap();
    std::fs::write(
        &lock_path,
        lock_contents.replace(
            sha1_line,
            "sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"",
        ),
    )
    .unwrap();
    e.aenv().args(["install", "-E", "x"]).assert().success();
    let lock2 = std::fs::read_to_string(e.env_dir("x").join("aenv.lock")).unwrap();
    let sha2_line = lock2.lines().find(|l| l.contains("sha256")).unwrap();
    assert_eq!(
        sha1_line, sha2_line,
        "content unchanged → sha must match regardless of mode bits"
    );

    // And the materialized hook script must still be executable —
    // ensure_shebang_executable() compensates for mode-out-of-hash by
    // re-applying +x to anything starting with `#!`.
    let materialized = e
        .env_dir("x")
        .join(".claude")
        .join("plugins")
        .join("p")
        .join("run.sh");
    let mode = std::fs::metadata(&materialized)
        .unwrap()
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "materialized shebang script must be executable: mode = {mode:o}"
    );
}

#[test]
fn materialize_detects_corrupted_store_object() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p", "v1");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p",
            "-E",
            "x",
            "--source",
            p.to_str().unwrap(),
        ])
        .assert()
        .success();
    e.aenv().args(["install", "-E", "x"]).assert().success();

    // Corrupt the store object — flip a byte in a stored file.
    let store_objects = e.home_path().join("store/objects");
    let mut store_files = vec![];
    for entry in walkdir::WalkDir::new(&store_objects) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            store_files.push(entry.path().to_path_buf());
        }
    }
    let target = store_files.first().expect("store has at least one file");
    let mut body = std::fs::read(target).unwrap();
    body.push(b'!');
    std::fs::write(target, &body).unwrap();

    // sync should detect the corruption when re-materializing.
    let out = e.aenv().args(["sync", "-E", "x"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("corrupted") || stderr.contains("re-hash"),
        "expected corruption detection, got: {stderr}"
    );
}

#[test]
fn env_banner_shows_active_env_name() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "demo"]).assert().success();
    let mut cmd = e.aenv();
    cmd.env_remove("AENV_QUIET");
    cmd.args(["list"]).env("AENV_OVERRIDE", "demo");
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[demo]"),
        "expected [demo] banner, got: {stderr}"
    );
}

#[test]
fn env_banner_silent_under_global_fallback() {
    // Pre-pivot, unresolved cwd produced `[no-env]`. The new model
    // resolves to `global` (alias for ~/.claude) and we suppress the
    // banner entirely — when the user is using their real ~/.claude,
    // the shell should look exactly like a non-aenv shell. Mirrors
    // the shell-init `_aenv_prompt` rule for the same case.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let clean = e.home_path().join("clean");
    std::fs::create_dir_all(&clean).unwrap();
    let mut cmd = e.aenv();
    cmd.env_remove("AENV_QUIET");
    let out = cmd.args(["list"]).current_dir(&clean).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("[global]"),
        "global must be silent in the banner; got: {stderr}"
    );
    assert!(
        !stderr.contains("[no-env]"),
        "[no-env] should never appear post-pivot; got: {stderr}"
    );
}

#[test]
fn env_banner_suppressed_for_help() {
    let e = Env::new();
    let mut cmd = e.aenv();
    cmd.env_remove("AENV_QUIET");
    let out = cmd.args(["--help"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("[no-env]"),
        "banner should not print on --help: {stderr}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn init_no_guidance_suppresses_shell_wiring_block() {
    // `aenv upgrade` invokes `aenv init --force --no-guidance` so the
    // already-wired user doesn't see the "echo 'eval ...' >> ~/.zshrc"
    // guidance again (re-running it would duplicate their rc line).
    let e = Env::new();
    let out = e
        .aenv()
        .args(["init", "--no-default", "--no-guidance"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    // Info lines stay — useful to confirm what was refreshed.
    assert!(stdout.contains("aenv: home"));
    assert!(stdout.contains("aenv: shim"));
    // Wiring guidance must be absent.
    assert!(
        !stdout.contains("Wire your shell"),
        "guidance leaked into --no-guidance output: {stdout}"
    );
    assert!(
        !stdout.contains("shell-init"),
        "shell-init reference leaked: {stdout}"
    );
}

#[test]
fn upgrade_dry_run_prints_command_plan_without_invoking_cargo() {
    // The dry-run path is the only way to exercise the upgrade flow in
    // CI without actually running `cargo install` (network + side
    // effect on the runner's ~/.cargo/bin). Verifies the two key
    // pieces: the cargo command we'd run, and the path of the new
    // binary we'd then spawn for the refresh step.
    let e = Env::new();
    let out = e.aenv().args(["upgrade", "--dry-run"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("cargo install --git"),
        "missing cargo install line: {stdout}"
    );
    assert!(
        stdout.contains("--force --locked"),
        "missing --force --locked: {stdout}"
    );
    assert!(
        stdout.contains("init --force --no-guidance --no-default"),
        "missing refresh-step plan: {stdout}"
    );
}

#[test]
fn use_writes_aenv_version() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "y"]).assert().success();
    let proj = e.home_path().join("proj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["use", "y"])
        .current_dir(&proj)
        .assert()
        .success();
    let pin = std::fs::read_to_string(proj.join(".aenv-version")).unwrap();
    assert_eq!(pin.trim(), "y");
}

// =====================================================================
//   Project-local manifest mode
// =====================================================================
//
// Inspired by poetry/uv/pnpm: the project ships `aenv.toml` + `aenv.lock`
// (committed to git), and aenv discovers it via cwd walk-up. The
// materialized env stays at `~/.aenv/envs/<slot>/` — but the slot name
// is path-hashed (pipenv pattern) so two clones of the same repo never
// collide on the same on-disk state.

#[test]
fn init_here_writes_project_manifest_and_lock() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    let manifest = std::fs::read_to_string(proj.join("aenv.toml")).unwrap();
    assert!(
        manifest.contains("name = \"alpha\""),
        "manifest missing [env].name: {manifest}"
    );
    assert!(proj.join("aenv.lock").is_file(), "aenv.lock not written");
}

#[test]
fn init_here_default_name_uses_basename() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("frobozz");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here"])
        .current_dir(&proj)
        .assert()
        .success();
    let manifest = std::fs::read_to_string(proj.join("aenv.toml")).unwrap();
    assert!(
        manifest.contains("name = \"frobozz\""),
        "default name should be cwd basename: {manifest}"
    );
}

#[test]
fn init_here_refuses_to_clobber_existing_manifest() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    std::fs::write(proj.join("aenv.toml"), "# original").unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .failure();
    let still = std::fs::read_to_string(proj.join("aenv.toml")).unwrap();
    assert_eq!(still, "# original");
}

#[test]
fn project_manifest_resolves_via_walk_up() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    let nested = proj.join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    e.aenv()
        .args(["current"])
        .current_dir(&nested)
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"));
}

#[test]
fn project_manifest_takes_priority_over_aenv_version() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    // Plant a stale .aenv-version that points at a different name; the
    // project manifest must still win.
    std::fs::write(proj.join(".aenv-version"), "stale-name\n").unwrap();
    e.aenv()
        .args(["current"])
        .current_dir(&proj)
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"));
}

#[test]
fn project_manifest_slot_is_path_hashed() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    // status output reveals the slot name; check it's `alpha-<8 hex>`,
    // not bare "alpha" (which would collide with global envs).
    let out = e
        .aenv()
        .args(["status"])
        .current_dir(&proj)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The status output prints "env: <slot>" — pull the line and parse.
    let env_line = stdout
        .lines()
        .find(|l| l.starts_with("env:"))
        .expect("status missing env: line");
    let slot = env_line.trim_start_matches("env:").trim();
    assert!(
        slot.starts_with("alpha-") && slot.len() == "alpha-".len() + 8,
        "slot should be 'alpha-<sha8>': got '{slot}'"
    );
}

#[test]
fn project_clones_at_different_paths_get_different_slots() {
    // Two project dirs with the same [env].name must end up in distinct
    // slot dirs — otherwise materialized state silently overwrites
    // between clones.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let p1 = e.home_path().join("clone-a/myproj");
    let p2 = e.home_path().join("clone-b/myproj");
    std::fs::create_dir_all(&p1).unwrap();
    std::fs::create_dir_all(&p2).unwrap();
    e.aenv()
        .args(["init", "--here", "shared-name"])
        .current_dir(&p1)
        .assert()
        .success();
    e.aenv()
        .args(["init", "--here", "shared-name"])
        .current_dir(&p2)
        .assert()
        .success();
    let slot1 = read_slot(&e, &p1);
    let slot2 = read_slot(&e, &p2);
    assert_ne!(slot1, slot2, "clones must hash to distinct slots");
    assert!(slot1.starts_with("shared-name-"));
    assert!(slot2.starts_with("shared-name-"));
}

fn read_slot(e: &Env, project_dir: &std::path::Path) -> String {
    let out = e
        .aenv()
        .args(["status"])
        .current_dir(project_dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find(|l| l.starts_with("env:"))
        .unwrap()
        .trim_start_matches("env:")
        .trim()
        .to_string()
}

#[test]
fn project_mode_add_writes_to_cwd_manifest_not_slot() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "mcp",
            "demo",
            "--command",
            "true",
            "--env-var",
            "K=v",
        ])
        .current_dir(&proj)
        .assert()
        .success();
    let manifest = std::fs::read_to_string(proj.join("aenv.toml")).unwrap();
    assert!(
        manifest.contains("[mcp.demo]"),
        "add must write to cwd manifest: {manifest}"
    );
    // The slot dir should NOT have an aenv.toml — the project manifest
    // is the source of truth.
    let slot = read_slot(&e, &proj);
    let slot_manifest = e.home_path().join("envs").join(&slot).join("aenv.toml");
    assert!(
        !slot_manifest.exists(),
        "slot must not get a duplicate manifest: {}",
        slot_manifest.display()
    );
}

#[test]
fn project_mode_install_writes_lockfile_to_project_dir() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    // Plant a local plugin source so install resolves without network.
    let plug = make_local_plugin(e.home_path(), "foo", "hello");
    e.aenv()
        .args(["add", "plugin", "foo", "--source", plug.to_str().unwrap()])
        .current_dir(&proj)
        .assert()
        .success();
    e.aenv()
        .args(["install"])
        .current_dir(&proj)
        .assert()
        .success();
    let lock_body =
        std::fs::read_to_string(proj.join("aenv.lock")).expect("project lockfile must exist");
    assert!(
        lock_body.contains("[[plugins]]"),
        "lockfile missing plugins entry: {lock_body}"
    );
    assert!(
        lock_body.contains("name = \"foo\""),
        "lockfile missing foo entry: {lock_body}"
    );
    // The slot's lockfile should NOT exist (project lock is authoritative).
    let slot = read_slot(&e, &proj);
    assert!(
        !e.home_path()
            .join("envs")
            .join(&slot)
            .join("aenv.lock")
            .exists(),
        "slot must not have a lockfile copy",
    );
}

// =====================================================================
//   Cross-platform sharing
// =====================================================================
//
// `aenv.toml` + `aenv.lock` get committed to git. Their bytes need to
// be byte-identical across macOS / Linux / Windows checkouts so sha256
// verification round-trips. These tests cover:
//   - .gitattributes auto-gen (matches Go's `* -text` repo policy)
//   - schema enrichment for multi-platform native binaries
//   - aenv doctor warnings for non-portable manifests / lockfiles

#[test]
fn init_here_writes_gitattributes_protecting_aenv_files() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    let body = std::fs::read_to_string(proj.join(".gitattributes")).unwrap();
    assert!(
        body.contains("aenv.toml") && body.contains("aenv.lock") && body.contains("-text"),
        ".gitattributes must protect aenv.{{toml,lock}} from line-ending normalization. \
         Got:\n{body}"
    );
}

#[test]
fn init_here_appends_aenv_rules_to_existing_gitattributes() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    let pre_existing = "*.sh text eol=lf\n";
    std::fs::write(proj.join(".gitattributes"), pre_existing).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    let body = std::fs::read_to_string(proj.join(".gitattributes")).unwrap();
    assert!(
        body.starts_with(pre_existing),
        "must preserve existing rules: {body}"
    );
    assert!(body.contains("aenv.lock"), "must append aenv rules: {body}");
}

#[test]
fn init_here_does_not_re_append_if_aenv_rules_already_present() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    let pre_existing = "aenv.toml -text\naenv.lock -text\n";
    std::fs::write(proj.join(".gitattributes"), pre_existing).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    let body = std::fs::read_to_string(proj.join(".gitattributes")).unwrap();
    assert_eq!(body, pre_existing);
}

#[test]
fn manifest_supports_supported_platforms_block() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    // Hand-edit the manifest to exercise the new schema.
    let manifest = proj.join("aenv.toml");
    let body = std::fs::read_to_string(&manifest).unwrap();
    let augmented = format!(
        "{body}\n\
         [platforms]\n\
         required = [\"darwin-arm64\", \"linux-x86_64\"]\n"
    );
    std::fs::write(&manifest, augmented).unwrap();
    e.aenv()
        .args(["current"])
        .current_dir(&proj)
        .assert()
        .success();
}

#[test]
fn doctor_warns_on_missing_gitattributes_in_project_mode() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    std::fs::remove_file(proj.join(".gitattributes")).unwrap();
    let out = e
        .aenv()
        .args(["doctor"])
        .current_dir(&proj)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains(".gitattributes"),
        "doctor should warn about missing .gitattributes: {combined}"
    );
}

#[test]
fn doctor_flags_file_url_source_in_project_manifest() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    let manifest = proj.join("aenv.toml");
    let body = std::fs::read_to_string(&manifest).unwrap();
    let augmented = format!(
        "{body}\n\
         [[plugins.enabled]]\n\
         name = \"foo\"\n\
         source = \"file:///Users/me/local-plugin\"\n"
    );
    std::fs::write(&manifest, augmented).unwrap();
    let out = e
        .aenv()
        .args(["doctor"])
        .current_dir(&proj)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("file://") && combined.contains("non-portable"),
        "doctor should flag file:// as non-portable: {combined}"
    );
}

#[test]
fn doctor_flags_utf8_bom_in_manifest() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    let manifest = proj.join("aenv.toml");
    let body = std::fs::read(&manifest).unwrap();
    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
    with_bom.extend_from_slice(&body);
    std::fs::write(&manifest, with_bom).unwrap();
    let out = e
        .aenv()
        .args(["doctor"])
        .current_dir(&proj)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("BOM"),
        "doctor should flag UTF-8 BOM: {combined}"
    );
}

#[test]
fn doctor_flags_crlf_in_shell_scripts() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let proj = e.home_path().join("myproj");
    std::fs::create_dir(&proj).unwrap();
    e.aenv()
        .args(["init", "--here", "alpha"])
        .current_dir(&proj)
        .assert()
        .success();
    std::fs::write(proj.join("build.sh"), "#!/bin/bash\r\necho hi\r\n").unwrap();
    let out = e
        .aenv()
        .args(["doctor"])
        .current_dir(&proj)
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("CRLF") || combined.contains("build.sh"),
        "doctor should flag CRLF in shell scripts: {combined}"
    );
}

// =====================================================================
//   MCP add ergonomics — `claude mcp add` compatible grammar
// =====================================================================
//
// The grammar below mirrors Anthropic's `claude mcp add` exactly (with
// `--` separator + `-e KEY=VAL` + `--transport` + `--json`) so users
// don't need to learn a second syntax. Plus `--from <source>` for bulk
// import from existing tools' configs.

fn manifest_for(e: &Env, name: &str) -> String {
    std::fs::read_to_string(e.env_dir(name).join("aenv.toml")).unwrap()
}

#[test]
fn add_mcp_dash_dash_form_records_command_and_args() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    e.aenv()
        .args([
            "add",
            "mcp",
            "github",
            "-E",
            "x",
            "-e",
            "GITHUB_TOKEN=abc",
            "--",
            "npx",
            "-y",
            "@modelcontextprotocol/server-github",
        ])
        .assert()
        .success();
    let body = manifest_for(&e, "x");
    assert!(body.contains("[mcp.github]"), "{body}");
    assert!(body.contains("command = \"npx\""), "{body}");
    assert!(body.contains("\"-y\""), "args missing: {body}");
    assert!(
        body.contains("@modelcontextprotocol/server-github"),
        "{body}"
    );
    assert!(
        body.contains("GITHUB_TOKEN = \"abc\""),
        "env missing: {body}"
    );
}

#[test]
fn add_mcp_short_e_flag_is_repeatable() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    e.aenv()
        .args([
            "add", "mcp", "demo", "-E", "x", "-e", "K1=v1", "-e", "K2=v2", "--", "true",
        ])
        .assert()
        .success();
    let body = manifest_for(&e, "x");
    assert!(body.contains("K1 = \"v1\""), "{body}");
    assert!(body.contains("K2 = \"v2\""), "{body}");
}

#[test]
fn add_mcp_http_transport_with_url() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    e.aenv()
        .args([
            "add",
            "mcp",
            "notion",
            "-E",
            "x",
            "--transport",
            "http",
            "https://mcp.notion.com/mcp",
        ])
        .assert()
        .success();
    let body = manifest_for(&e, "x");
    assert!(body.contains("type = \"http\""), "{body}");
    assert!(
        body.contains("url = \"https://mcp.notion.com/mcp\""),
        "{body}"
    );
}

#[test]
fn add_mcp_json_form_round_trips() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    e.aenv()
        .args([
            "add",
            "mcp",
            "demo",
            "-E",
            "x",
            "--json",
            r#"{"command":"npx","args":["-y","@scope/X"],"env":{"K":"V"}}"#,
        ])
        .assert()
        .success();
    let body = manifest_for(&e, "x");
    assert!(body.contains("command = \"npx\""), "{body}");
    assert!(body.contains("@scope/X"), "{body}");
    assert!(body.contains("K = \"V\""), "{body}");
}

#[test]
fn add_mcp_legacy_command_form_still_works_with_hint() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let out = e
        .aenv()
        .args([
            "add",
            "mcp",
            "old",
            "-E",
            "x",
            "--command",
            "true",
            "--arg=-x",
            "--env-var",
            "K=V",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("hint") && stderr.contains("--"),
        "should hint to use new -- form: {stderr}"
    );
    let body = manifest_for(&e, "x");
    assert!(body.contains("command = \"true\""));
    assert!(body.contains("\"-x\""));
    assert!(body.contains("K = \"V\""));
}

#[test]
fn add_mcp_from_path_imports_wrapped_mcp_servers_shape() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let cfg = e.home_path().join("import.json");
    std::fs::write(
        &cfg,
        r#"{
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@scope/X"],
                    "env": {"T": "1"}
                },
                "notion": {
                    "type": "http",
                    "url": "https://mcp.notion.com/mcp"
                }
            }
        }"#,
    )
    .unwrap();
    e.aenv()
        .args(["add", "mcp", "-E", "x", "--from", cfg.to_str().unwrap()])
        .assert()
        .success();
    let body = manifest_for(&e, "x");
    assert!(body.contains("[mcp.github]"), "{body}");
    assert!(body.contains("[mcp.notion]"), "{body}");
    assert!(body.contains("type = \"http\""), "{body}");
    assert!(
        body.contains("url = \"https://mcp.notion.com/mcp\""),
        "{body}"
    );
}

#[test]
fn add_mcp_from_cursor_deeplink_imports_single_entry() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    // Construct a Cursor-style deeplink with base64-url-encoded config.
    // {"command":"npx","args":["-y","@scope/X"]}
    let json = r#"{"command":"npx","args":["-y","@scope/X"]}"#;
    let b64 = base64_url_encode_test(json.as_bytes());
    let deeplink =
        format!("cursor://anysphere.cursor-deeplink/mcp/install?name=cursor-imp&config={b64}");
    e.aenv()
        .args([
            "add",
            "mcp",
            "-E",
            "x",
            "--from",
            &format!("cursor-deeplink:{deeplink}"),
        ])
        .assert()
        .success();
    let body = manifest_for(&e, "x");
    assert!(
        body.contains("[mcp.cursor-imp]") || body.contains("[mcp.\"cursor-imp\"]"),
        "{body}"
    );
    assert!(body.contains("@scope/X"), "{body}");
}

#[test]
fn add_mcp_without_command_or_url_errors_with_examples() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "x"]).assert().success();
    let out = e
        .aenv()
        .args(["add", "mcp", "naked", "-E", "x"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("aenv add mcp"),
        "error should include usage example: {combined}"
    );
}

fn base64_url_encode_test(bytes: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = if i + 1 < bytes.len() {
            bytes[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < bytes.len() {
            bytes[i + 2] as u32
        } else {
            0
        };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(ALPHA[((n >> 6) & 63) as usize] as char);
        }
        if i + 2 < bytes.len() {
            out.push(ALPHA[(n & 63) as usize] as char);
        }
        i += 3;
    }
    out
}

// =====================================================================
//   ifl — Import From List (non-interactive form only)
// =====================================================================
//
// `cargo test` has no TTY, so the TUI itself can't be exercised here.
// All tests below drive the non-interactive flag form (`--from <env>
// [--plugin/--skill/--mcp]...`) which exercises the same `Plan` +
// `apply` core logic the TUI uses.

#[test]
fn ifl_refuses_when_no_other_envs_exist() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "only"]).assert().success();
    let out = e.aenv().args(["ifl", "-E", "only"]).output().unwrap();
    assert!(
        out.status.success(),
        "should exit 0 with a friendly message"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("no other envs"),
        "expected 'no other envs' message: {combined}"
    );
}

#[test]
fn ifl_refuses_tui_without_force_tty_in_test_runner() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.aenv().args(["new", "beta"]).assert().success();
    let out = e.aenv().args(["ifl", "-E", "beta"]).output().unwrap();
    assert!(!out.status.success(), "TUI must refuse without TTY");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("non-interactive") || combined.contains("--from"),
        "should hint non-interactive form: {combined}"
    );
}

#[test]
fn ifl_non_interactive_imports_all_items_from_one_env() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.aenv().args(["new", "beta"]).assert().success();
    let foo = make_local_plugin(e.home_path(), "foo-src", "foo");
    let review = make_local_skill(e.home_path(), "review-src", "# Review\n");
    e.aenv()
        .args([
            "add",
            "plugin",
            "foo",
            "-E",
            "alpha",
            "--source",
            foo.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "skill",
            "review",
            "-E",
            "alpha",
            "--source",
            review.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args(["add", "mcp", "github", "-E", "alpha", "--", "true"])
        .assert()
        .success();

    e.aenv()
        .args(["ifl", "-E", "beta", "--from", "alpha"])
        .assert()
        .success();

    let body = std::fs::read_to_string(e.env_dir("beta").join("aenv.toml")).unwrap();
    assert!(body.contains("foo"), "plugin not imported: {body}");
    assert!(body.contains("review"), "skill not imported: {body}");
    assert!(body.contains("[mcp.github]"), "mcp not imported: {body}");
    assert!(
        e.env_dir("beta")
            .join(".claude/plugins/foo/README.md")
            .is_file(),
        "ifl should apply plugin immediately"
    );
    assert!(
        e.env_dir("beta")
            .join(".claude/plugins/skill-review/skills/review/SKILL.md")
            .is_file(),
        "ifl should apply skill immediately"
    );
    let settings =
        std::fs::read_to_string(e.env_dir("beta").join(".claude/settings.json")).unwrap();
    assert!(
        settings.contains("\"github\""),
        "ifl should render mcp settings immediately: {settings}"
    );
}

#[test]
fn ifl_non_interactive_imports_only_named_items() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.aenv().args(["new", "beta"]).assert().success();
    let foo = make_local_plugin(e.home_path(), "foo-src", "foo");
    let bar = make_local_plugin(e.home_path(), "bar-src", "bar");
    e.aenv()
        .args([
            "add",
            "plugin",
            "foo",
            "-E",
            "alpha",
            "--source",
            foo.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "plugin",
            "bar",
            "-E",
            "alpha",
            "--source",
            bar.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args(["ifl", "-E", "beta", "--from", "alpha", "--plugin", "foo"])
        .assert()
        .success();
    let body = std::fs::read_to_string(e.env_dir("beta").join("aenv.toml")).unwrap();
    assert!(body.contains("\"foo\""), "foo missing: {body}");
    assert!(
        !body.contains("\"bar\""),
        "bar should NOT be imported: {body}"
    );
}

#[test]
fn ifl_first_check_wins_across_multiple_sources() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.aenv().args(["new", "beta"]).assert().success();
    e.aenv().args(["new", "target"]).assert().success();
    let alpha_plugin = make_local_plugin(e.home_path(), "shared-alpha-src", "alpha");
    let beta_plugin = make_local_plugin(e.home_path(), "shared-beta-src", "beta");
    e.aenv()
        .args([
            "add",
            "plugin",
            "shared",
            "-E",
            "alpha",
            "--source",
            alpha_plugin.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "plugin",
            "shared",
            "-E",
            "beta",
            "--source",
            beta_plugin.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "ifl", "-E", "target", "--from", "alpha", "--from", "beta", "--plugin", "shared",
        ])
        .assert()
        .success();
    let body = std::fs::read_to_string(e.env_dir("target").join("aenv.toml")).unwrap();
    assert!(
        body.contains(&alpha_plugin.display().to_string()),
        "first source (alpha) should win: {body}"
    );
    assert!(
        !body.contains(&beta_plugin.display().to_string()),
        "later source (beta) should be ignored: {body}"
    );
}

#[test]
fn ifl_skips_items_already_in_target() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "alpha"]).assert().success();
    e.aenv().args(["new", "beta"]).assert().success();
    let alpha_plugin = make_local_plugin(e.home_path(), "shared-alpha-src", "alpha");
    let beta_plugin = make_local_plugin(e.home_path(), "shared-beta-src", "beta");
    e.aenv()
        .args([
            "add",
            "plugin",
            "shared",
            "-E",
            "alpha",
            "--source",
            alpha_plugin.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "plugin",
            "shared",
            "-E",
            "beta",
            "--source",
            beta_plugin.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    let out = e
        .aenv()
        .args(["ifl", "-E", "beta", "--from", "alpha"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The new ifl is bidirectional: items already in the target
    // round-trip as no-ops (skipped) and surface in the summary
    // count, not as a per-item warning. The target's existing
    // version must survive — source MUST NOT clobber.
    assert!(
        combined.contains("skipped"),
        "expected skipped-count summary: {combined}"
    );
    let body = std::fs::read_to_string(e.env_dir("beta").join("aenv.toml")).unwrap();
    assert!(
        body.contains(&beta_plugin.display().to_string()),
        "target's existing source must survive: {body}"
    );
    assert!(
        !body.contains(&alpha_plugin.display().to_string()),
        "source's plugin must NOT clobber target: {body}"
    );
}

#[test]
fn ifl_non_interactive_round_trip_for_existing_items_is_noop() {
    // Before the bidirectional pivot, importing an item that's
    // already in the target double-listed in summary (`imported 1`).
    // New behavior: it counts as skipped, target manifest unchanged,
    // 0 added.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "src"]).assert().success();
    e.aenv().args(["new", "tgt"]).assert().success();
    let p = make_local_plugin(e.home_path(), "p-src", "p");
    e.aenv()
        .args([
            "add",
            "plugin",
            "p",
            "-E",
            "src",
            "--source",
            p.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    e.aenv()
        .args([
            "add",
            "plugin",
            "p",
            "-E",
            "tgt",
            "--source",
            p.to_string_lossy().as_ref(),
        ])
        .assert()
        .success();
    let before = std::fs::read_to_string(e.env_dir("tgt").join("aenv.toml")).unwrap();
    let out = e
        .aenv()
        .args(["ifl", "-E", "tgt", "--from", "src"])
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("imported 0") && combined.contains("skipped"),
        "expected 0-added + skipped-count: {combined}"
    );
    let after = std::fs::read_to_string(e.env_dir("tgt").join("aenv.toml")).unwrap();
    assert_eq!(before, after, "manifest must be byte-identical after no-op");
}

#[test]
fn ifl_global_source_imports_plugins_and_mcps_from_real_claude_state() {
    // Synthesize a fake `~/.claude` under the test's HOME so the global
    // source detector sees real-shape plugin records + an MCP server.
    // Also synthesize Codex's installed skill layout: global ifl used
    // to expose skills, and users expect those to be selectable beside
    // plugins/MCPs.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "target"]).assert().success();

    let fake_home = e.home_path().join("user-home");
    let claude_dir = fake_home.join(".claude");
    std::fs::create_dir_all(claude_dir.join("plugins")).unwrap();
    let codex_plugin = make_local_plugin(e.home_path(), "codex-global-src", "codex");
    let review_plugin = make_local_plugin(e.home_path(), "review-global-src", "review");
    std::fs::write(
        claude_dir.join("plugins").join("installed_plugins.json"),
        r#"{
            "version": 2,
            "plugins": {
                "codex@openai-codex": [{
                    "scope": "user",
                    "version": "1.0.3"
                }],
                "code-review@claude-plugins-official": [{
                    "scope": "user",
                    "version": "unknown"
                }]
            }
        }"#,
    )
    .unwrap();
    std::fs::write(
        claude_dir.join("plugins").join("known_marketplaces.json"),
        r#"{
            "openai-codex": {
                "source": {"source": "url", "url": ""}
            },
            "claude-plugins-official": {
                "source": {"source": "url", "url": ""}
            }
        }"#,
    )
    .unwrap();
    for (marketplace, plugin_name, plugin_dir) in [
        ("openai-codex", "codex", &codex_plugin),
        ("claude-plugins-official", "code-review", &review_plugin),
    ] {
        let marketplace_dir = claude_dir
            .join("plugins")
            .join("marketplaces")
            .join(marketplace)
            .join(".claude-plugin");
        std::fs::create_dir_all(&marketplace_dir).unwrap();
        std::fs::write(
            marketplace_dir.join("marketplace.json"),
            format!(
                r#"{{"plugins":[{{"name":"{plugin_name}","source":{{"source":"url","url":"{}"}}}}]}}"#,
                plugin_dir.display()
            ),
        )
        .unwrap();
    }
    std::fs::write(
        fake_home.join(".claude.json"),
        r#"{
            "mcpServers": {
                "soma": {"command": "/usr/bin/soma", "args": []}
            }
        }"#,
    )
    .unwrap();
    let codex_skill = fake_home
        .join(".codex")
        .join("skills")
        .join(".system")
        .join("reviewer");
    std::fs::create_dir_all(&codex_skill).unwrap();
    std::fs::write(codex_skill.join("SKILL.md"), "# Reviewer\n").unwrap();

    e.aenv()
        .env("HOME", &fake_home)
        .args(["ifl", "-E", "target", "--from", "(global)"])
        .assert()
        .success();

    let body = std::fs::read_to_string(e.env_dir("target").join("aenv.toml")).unwrap();
    // Plugin imported with reconstructed source and applied immediately.
    assert!(body.contains("name = \"codex\""), "plugin missing: {body}");
    assert!(
        body.contains(&codex_plugin.display().to_string()),
        "expected reconstructed source URL: {body}"
    );
    assert!(
        body.contains("name = \"code-review\"")
            && body.contains(&review_plugin.display().to_string()),
        "expected marketplace plugin import: {body}"
    );
    assert!(
        e.env_dir("target")
            .join(".claude/plugins/codex/README.md")
            .is_file(),
        "global plugin should be applied immediately"
    );
    // MCP imported with command + args.
    assert!(body.contains("[mcp.soma]"), "mcp missing: {body}");
    assert!(
        body.contains("command = \"/usr/bin/soma\""),
        "mcp command missing: {body}"
    );
    // Skill imported with an env-local path source, so `aenv install`
    // can materialize it through the normal skill wrapper-plugin path.
    assert!(
        body.contains("name = \"reviewer\""),
        "skill missing: {body}"
    );
    assert!(
        body.contains(&format!("source = \"{}\"", codex_skill.display())),
        "skill source should point at the discovered SKILL.md directory: {body}"
    );
}

#[test]
fn ifl_global_source_can_be_skills_only_from_codex_home() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "target"]).assert().success();

    let fake_home = e.home_path().join("user-home-codex-skills-only");
    let skill = fake_home.join(".codex").join("skills").join("solo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "# Solo\n").unwrap();

    e.aenv()
        .env("HOME", &fake_home)
        .args([
            "ifl", "-E", "target", "--from", "(global)", "--skill", "solo",
        ])
        .assert()
        .success();

    let body = std::fs::read_to_string(e.env_dir("target").join("aenv.toml")).unwrap();
    assert!(body.contains("name = \"solo\""), "skill missing: {body}");
    assert!(
        body.contains(&format!("source = \"{}\"", skill.display())),
        "skill source should be the global Codex skill path: {body}"
    );
}

#[test]
fn ifl_no_global_source_when_no_claude_dir_exists() {
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "only"]).assert().success();
    // HOME points at a dir with no ~/.claude — global source should
    // not appear, and since 'only' is the only env, ifl bails.
    let fake_home = e.home_path().join("user-home-empty");
    std::fs::create_dir_all(&fake_home).unwrap();
    let out = e
        .aenv()
        .env("HOME", &fake_home)
        .args(["ifl", "-E", "only"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("no other envs"),
        "expected 'no other envs' (no global, no other envs): {combined}"
    );
}

#[test]
fn ifl_global_source_reads_mcp_from_claude_settings_json() {
    // Modern Claude Code routes `/mcp add` into
    // `~/.claude/settings.json::mcpServers`. (global) used to only
    // read legacy `~/.claude.json`, so anything added via the new
    // path was invisible to `aenv ifl --from "(global)"`. Pinned.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "target"]).assert().success();

    let fake_home = e.home_path().join("user-home-settings-mcp");
    std::fs::create_dir_all(fake_home.join(".claude")).unwrap();
    std::fs::write(
        fake_home.join(".claude/settings.json"),
        r#"{"mcpServers":{"settings-only-mcp":{"command":"/usr/bin/example","args":["--port","8080"]}}}"#,
    )
    .unwrap();

    e.aenv()
        .env("HOME", &fake_home)
        .args([
            "ifl",
            "-E",
            "target",
            "--from",
            "(global)",
            "--mcp",
            "settings-only-mcp",
        ])
        .assert()
        .success();
    let body = std::fs::read_to_string(e.env_dir("target").join("aenv.toml")).unwrap();
    assert!(
        body.contains("[mcp.settings-only-mcp]"),
        "settings.json mcp must be importable via (global): {body}"
    );
    assert!(
        body.contains("command = \"/usr/bin/example\""),
        "mcp spec fields must round-trip: {body}"
    );
}

#[test]
fn ifl_global_plugin_falls_back_to_install_path_when_marketplace_not_github() {
    // Plugins installed from a non-github marketplace (or from a
    // local path via `/plugin install`) have a `known_marketplaces`
    // entry whose `source` isn't `{source: github, ...}`. ifl used
    // to drop these to `source = None`, which then made apply_live
    // bail. The installPath fallback keeps the import working by
    // pointing at the local clone as a `file://` source.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "target"]).assert().success();

    let fake_home = e.home_path().join("user-home-local-plugin");
    let plugin_dir = fake_home.join(".claude/plugins/local-foo@private-mkt/abc123");
    std::fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
    std::fs::write(
        plugin_dir.join(".claude-plugin/plugin.json"),
        r#"{"name":"local-foo","version":"0.1.0"}"#,
    )
    .unwrap();
    std::fs::write(plugin_dir.join("README.md"), "hi").unwrap();

    let plugins_dir = fake_home.join(".claude/plugins");
    std::fs::write(
        plugins_dir.join("installed_plugins.json"),
        format!(
            r#"{{"version":2,"plugins":{{"local-foo@private-mkt":[{{"scope":"user","installPath":"{}"}}]}}}}"#,
            plugin_dir.display()
        ),
    )
    .unwrap();
    // Marketplace with a non-github source shape — the github
    // fallback can't reconstruct a URL from this.
    std::fs::write(
        plugins_dir.join("known_marketplaces.json"),
        r#"{"private-mkt":{"source":{"source":"local","kind":"manual"},"installLocation":"/tmp/x","lastUpdated":"2026-01-01T00:00:00Z"}}"#,
    )
    .unwrap();

    e.aenv()
        .env("HOME", &fake_home)
        .args([
            "ifl",
            "-E",
            "target",
            "--from",
            "(global)",
            "--plugin",
            "local-foo",
        ])
        .assert()
        .success();
    let body = std::fs::read_to_string(e.env_dir("target").join("aenv.toml")).unwrap();
    assert!(
        body.contains("name = \"local-foo\""),
        "plugin missing: {body}"
    );
    assert!(
        body.contains(&format!("source = \"file://{}\"", plugin_dir.display())),
        "expected installPath file:// fallback for non-github marketplace: {body}"
    );
    // Fanout dir is on disk.
    assert!(
        e.env_dir("target")
            .join(".claude/plugins/local-foo/README.md")
            .is_file(),
        "ifl should have applied the plugin via the installPath fallback"
    );
    // CRITICAL: native JSON registration under the synthetic
    // aenv-local marketplace. Without this, Claude Code's `cG()`
    // discovery wouldn't see the plugin even though the fanout
    // dir exists — that was the "import success, claude blind"
    // gap before the synthetic marketplace path landed.
    let ip: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            e.env_dir("target")
                .join(".claude/plugins/installed_plugins.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let key = "local-foo@aenv-local";
    assert!(
        ip["plugins"][key].is_array(),
        "local-source plugin must be in installed_plugins.json under aenv-local: {ip}"
    );
    let settings_body =
        std::fs::read_to_string(e.env_dir("target").join(".claude/settings.json")).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&settings_body).unwrap();
    assert_eq!(
        settings["enabledPlugins"][key], true,
        "enabledPlugins must include the local plugin: {settings_body}"
    );
    // Synthetic marketplace manifest at install_location.
    let mkt_manifest = e
        .env_dir("target")
        .join(".claude/plugins/marketplaces/aenv-local/.claude-plugin/marketplace.json");
    assert!(
        mkt_manifest.is_file(),
        "synthetic aenv-local marketplace.json missing — claude resolver will say 'Plugin not found in marketplace'"
    );
    let mkt_body = std::fs::read_to_string(&mkt_manifest).unwrap();
    assert!(
        mkt_body.contains("\"local-foo\""),
        "marketplace.json must list the plugin: {mkt_body}"
    );
}

#[test]
fn ifl_global_picks_up_nested_plugin_wrapped_skills() {
    // `aenv install` fanout writes plugin trees as
    // `<env>/.claude/plugins/<name@mkt>/<sha>/...`. Skills inside
    // those plugin trees live at
    // `<name@mkt>/<sha>/skills/<skill>/SKILL.md`. ifl's
    // ClaudePlugin scanner used to filter by fixed depth (== 1
    // for the `skills` component) and missed this layout
    // entirely. Pinned now via suffix matching.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "target"]).assert().success();

    let fake_home = e.home_path().join("user-home-nested-skill");
    let skill_dir =
        fake_home.join(".claude/plugins/some-plugin@some-mkt/abc123def/skills/deep-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# Deep\n").unwrap();

    e.aenv()
        .env("HOME", &fake_home)
        .args([
            "ifl",
            "-E",
            "target",
            "--from",
            "(global)",
            "--skill",
            "deep-skill",
        ])
        .assert()
        .success();
    let body = std::fs::read_to_string(e.env_dir("target").join("aenv.toml")).unwrap();
    assert!(
        body.contains("name = \"deep-skill\""),
        "nested skill missing: {body}"
    );
    assert!(
        body.contains(&format!("source = \"{}\"", skill_dir.display())),
        "skill source should point at the nested SKILL.md dir: {body}"
    );
}

#[test]
fn ifl_global_picks_up_top_level_claude_skills_dir() {
    // Claude Code 2.x writes user-level skills to
    // `~/.claude/skills/<name>/SKILL.md`, separate from the
    // plugin-wrapped layout under `~/.claude/plugins/**/skills/`.
    // ifl used to scan only the plugin-wrapped tree.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    e.aenv().args(["new", "target"]).assert().success();

    let fake_home = e.home_path().join("user-home-toplevel-skill");
    let skill_dir = fake_home.join(".claude/skills/lint-strict");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# Lint\n").unwrap();

    e.aenv()
        .env("HOME", &fake_home)
        .args([
            "ifl",
            "-E",
            "target",
            "--from",
            "(global)",
            "--skill",
            "lint-strict",
        ])
        .assert()
        .success();
    let body = std::fs::read_to_string(e.env_dir("target").join("aenv.toml")).unwrap();
    assert!(
        body.contains("name = \"lint-strict\""),
        "top-level skill missing: {body}"
    );
    assert!(
        body.contains(&format!("source = \"{}\"", skill_dir.display())),
        "skill source should point at ~/.claude/skills/<name>: {body}"
    );
}

#[test]
fn quit_subcommand_prints_shell_init_instructions_when_run_directly() {
    // `aenv quit` is meant to be intercepted by the shell function
    // (`aenv()` wrapper from `aenv shell-init`). When the shell function
    // isn't loaded, the binary itself runs — it must print clear
    // instructions instead of silently no-op'ing or clap-erroring.
    let e = Env::new();
    e.aenv().args(["init", "--no-default"]).assert().success();
    let out = e.aenv().args(["quit"]).output().unwrap();
    assert!(!out.status.success(), "should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("shell function") && stderr.contains("shell-init"),
        "expected shell-init guidance: {stderr}"
    );
}

#[test]
fn shell_init_zsh_quit_pins_global_via_override() {
    // The new quit/deactivate contract: this shell becomes env-less
    // visually AND behaviorally. We set AENV_OVERRIDE=global so
    // resolve step 1 wins, shadowing any cwd `.aenv-version` /
    // `aenv.toml` for this shell only — the disk pin is untouched.
    let e = Env::new();
    let out = e.aenv().args(["shell-init", "zsh"]).output().unwrap();
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.contains("quit|deactivate)"),
        "zsh init must alias quit→deactivate: {body}"
    );
    assert!(
        body.contains("export AENV_OVERRIDE=global"),
        "quit must pin AENV_OVERRIDE=global: {body}"
    );
    assert!(body.contains("unset AENV"), "quit must drop $AENV: {body}");
}

#[test]
fn shell_init_zsh_use_clears_override_so_quit_doesnt_shadow() {
    // After `aenv quit` exports AENV_OVERRIDE=global, a subsequent
    // `aenv use foo` must drop the override or resolve step 1 keeps
    // pinning global and the use silently no-ops.
    let e = Env::new();
    let out = e.aenv().args(["shell-init", "zsh"]).output().unwrap();
    let body = String::from_utf8_lossy(&out.stdout);
    let use_block = body
        .split("    use)")
        .nth(1)
        .and_then(|s| s.split("    quit|deactivate)").next())
        .unwrap_or("");
    assert!(
        use_block.contains("unset AENV_OVERRIDE"),
        "use must clear AENV_OVERRIDE before exporting AENV: {use_block}"
    );
}

#[test]
fn shell_init_prompt_silent_for_global_in_all_shells() {
    // venv-`deactivate` UX: when `global` is active (the alias for
    // the user's real ~/.claude), `_aenv_prompt` clears AENV_PROMPT.
    // The shell looks exactly like a non-aenv shell.
    let e = Env::new();
    for shell in ["zsh", "bash", "fish"] {
        let out = e.aenv().args(["shell-init", shell]).output().unwrap();
        let body = String::from_utf8_lossy(&out.stdout);
        assert!(
            body.contains("\"global\""),
            "{shell} init must compare against the literal \"global\": {body}"
        );
    }
}

#[test]
fn shell_init_zsh_use_wrapper_exports_aenv() {
    // `aenv use <name>` should both run the binary (writes the pin)
    // AND export $AENV in the current shell. Checked at emitted-script
    // level since real-shell behavior isn't reachable from cargo test.
    let e = Env::new();
    let out = e.aenv().args(["shell-init", "zsh"]).output().unwrap();
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.contains("    use)"),
        "zsh init must intercept `use`: {body}"
    );
    assert!(
        body.contains("export AENV=\"$_name\""),
        "use wrapper must export AENV: {body}"
    );
    // Order matters: run `command aenv` first (writes pin), THEN
    // export. Otherwise a binary error leaves $AENV stale.
    let use_block = body
        .split("    use)")
        .nth(1)
        .and_then(|s| s.split("    activate)").next())
        .unwrap_or("");
    let cmd_pos = use_block.find("command aenv").unwrap_or(usize::MAX);
    let export_pos = use_block.find("export AENV").unwrap_or(0);
    assert!(
        cmd_pos < export_pos,
        "must run binary BEFORE exporting: {use_block}"
    );
}

#[test]
fn shell_init_bash_use_wrapper_exports_aenv() {
    let e = Env::new();
    let out = e.aenv().args(["shell-init", "bash"]).output().unwrap();
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.contains("    use)") && body.contains("export AENV=\"$_name\""),
        "bash init must intercept `use` and export AENV: {body}"
    );
}

#[test]
fn shell_init_fish_use_wrapper_exports_aenv() {
    let e = Env::new();
    let out = e.aenv().args(["shell-init", "fish"]).output().unwrap();
    let body = String::from_utf8_lossy(&out.stdout);
    assert!(
        body.contains("case use") && body.contains("set -gx AENV"),
        "fish init must intercept `use` and set AENV: {body}"
    );
}
