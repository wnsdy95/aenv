//! Shared test harness. Spins up an isolated AENV_HOME + a fake claude binary
//! so integration tests don't touch the user's real ~/.aenv or ~/.claude.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

pub struct Env {
    pub home: TempDir,
    pub fakebin: PathBuf,
}

impl Env {
    pub fn new() -> Self {
        let home = TempDir::new().expect("tempdir");
        let fakebin = home.path().join("fakebin");
        std::fs::create_dir_all(&fakebin).unwrap();
        let claude = fakebin.join("claude");
        std::fs::write(
            &claude,
            r#"#!/bin/sh
echo "fake-claude args: $*" >&2
echo "CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR" >&2
exit 0
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&claude).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&claude, perms).unwrap();
        }
        Self { home, fakebin }
    }

    pub fn aenv(&self) -> Command {
        let mut c = Command::cargo_bin("aenv").expect("aenv bin");
        c.env("AENV_HOME", self.home.path());
        // Override HOME so anything that reads `~/.claude` (the global
        // source in `aenv ifl`, `aenv migrate-auth`, etc.) sees an
        // empty dir instead of the dev machine's real config. Tests
        // that need a populated `~/.claude` plant fixtures under this
        // HOME and pass `.env("HOME", ...)` per-call to override.
        c.env("HOME", self.home.path());
        // Prepend fakebin so `aenv init` discovers our fake claude.
        let path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", self.fakebin.display(), path);
        c.env("PATH", new_path);
        // Strip every aenv/claude breadcrumb the parent shell may carry —
        // running `cargo test` from inside a supervised aenv session
        // (AENV/AENV_SUPERVISED set by our own shim) used to leak the
        // dev's active env into tests, breaking precedence assertions
        // and the "supervised reload refuses outside session" check.
        for var in scrub_vars() {
            c.env_remove(var);
        }
        // Suppress the "[envname]" banner so stderr-based assertions only
        // see real diagnostic output. Tests that want to verify the banner
        // can override this on the per-call command.
        c.env("AENV_QUIET", "1");
        c
    }

    /// Variables every spawned aenv (or shim, or fake-claude) command
    /// must shed before it starts. Centralised so the direct-shim
    /// invocations in `tests/cli.rs` (which can't go through `aenv()`)
    /// can call `for v in common::scrub_vars() { cmd.env_remove(v); }`.
    pub fn scrub_vars_static() -> &'static [&'static str] {
        scrub_vars()
    }

    pub fn home_path(&self) -> &std::path::Path {
        self.home.path()
    }

    pub fn env_dir(&self, name: &str) -> PathBuf {
        self.home.path().join("envs").join(name)
    }

    /// Path the recording fake-claude writes its argv to.
    pub fn argv_capture_path(&self) -> PathBuf {
        self.home.path().join("fake-claude.argv")
    }

    /// Replace the default fake claude with a recording one that dumps
    /// every CLI argument (one per line) and the relevant CLAUDE_*
    /// env vars to the capture file. Also exits 0 immediately so the
    /// supervisor doesn't loop waiting for input.
    pub fn install_recording_claude(&self) {
        let claude = self.fakebin.join("claude");
        let cap = self.argv_capture_path();
        let body = format!(
            r#"#!/bin/sh
out="{}"
: > "$out"
for a in "$@"; do
    printf 'arg=%s\n' "$a" >> "$out"
done
printf 'env:CLAUDE_CONFIG_DIR=%s\n' "${{CLAUDE_CONFIG_DIR-<unset>}}" >> "$out"
printf 'env:XDG_CONFIG_HOME=%s\n' "${{XDG_CONFIG_HOME-<unset>}}" >> "$out"
printf 'env:AENV=%s\n' "${{AENV-<unset>}}" >> "$out"
exit 0
"#,
            cap.display()
        );
        std::fs::write(&claude, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&claude).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&claude, perms).unwrap();
        }
    }

    pub fn read_capture(&self) -> String {
        std::fs::read_to_string(self.argv_capture_path()).unwrap_or_default()
    }
}

/// Env vars that must not bleed from the host shell into a test process.
/// Covers (a) the AENV_* breadcrumbs the supervisor / shim / shell-init
/// export, (b) the resolver's two override slots, (c) the inherited
/// CLAUDE_CONFIG_DIR / CLAUDE_SESSION_ID that overlay-mode strips at
/// runtime but tests need to see absent at process start.
fn scrub_vars() -> &'static [&'static str] {
    &[
        "AENV",
        "AENV_OVERRIDE",
        "AENV_ACTIVE",
        "AENV_SUPERVISED",
        "AENV_LAST_SESSION",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_SESSION_ID",
    ]
}

/// Convenience: a local plugin source matching the Claude Code spec
/// (`<root>/.claude-plugin/plugin.json` is the entry-point manifest).
pub fn make_local_plugin(parent: &std::path::Path, name: &str, content: &str) -> PathBuf {
    let dir = parent.join(name);
    let cp = dir.join(".claude-plugin");
    std::fs::create_dir_all(&cp).unwrap();
    std::fs::write(
        cp.join("plugin.json"),
        format!("{{\"name\":\"{name}\",\"version\":\"0.1.0\"}}"),
    )
    .unwrap();
    std::fs::write(dir.join("README.md"), content).unwrap();
    dir
}

pub fn make_local_skill(parent: &std::path::Path, name: &str, content: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
    dir
}
