use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use once_cell::sync::OnceCell;

/// Root for all aenv state.
/// Resolution: $AENV_HOME > $XDG_DATA_HOME/aenv (if explicitly set) > $HOME/.aenv.
/// We deliberately avoid macOS's `~/Library/Application Support` (has a space,
/// breaks fragile shell scripts and doesn't match the rustup/nvm/uv convention).
pub fn aenv_home() -> Result<PathBuf> {
    static CELL: OnceCell<PathBuf> = OnceCell::new();
    CELL.get_or_try_init(|| {
        if let Ok(p) = std::env::var("AENV_HOME") {
            if !p.is_empty() {
                return Ok(PathBuf::from(p));
            }
        }
        if let Ok(p) = std::env::var("XDG_DATA_HOME") {
            if !p.is_empty() {
                return Ok(PathBuf::from(p).join("aenv"));
            }
        }
        let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve $HOME"))?;
        Ok(home.join(".aenv"))
    })
    .cloned()
}

pub fn envs_dir() -> Result<PathBuf> {
    Ok(aenv_home()?.join("envs"))
}

pub fn env_dir(name: &str) -> Result<PathBuf> {
    Ok(envs_dir()?.join(name))
}

/// Per-env XDG roots so plugins/MCPs that respect XDG also stay isolated.
pub fn env_xdg_dir(name: &str, kind: XdgKind) -> Result<PathBuf> {
    Ok(env_dir(name)?.join("xdg").join(kind.as_str()))
}

#[derive(Clone, Copy, Debug)]
pub enum XdgKind {
    Config,
    Data,
    State,
    Cache,
}

impl XdgKind {
    pub fn as_str(self) -> &'static str {
        match self {
            XdgKind::Config => "config",
            XdgKind::Data => "data",
            XdgKind::State => "state",
            XdgKind::Cache => "cache",
        }
    }
}

pub fn store_dir() -> Result<PathBuf> {
    Ok(aenv_home()?.join("store"))
}

pub fn shims_dir() -> Result<PathBuf> {
    Ok(aenv_home()?.join("shims"))
}

pub fn state_dir() -> Result<PathBuf> {
    Ok(aenv_home()?.join("state"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(aenv_home()?.join("config.toml"))
}

/// Walk up from `start` looking for `.aenv-version`.
pub fn find_version_file(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join(".aenv-version");
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// Walk up from `start` looking for `aenv.toml` (project-local manifest,
/// committed to git). Takes precedence over `.aenv-version` in the
/// resolver — its presence opts the project into "project-local manifest
/// mode" (analogue of `pyproject.toml` for poetry/uv, `Cargo.toml` for
/// rust). Returns the absolute path to the manifest file.
pub fn find_project_manifest(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join("aenv.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// Compute the on-disk slot suffix for a project at `project_dir`. Used
/// to disambiguate two clones of the same repo that share the manifest
/// `[env].name` — the disk slot becomes `<name>-<sha8(canonical_path)>`,
/// matching pipenv's `~/.local/share/virtualenvs/<dirname>-<hash>` pattern.
/// 8 hex chars (32 bits) is enough — collisions across the same user's
/// home directory require ~64k clones, far beyond realistic.
pub fn project_path_hash(project_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Compute the env slot name for project mode: `<label>-<sha8(path)>`.
/// `label` is the manifest's `[env].name` (display) and `project_dir` is
/// the directory containing the manifest.
pub fn project_slot_name(label: &str, project_dir: &Path) -> String {
    format!("{label}-{}", project_path_hash(project_dir))
}

pub fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("create_dir_all {}", p.display()))
}

/// Atomic-replace write: writes `body` to a sibling temp file, fsyncs, then
/// `rename`s into place. POSIX rename is atomic on the same filesystem so
/// readers see either the old or new file, never a half-written one. Use for
/// every mutating write of critical state (manifest, lockfile, settings.json,
/// secrets.list, config.toml).
pub fn write_atomic(dst: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = dst
        .parent()
        .ok_or_else(|| anyhow!("write_atomic target has no parent: {}", dst.display()))?;
    ensure_dir(parent)?;
    // tmp filename derived from pid + nanosec to avoid collision with any
    // concurrent writer. Stays in the same dir so rename is same-FS.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        dst.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "_".into()),
        std::process::id(),
        nanos
    ));
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create temp {}", tmp.display()))?;
        f.write_all(body)
            .with_context(|| format!("write temp {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync temp {}", tmp.display()))?;
    }
    // If rename fails, leave the .tmp around for forensics — let user clean it.
    std::fs::rename(&tmp, dst).map_err(|e| {
        anyhow!(
            "atomic rename {} -> {} failed: {e}",
            tmp.display(),
            dst.display()
        )
    })?;
    Ok(())
}

/// Best-effort tighten directory perms to 0700 (owner-only). Used on `aenv
/// home` and per-env directories so plaintext-adjacent state (settings.json,
/// audit log, lockfiles) isn't world-readable on shared systems. Failures are
/// logged but non-fatal — some filesystems (network mounts, FAT) ignore mode.
///
/// On Windows, this is a no-op: the per-user home (`%USERPROFILE%\.aenv`)
/// is owner-only by default ACLs, and applying an explicit ACL would
/// require a much larger dependency. `aenv doctor` documents this.
#[cfg(unix)]
pub fn lock_down_dir(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !p.is_dir() {
        return Ok(());
    }
    let meta = std::fs::metadata(p).with_context(|| format!("stat {}", p.display()))?;
    let mut perms = meta.permissions();
    perms.set_mode(0o700);
    if let Err(e) = std::fs::set_permissions(p, perms) {
        tracing::debug!("could not chmod 0700 {}: {e}", p.display());
    }
    Ok(())
}

#[cfg(windows)]
pub fn lock_down_dir(_p: &Path) -> Result<()> {
    Ok(())
}

/// Best-effort tighten file perms to 0600. See `lock_down_dir`.
#[cfg(unix)]
pub fn lock_down_file(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !p.is_file() {
        return Ok(());
    }
    let meta = std::fs::metadata(p).with_context(|| format!("stat {}", p.display()))?;
    let mut perms = meta.permissions();
    perms.set_mode(0o600);
    if let Err(e) = std::fs::set_permissions(p, perms) {
        tracing::debug!("could not chmod 0600 {}: {e}", p.display());
    }
    Ok(())
}

#[cfg(windows)]
pub fn lock_down_file(_p: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn find_version_file_walks_up() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let a = root.join("a");
        let b = a.join("b");
        let c = b.join("c");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(a.join(".aenv-version"), "myenv\n").unwrap();
        let found = find_version_file(&c).expect("walk-up failed");
        assert_eq!(found, a.join(".aenv-version"));
    }

    #[test]
    fn find_version_file_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        assert!(find_version_file(tmp.path()).is_none());
    }

    #[test]
    fn ensure_dir_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a/b/c");
        ensure_dir(&p).unwrap();
        ensure_dir(&p).unwrap();
        assert!(p.is_dir());
    }

    #[test]
    fn xdg_kind_str_stable() {
        assert_eq!(XdgKind::Config.as_str(), "config");
        assert_eq!(XdgKind::Data.as_str(), "data");
        assert_eq!(XdgKind::State.as_str(), "state");
        assert_eq!(XdgKind::Cache.as_str(), "cache");
    }
}
