use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use walkdir::WalkDir;

use crate::error::AenvError;
use crate::paths;

pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    paths::ensure_dir(dst)?;
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            paths::ensure_dir(&target)?;
        } else if ft.is_file() {
            if let Some(p) = target.parent() {
                paths::ensure_dir(p)?;
            }
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!("copy {} -> {}", entry.path().display(), target.display())
            })?;
        } else if ft.is_symlink() {
            if let Some(p) = target.parent() {
                paths::ensure_dir(p)?;
            }
            let _ = std::fs::remove_file(&target);
            #[cfg(unix)]
            {
                let link_target = std::fs::read_link(entry.path())?;
                std::os::unix::fs::symlink(link_target, &target)?;
            }
            #[cfg(windows)]
            {
                // Windows can't replicate symlinks without admin/dev-mode.
                // Resolve and copy as a regular file. Directory-symlinks
                // are skipped (would require admin junction creation and
                // are vanishingly rare in env trees).
                if let Ok(meta) = std::fs::metadata(entry.path()) {
                    if meta.is_file() {
                        std::fs::copy(entry.path(), &target)?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub mod manifest;

pub use manifest::Manifest;

/// Reserved env name that aliases the user's real `~/.claude` (and
/// `~/.codex` for codex). Always present in `Env::list()`, can never
/// be created / removed / mutated. Acts as the universal escape hatch:
/// `aenv quit` → resolve falls through to `global` → claude / codex
/// run against the user's pre-aenv config dir, exactly as if aenv
/// weren't installed. This guarantees no hook, broken manifest, or
/// missing secret can lock the user out of their tools.
pub const GLOBAL_NAME: &str = "global";

pub fn is_global(name: &str) -> bool {
    name == GLOBAL_NAME
}

/// In-memory handle to an env on disk.
#[derive(Debug, Clone)]
pub struct Env {
    /// Slot name = the on-disk directory under `~/.aenv/envs/<name>/`.
    /// For global envs this is just the user-given name (e.g. "default").
    /// For project envs this is `<label>-<sha8(project-path)>`, encoded
    /// so two clones of the same repo never collide on the same slot.
    pub name: String,
    pub root: PathBuf,
    /// If set, the manifest (and adjacent lockfile) live at this path
    /// instead of `<root>/aenv.toml`. Used for project-local mode where
    /// the manifest is committed to the project's git repo and the slot
    /// only holds materialized artifacts.
    pub manifest_override: Option<PathBuf>,
}

impl Env {
    pub fn open(name: &str) -> Result<Self> {
        if is_global(name) {
            return Self::global();
        }
        validate_name(name)?;
        let root = paths::env_dir(name)?;
        if !root.is_dir() {
            return Err(AenvError::EnvNotFound(name.into()).into());
        }
        Ok(Self {
            name: name.to_string(),
            root,
            manifest_override: None,
        })
    }

    /// Virtual handle for the reserved `global` env. `root` is the
    /// user's home dir so `claude_dir()` resolves to `~/.claude`
    /// (codex's `~/.codex` is handled in the codex backend's
    /// `codex_home_for` because `<root>/codex` is the wrong shape for
    /// the dot-prefixed real codex home).
    pub fn global() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve $HOME"))?;
        Ok(Self {
            name: GLOBAL_NAME.to_string(),
            root: home,
            manifest_override: None,
        })
    }

    /// Open an env in project-local mode: manifest authority lives at
    /// `manifest_path`, slot lives at `~/.aenv/envs/<slot>/` (computed
    /// from manifest's `[env].name` + sha8 of the project dir). If the
    /// slot doesn't exist yet, this lazily creates it (no manifest write
    /// — the project's manifest is the source of truth).
    pub fn open_at_project(slot: &str, manifest_path: &Path) -> Result<Self> {
        validate_name(slot)?;
        let root = paths::env_dir(slot)?;
        if !root.is_dir() {
            // Lazy create — first `aenv install` after `git clone`
            // materializes here. We don't write a manifest because the
            // project's `aenv.toml` is the source of truth.
            paths::ensure_dir(&root)?;
            paths::ensure_dir(&root.join(".claude"))?;
            for kind in [
                paths::XdgKind::Config,
                paths::XdgKind::Data,
                paths::XdgKind::State,
                paths::XdgKind::Cache,
            ] {
                paths::ensure_dir(&paths::env_xdg_dir(slot, kind)?)?;
            }
            paths::lock_down_dir(&root)?;
            paths::lock_down_dir(&root.join(".claude"))?;
            // Record the project source path for `aenv list` provenance.
            let _ = std::fs::write(
                root.join(".aenv-project-source"),
                manifest_path.to_string_lossy().as_bytes(),
            );
        }
        Ok(Self {
            name: slot.to_string(),
            root,
            manifest_override: Some(manifest_path.to_path_buf()),
        })
    }

    pub fn create(name: &str, bare: bool) -> Result<Self> {
        if is_global(name) {
            bail!(
                "'{GLOBAL_NAME}' is a reserved env name aliasing the user's real \
                 ~/.claude (and ~/.codex). It always exists; you can't create \
                 or remove it. Pick a different name."
            );
        }
        validate_name(name)?;
        let root = paths::env_dir(name)?;
        if root.exists() {
            return Err(AenvError::EnvAlreadyExists(name.into()).into());
        }
        paths::ensure_dir(&root)?;
        let claude = root.join(".claude");
        paths::ensure_dir(&claude)?;
        // Codex's CODEX_HOME for this env. Pre-creating the directory
        // means the codex shim can `cd` into a known-existing path
        // before exec without racing on first launch.
        let codex = root.join("codex");
        paths::ensure_dir(&codex)?;
        // Per-env XDG roots.
        for kind in [
            paths::XdgKind::Config,
            paths::XdgKind::Data,
            paths::XdgKind::State,
            paths::XdgKind::Cache,
        ] {
            paths::ensure_dir(&paths::env_xdg_dir(name, kind)?)?;
        }
        if !bare {
            let settings = claude.join("settings.json");
            std::fs::write(&settings, "{}\n")
                .with_context(|| format!("seed {}", settings.display()))?;
            paths::lock_down_file(&settings)?;
        }
        Manifest::default_for(name).save(&root)?;
        // Tighten the env root + per-backend dirs (settings.json,
        // plugins, codex auth/sessions) so other users on the system
        // can't read tokens-derived state.
        paths::lock_down_dir(&root)?;
        paths::lock_down_dir(&claude)?;
        paths::lock_down_dir(&codex)?;
        Ok(Self {
            name: name.to_string(),
            root,
            manifest_override: None,
        })
    }

    pub fn clone_from(name: &str, src: &Env) -> Result<Self> {
        if is_global(name) {
            bail!("'{GLOBAL_NAME}' is reserved; can't be cloned to.");
        }
        validate_name(name)?;
        let dst_root = paths::env_dir(name)?;
        if dst_root.exists() {
            return Err(AenvError::EnvAlreadyExists(name.into()).into());
        }
        paths::ensure_dir(dst_root.parent().unwrap_or(Path::new("/")))?;
        // Wrap the rest in a closure so we can rm the partial dir on any
        // failure — clone is meant to be all-or-nothing like Env::create.
        let result: Result<()> = (|| {
            copy_tree(&src.root, &dst_root)?;
            let mut m = Manifest::load(&dst_root).unwrap_or_else(|_| Manifest::default_for(name));
            m.env.name = name.to_string();
            m.env.created = Some(chrono::Utc::now());
            m.save(&dst_root)?;
            // copy_tree replicates source mode bits, dropping the 0700
            // invariant Env::create enforces. Reapply.
            paths::lock_down_dir(&dst_root)?;
            let claude = dst_root.join(".claude");
            if claude.is_dir() {
                paths::lock_down_dir(&claude)?;
                let settings = claude.join("settings.json");
                if settings.is_file() {
                    paths::lock_down_file(&settings)?;
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            // Best-effort cleanup so a half-cloned env doesn't haunt
            // future commands and `aenv list`.
            let _ = std::fs::remove_dir_all(&dst_root);
            return Err(e);
        }
        Ok(Self {
            name: name.to_string(),
            root: dst_root,
            manifest_override: None,
        })
    }

    pub fn remove(name: &str) -> Result<()> {
        if is_global(name) {
            bail!(
                "'{GLOBAL_NAME}' is reserved; refusing to remove. The user's real \
                 ~/.claude is not aenv-managed."
            );
        }
        validate_name(name)?;
        let root = paths::env_dir(name)?;
        if !root.exists() {
            return Err(AenvError::EnvNotFound(name.into()).into());
        }
        std::fs::remove_dir_all(&root).with_context(|| format!("remove {}", root.display()))?;
        Ok(())
    }

    pub fn list() -> Result<Vec<EnvSummary>> {
        // `global` is always present — it's the alias for the user's
        // real ~/.claude (and ~/.codex), the universal escape hatch.
        // Listed first so it shows above on-disk envs in `aenv list`.
        let mut out: Vec<EnvSummary> = Vec::new();
        if let Some(home) = dirs::home_dir() {
            out.push(EnvSummary {
                name: GLOBAL_NAME.to_string(),
                root: home,
                description: Some("alias for the user's real ~/.claude / ~/.codex".to_string()),
                broken: false,
            });
        }

        let dir = paths::envs_dir()?;
        if !dir.is_dir() {
            return Ok(out);
        }
        let mut on_disk: Vec<EnvSummary> = Vec::new();
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let root = entry.path();
            let manifest = Manifest::load(&root);
            let (description, broken) = match manifest {
                Ok(m) => (m.env.description, false),
                Err(_) => (None, true),
            };
            on_disk.push(EnvSummary {
                name,
                root,
                description,
                broken,
            });
        }
        on_disk.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(on_disk);
        Ok(out)
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.root.join(".claude")
    }

    pub fn xdg(&self, kind: paths::XdgKind) -> PathBuf {
        self.root.join("xdg").join(kind.as_str())
    }

    pub fn manifest(&self) -> Result<Manifest> {
        match &self.manifest_override {
            Some(path) => Manifest::load_from(path),
            None => Manifest::load(&self.root),
        }
    }

    /// Path where mutating commands should write the manifest. Project
    /// mode → the project's `aenv.toml`; global mode → `<root>/aenv.toml`.
    pub fn manifest_write_path(&self) -> PathBuf {
        match &self.manifest_override {
            Some(p) => p.clone(),
            None => Manifest::manifest_path(&self.root),
        }
    }

    /// Path where lockfile reads/writes happen. In project mode the lock
    /// is committed alongside the manifest; in global mode it lives in
    /// the slot.
    pub fn lockfile_path(&self) -> PathBuf {
        match &self.manifest_override {
            Some(p) => p.with_file_name("aenv.lock"),
            None => Manifest::lockfile_path(&self.root),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvSummary {
    pub name: String,
    pub root: PathBuf,
    pub description: Option<String>,
    /// True if the env directory exists but its manifest is missing or
    /// invalid. Surfaced by `aenv list` and `aenv doctor` so users can
    /// notice partially-cleaned-up state without it silently lingering.
    pub broken: bool,
}

pub fn validate_name(name: &str) -> Result<()> {
    validate_resource_name("env", name).map_err(|_| AenvError::InvalidEnvName(name.into()).into())
}

/// Validate that `name` is safe to use as a single path component for the
/// given `kind` of resource (env / plugin / skill / mcp). Same rules as env
/// names: ASCII alphanumeric + `._-`, no leading dot, non-empty. This rules
/// out `..`, `/`, `\`, NUL, whitespace, and any other path-traversal vector.
pub fn validate_resource_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{kind} name cannot be empty");
    }
    if name.starts_with('.') {
        bail!("{kind} name '{name}' must not start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("{kind} name '{name}' must match [a-zA-Z0-9._-]+");
    }
    Ok(())
}

/// Validate a source subpath such as `plugins/code-review`.
///
/// The path is later joined under a fetched source root, so only normal
/// relative components are allowed. This rejects absolute paths, `..`,
/// empty components, Windows prefixes, and odd component types.
pub fn validate_relative_subpath(kind: &str, path: &str) -> Result<()> {
    if path.trim().is_empty() {
        bail!("{kind} subpath cannot be empty");
    }
    let p = Path::new(path);
    if p.is_absolute() {
        bail!("{kind} subpath '{path}' must be relative");
    }
    for component in p.components() {
        match component {
            std::path::Component::Normal(part) => {
                let s = part.to_string_lossy();
                if s.is_empty() {
                    bail!("{kind} subpath '{path}' contains an empty component");
                }
            }
            _ => bail!("{kind} subpath '{path}' must not contain '.' or '..'"),
        }
    }
    Ok(())
}

/// Helper for commands that need to load an env by an optional explicit name,
/// falling back to the resolved active env.
///
/// In project-local mode (cwd has `aenv.toml`), the returned Env's
/// `manifest_override` is set to the project manifest path so reads/writes
/// land on the committed file rather than the slot's copy.
pub fn open_or_active(name: Option<&str>) -> Result<Env> {
    if let Some(n) = name {
        return Env::open(n);
    }
    let cwd = std::env::current_dir()?;
    let resolved =
        crate::resolve::resolve(&cwd)?.ok_or_else(|| anyhow!("no env active and no name given"))?;
    open_resolved(&resolved)
}

/// Open the Env corresponding to a resolved entry, picking the right
/// constructor based on whether the resolution found a project manifest.
pub fn open_resolved(r: &crate::resolve::Resolved) -> Result<Env> {
    match &r.project_manifest {
        Some(manifest_path) => Env::open_at_project(&r.slot, manifest_path),
        None => Env::open(&r.slot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn validate_name_accepts_normal() {
        for n in ["a", "abc", "abc-1", "a.b", "a_b", "Foo123"] {
            validate_name(n).unwrap_or_else(|_| panic!("rejected: {n}"));
        }
    }

    #[test]
    fn validate_name_rejects_bad() {
        for n in ["", ".hidden", "a/b", "a b", "a:b", "a$b"] {
            assert!(validate_name(n).is_err(), "should reject: {n}");
        }
    }

    #[test]
    fn copy_tree_replicates_files_and_subdirs() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), "world").unwrap();
        copy_tree(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(),
            "world"
        );
    }

    // File-symlinks need admin/dev-mode on Windows (and the API
    // `std::os::unix::fs::symlink` is Unix-only). The runtime
    // `copy_tree` already has a Windows branch that copies through
    // resolved targets — see env/mod.rs.
    #[cfg(unix)]
    #[test]
    fn copy_tree_preserves_symlinks() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("real"), "x").unwrap();
        std::os::unix::fs::symlink("real", src.join("link")).unwrap();
        copy_tree(&src, &dst).unwrap();
        let link_target = std::fs::read_link(dst.join("link")).unwrap();
        assert_eq!(link_target, std::path::PathBuf::from("real"));
    }
}
