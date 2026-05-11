use anyhow::{anyhow, bail, Context, Result};

use crate::backend::claude::shim;
use crate::cli::InitArgs;
use crate::env::{Env, Manifest};
use crate::paths;
use crate::resolve::GlobalConfig;

pub fn run(args: InitArgs) -> Result<u8> {
    if let Some(label_arg) = &args.here {
        return run_here(label_arg);
    }

    // Serialize against any other mutating aenv command. Without the lock,
    // a concurrent `aenv install` (or another `aenv init` from `aenv
    // upgrade`) can race with our shim relink + `default` env seeding and
    // leave partial state. Every other mutating entry point already takes
    // this lock — init was the lone holdout.
    let _lock = crate::tx::GlobalLock::acquire()?;

    paths::ensure_dir(&paths::aenv_home()?)?;
    paths::ensure_dir(&paths::envs_dir()?)?;
    paths::ensure_dir(&paths::state_dir()?)?;
    paths::ensure_dir(&paths::store_dir()?)?;
    let shims = paths::shims_dir()?;
    paths::ensure_dir(&shims)?;
    // Tighten home + state perms (owner-only). Best-effort; non-fatal.
    paths::lock_down_dir(&paths::aenv_home()?)?;
    paths::lock_down_dir(&paths::state_dir()?)?;

    // Find the real claude before installing the shim, so we don't accidentally
    // make it find itself later.
    let real = shim::locate_real_claude().context(
        "could not find real claude in PATH. install Claude Code first, then run `aenv init`",
    )?;

    // Surface a corrupt config.toml (truncated, hand-edited) instead of
    // silently rewriting it with only real_claude — that would clobber
    // the user's default_env and any other persisted state.
    let mut cfg = GlobalConfig::load()?;
    cfg.real_claude = Some(real.clone());
    cfg.save()?;

    install_shim(&shims, args.force)?;
    install_codex_shim_if_present(&shims, args.force);

    if !args.no_default && Env::open("default").is_err() {
        {
            // If the user has an existing global ~/.claude with content,
            // seed `default` from it so plugins/skills/MCPs persist into
            // the isolated env. Otherwise create an empty default.
            let home = dirs::home_dir();
            let has_global = home
                .as_ref()
                .map(|h| h.join(".claude").is_dir())
                .unwrap_or(false);
            if has_global {
                let env = Env::create("default", true)?;
                let src = home.unwrap().join(".claude");
                let dst = env.claude_dir();
                if dst.exists() {
                    std::fs::remove_dir_all(&dst).ok();
                }
                crate::env::copy_tree(&src, &dst)?;
                // copy_tree replicates source mode bits — at the typical
                // 022 umask, .claude and settings.json end up
                // group/world-readable, contradicting the owner-only
                // invariant Env::create just established. Re-lock down.
                paths::lock_down_dir(&dst)?;
                let settings = dst.join("settings.json");
                if settings.is_file() {
                    paths::lock_down_file(&settings)?;
                }
                println!("aenv: seeded 'default' from existing {}", src.display());
            } else {
                Env::create("default", false)?;
            }
        }
    }

    // No global slash-command provisioning. Pre-pivot, `/aenv:use` and
    // `/aenv:reload` lived under `~/.claude/commands/aenv/` and worked
    // through the supervisor restart loop (exit code 75). The new
    // shim model is single-exec — there's no supervisor watching for a
    // restart marker — so in-session reload would silently no-op. The
    // cleaner UX is `exit` + relaunch, mirroring codex. If you need to
    // switch envs from inside an active session, run `aenv use <name>`
    // in a side terminal, then exit and relaunch claude.

    println!("aenv: home  = {}", paths::aenv_home()?.display());
    println!(
        "aenv: shim  = {}",
        shims.join(shim::claude_binary_name()).display()
    );
    println!("aenv: real  = {}", real.display());
    println!();

    if !args.no_guidance {
        let shell = detect_shell();
        let (rc_path, shell_name) = rc_for_shell(&shell);

        println!("Next steps:");
        println!();
        println!("  1. Wire your shell (one command):");
        println!("       echo 'eval \"$(aenv shell-init {shell_name})\"' >> {rc_path}");
        println!();
        println!("  2. Reload — pick one:");
        println!("       source {rc_path}      # current shell");
        println!("       exec {shell_name}                   # restart current shell");
        println!("       (or just open a new terminal)");
        println!();
        println!(
            "  (other shells: aenv shell-init bash | aenv shell-init zsh | aenv shell-init fish)"
        );
        println!();
    }
    Ok(0)
}

/// `aenv init --here [<name>]` — bootstrap a project-local manifest.
/// Writes `./aenv.toml` (with sensible empty defaults) plus an empty
/// `./aenv.lock`. Refuses to overwrite an existing manifest. The
/// name defaults to the basename of cwd.
fn run_here(label_arg: &str) -> Result<u8> {
    let cwd = std::env::current_dir().context("getcwd")?;
    let manifest_path = cwd.join("aenv.toml");
    let lock_path = cwd.join("aenv.lock");
    if manifest_path.exists() {
        bail!(
            "{} already exists; refusing to overwrite",
            manifest_path.display()
        );
    }

    let label = if label_arg.is_empty() {
        cwd.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                anyhow!("could not derive env name from cwd; pass `aenv init --here <name>`")
            })?
            .to_string()
    } else {
        label_arg.to_string()
    };
    crate::env::validate_resource_name("env", &label)?;

    Manifest::default_for(&label).save_to(&manifest_path)?;
    // Empty lockfile, schema-correct (so `aenv install` doesn't refuse).
    let lock = crate::env::manifest::Lockfile::default();
    lock.save_to(&lock_path)?;

    // Protect aenv.lock + aenv.toml from line-ending normalization in
    // git checkouts on Windows (`core.autocrlf=true`). Without this, a
    // CRLF rewrite on checkout silently changes the bytes the lockfile's
    // sha256 verification reads and aenv install --frozen fails on the
    // other OS. Pattern matches Go's repo-wide policy:
    //   "Treat all files in the Go repo as binary, with no git magic
    //    updating line endings. This produces predictable results in
    //    different environments."
    //   — https://github.com/golang/go/blob/master/.gitattributes
    let attrs_path = cwd.join(".gitattributes");
    let aenv_attrs_block = "\
# aenv: never normalize line endings on the manifest/lockfile —
# byte-identical content across macOS / Linux / Windows is required
# for sha256 verification to round-trip.
aenv.toml   -text
aenv.lock   -text
";
    if attrs_path.exists() {
        let body = std::fs::read_to_string(&attrs_path).unwrap_or_default();
        if !body.contains("aenv.lock") {
            let mut updated = body;
            if !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push('\n');
            updated.push_str(aenv_attrs_block);
            paths::write_atomic(&attrs_path, updated.as_bytes())?;
            eprintln!("aenv: appended aenv rules to {}", attrs_path.display());
        }
    } else {
        paths::write_atomic(&attrs_path, aenv_attrs_block.as_bytes())?;
        eprintln!("aenv: wrote {}", attrs_path.display());
    }

    eprintln!("aenv: wrote {}", manifest_path.display());
    eprintln!("aenv: wrote {}", lock_path.display());
    eprintln!("aenv: project-local mode active. Slot will be:");
    eprintln!(
        "  {}",
        paths::env_dir(&paths::project_slot_name(&label, &cwd))?.display()
    );
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  1. `aenv add plugin <name> --source ...` (each `add` applies");
    eprintln!("     immediately — no separate `aenv install` step needed)");
    eprintln!("  2. `git add aenv.toml aenv.lock && git commit` to share");
    Ok(0)
}

fn detect_shell() -> String {
    // `$SHELL` first — set on every Unix login shell and on Git Bash.
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() {
            return s;
        }
    }
    // `$MSYSTEM` is Git Bash / MSYS2's identity marker (`MINGW64`,
    // `MSYS`, `UCRT64`, etc.). Treat any non-empty value as bash.
    if std::env::var_os("MSYSTEM").is_some() {
        return "/usr/bin/bash".to_string();
    }
    String::new()
}

fn rc_for_shell(shell_path: &str) -> (String, &'static str) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
    // Split on both `/` and `\` so Windows-style paths like
    // `C:\Program Files\Git\usr\bin\bash.exe` resolve to `bash.exe`.
    // Strip the `.exe` suffix so the match arm stays simple.
    let basename = shell_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .strip_suffix(".exe")
        .unwrap_or_else(|| shell_path.rsplit(['/', '\\']).next().unwrap_or(""));
    match basename {
        "zsh" => ("~/.zshrc".to_string(), "zsh"),
        "bash" | "sh" => {
            let bashrc = format!("{home}/.bashrc");
            let bash_profile = format!("{home}/.bash_profile");
            if std::path::Path::new(&bash_profile).exists()
                && !std::path::Path::new(&bashrc).exists()
            {
                ("~/.bash_profile".to_string(), "bash")
            } else {
                ("~/.bashrc".to_string(), "bash")
            }
        }
        "fish" => ("~/.config/fish/config.fish".to_string(), "fish"),
        // Unknown shell: pick a sane default for the host. Prior
        // behavior was always "zsh" which surprised every Linux/
        // Git-Bash user. Now Windows defaults to bash, others to
        // zsh (matches macOS default and most Linux distros today).
        _ => {
            if cfg!(windows) {
                ("~/.bashrc".to_string(), "bash")
            } else {
                ("~/.zshrc".to_string(), "zsh")
            }
        }
    }
}

/// Provision `<shims>/codex` only if codex is actually installed on PATH.
/// We don't require codex (most users have just claude) — but if it's
/// there, the codex shim should land alongside the claude shim so
/// `codex` invocations route through the universal env switcher too.
/// Errors are non-fatal: a missing codex isn't an aenv-init failure.
fn install_codex_shim_if_present(shims_dir: &std::path::Path, force: bool) {
    if crate::backend::codex::locate_real_codex().is_err() {
        return;
    }
    let Ok(target) = std::env::current_exe() else {
        return;
    };
    let link = shims_dir.join(crate::backend::codex::codex_binary_name());
    if let Err(e) = link_shim(&target, &link, force) {
        eprintln!(
            "aenv: warning: could not install codex shim at {}: {e}",
            link.display()
        );
    }
}

fn install_shim(shims_dir: &std::path::Path, force: bool) -> Result<()> {
    let target = std::env::current_exe().context("current_exe")?;
    let link = shims_dir.join(crate::backend::claude::shim::claude_binary_name());
    link_shim(&target, &link, force)
}

fn link_shim(target: &std::path::Path, link: &std::path::Path, force: bool) -> Result<()> {
    #[cfg(unix)]
    {
        if link.exists() || link.is_symlink() {
            if !force {
                // Symlink target match → already correct, no-op.
                if let Ok(t) = std::fs::read_link(link) {
                    if t == target {
                        return Ok(());
                    }
                }
            }
            std::fs::remove_file(link).ok();
        }
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("symlink {} -> {}", link.display(), target.display()))?;
    }
    #[cfg(windows)]
    {
        // Windows can't symlink without admin/dev-mode. Copy the binary
        // and document `aenv init --force` after every binary upgrade.
        if link.exists() && !force {
            // Compare modification times — if shim is older than the
            // installed binary, refresh anyway. Otherwise no-op.
            let target_mtime = std::fs::metadata(target).and_then(|m| m.modified()).ok();
            let link_mtime = std::fs::metadata(link).and_then(|m| m.modified()).ok();
            if matches!((target_mtime, link_mtime), (Some(t), Some(l)) if l >= t) {
                return Ok(());
            }
        }
        if link.exists() {
            std::fs::remove_file(link).ok();
        }
        std::fs::copy(target, link)
            .with_context(|| format!("copy {} -> {}", target.display(), link.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::rc_for_shell;

    #[test]
    fn rc_for_unix_shells_resolves_basename() {
        assert_eq!(rc_for_shell("/bin/zsh").1, "zsh");
        assert_eq!(rc_for_shell("/usr/bin/bash").1, "bash");
        assert_eq!(rc_for_shell("/usr/local/bin/fish").1, "fish");
    }

    #[test]
    fn rc_for_git_bash_windows_path() {
        // `$SHELL` on Git Bash can be a Windows path.
        assert_eq!(
            rc_for_shell(r"C:\Program Files\Git\usr\bin\bash.exe").1,
            "bash"
        );
        assert_eq!(rc_for_shell(r"C:\Tools\sh.exe").1, "bash");
    }

    #[test]
    fn rc_for_unknown_shell_falls_back_per_os() {
        // The point: previously Linux/Git-Bash users got "zsh" guidance.
        // Now non-Windows hosts still default to zsh (macOS-friendly),
        // Windows defaults to bash.
        let (_, shell) = rc_for_shell("");
        if cfg!(windows) {
            assert_eq!(shell, "bash");
        } else {
            assert_eq!(shell, "zsh");
        }
    }
}
