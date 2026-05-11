//! Resolve a `source` string into a local directory we can hash + insert into
//! the store. Supported sources:
//! - `file:///abs/path` or `/abs/path` or `./relative` — local directory
//! - `https://...tar.gz` / `.tgz` / `.tar` — remote tarball
//! - `git+https://...@<ref>` — git clone (shallow)
//! - `npm:<package>@<version>` — npm tarball via registry (best-effort)
//!
//! All sources land in a temporary dir; caller hashes that and inserts.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use tar::Archive;
use tempfile::TempDir;

pub struct Fetched {
    /// The directory containing the unpacked plugin/skill (caller takes ownership).
    pub dir: PathBuf,
    /// Holds the temp dir alive for the lifetime of `dir`.
    pub _guard: Option<TempDir>,
}

pub fn fetch(source: &str) -> Result<Fetched> {
    if source.starts_with("git+") {
        return fetch_git(source);
    }
    if source.starts_with("npm:") {
        return fetch_npm(source);
    }
    if source.starts_with("https://") {
        return fetch_http(source);
    }
    if source.starts_with("http://") {
        // Plaintext HTTP for executable plugin/skill content lets a MITM
        // poison the lockfile on first install — the malicious tarball gets
        // hashed and committed, and every subsequent install verifies
        // against that poisoned hash. Refuse outright; document the
        // file:// or git+https workaround for true offline/dev cases.
        bail!(
            "refusing plaintext http source '{source}'. Use https://, git+https://, \
             or download to disk and use file:///path/to/archive.tgz."
        );
    }
    if let Some(rest) = source.strip_prefix("file://") {
        return fetch_local(Path::new(rest));
    }
    fetch_local(Path::new(source))
}

fn fetch_local(p: &Path) -> Result<Fetched> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    if !abs.exists() {
        bail!("local source not found: {}", abs.display());
    }
    if abs.is_file() {
        // Treat as tarball.
        let tmp = TempDir::new()?;
        unpack_tarball(&abs, tmp.path())?;
        let dir = locate_unpacked_root(tmp.path())?;
        return Ok(Fetched {
            dir,
            _guard: Some(tmp),
        });
    }
    Ok(Fetched {
        dir: abs,
        _guard: None,
    })
}

fn fetch_http(url: &str) -> Result<Fetched> {
    // Defense in depth: every public caller checks scheme already, but
    // refuse here too so a future internal call site can't accidentally
    // bypass the plaintext block.
    if !url.to_ascii_lowercase().starts_with("https://") {
        bail!("refusing non-https fetch '{url}'");
    }
    fn name_for_download(url: &str) -> String {
        // Use the last path segment if it ends with a recognized archive
        // extension; otherwise fall back to a safe default.
        let no_query = url.split(['?', '#']).next().unwrap_or(url);
        let last = no_query.rsplit('/').next().unwrap_or("download");
        let lower = last.to_ascii_lowercase();
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".tar") {
            return last.to_string();
        }
        "download.tar.gz".to_string()
    }
    let tmp = TempDir::new()?;
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    if resp.status() >= 400 {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    // Preserve the URL-suggested extension so unpack can pick the right
    // decoder. Documented `.tar` (uncompressed) was being saved as `.tgz`
    // and failing in GzDecoder. Magic-byte detection in unpack_tarball is
    // the actual safeguard, but a sane filename helps debugging.
    let archive = tmp.path().join(name_for_download(url));
    // Stream the body directly to disk so multi-hundred-MB plugin tarballs
    // don't spike resident memory.
    let mut reader = resp.into_reader();
    let mut file =
        std::fs::File::create(&archive).with_context(|| format!("create {}", archive.display()))?;
    std::io::copy(&mut reader, &mut file)
        .with_context(|| format!("download body to {}", archive.display()))?;
    drop(file);
    unpack_tarball(&archive, tmp.path())?;
    let dir = locate_unpacked_root(tmp.path())?;
    Ok(Fetched {
        dir,
        _guard: Some(tmp),
    })
}

fn fetch_git(spec: &str) -> Result<Fetched> {
    // Format: git+<url>[#<ref>] (preferred) or git+<url>[@<ref>] (legacy,
    // refs without `/` only).
    let spec = spec.strip_prefix("git+").unwrap_or(spec);
    let (url_part, gitref) = if let Some((u, r)) = spec.rsplit_once('#') {
        (u, Some(r))
    } else if let Some((u, r)) = spec.rsplit_once('@') {
        // Legacy. Slash-containing refs (e.g. release/2026) get rejected
        // here so we don't accidentally split a URL with embedded
        // credentials. Users with such refs must switch to the # syntax.
        if r.contains('/') {
            (spec, None)
        } else {
            (u, Some(r))
        }
    } else {
        (spec, None)
    };
    // Plaintext git transports (http://, git://) ship executable code
    // without integrity. The first install hash gets locked, so a MITM
    // poisons trust permanently. Match the http:// rejection in fetch().
    let lower = url_part.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("git://") {
        bail!(
            "refusing plaintext git transport in '{spec}' — use git+https://, \
             git+ssh://, or fetch via https tarball."
        );
    }
    let url = url_part;
    let tmp = TempDir::new()?;
    let target = tmp.path().join("repo");
    clone_git_ref(url, gitref, &target)?;
    // Drop .git so the hash is content-only and reproducible.
    let dot_git = target.join(".git");
    if dot_git.exists() {
        std::fs::remove_dir_all(&dot_git).ok();
    }
    Ok(Fetched {
        dir: target,
        _guard: Some(tmp),
    })
}

fn clone_git_ref(url: &str, gitref: Option<&str>, target: &Path) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg("--depth=1");
    if let Some(r) = gitref {
        cmd.arg("--branch").arg(r);
    }
    cmd.arg(url).arg(target);
    let out = cmd.output().with_context(|| format!("git clone {url}"))?;
    if out.status.success() {
        return Ok(());
    }

    let first_err = String::from_utf8_lossy(&out.stderr).to_string();
    let Some(r) = gitref.filter(|r| looks_like_commit_sha(r)) else {
        bail!("git clone failed: {first_err}");
    };

    // `git clone --branch <ref>` accepts branches/tags, but not raw commit
    // SHAs on GitHub. Commit pins are common in aenv lock/import paths, so
    // fall back to a normal clone and detach at that commit.
    if target.exists() {
        std::fs::remove_dir_all(target).ok();
    }
    let out = Command::new("git")
        .arg("clone")
        .arg(url)
        .arg(target)
        .output()
        .with_context(|| format!("git clone {url}"))?;
    if !out.status.success() {
        bail!("git clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(target)
        .arg("checkout")
        .arg("--detach")
        .arg(r)
        .output()
        .with_context(|| format!("git checkout {r}"))?;
    if !out.status.success() {
        bail!(
            "git checkout '{r}' failed after clone fallback: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn looks_like_commit_sha(r: &str) -> bool {
    (7..=40).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_hexdigit())
}

fn fetch_npm(spec: &str) -> Result<Fetched> {
    // npm:<package>@<version>   (package may be scoped: @scope/name)
    let body = spec
        .strip_prefix("npm:")
        .ok_or_else(|| anyhow!("bad npm spec"))?;
    let (pkg, version) = parse_npm_spec(body)?;
    let registry_url = format!("https://registry.npmjs.org/{pkg}/{version}");
    let resp = ureq::get(&registry_url)
        .call()
        .with_context(|| format!("GET {registry_url}"))?;
    if resp.status() >= 400 {
        bail!("npm registry: HTTP {}", resp.status());
    }
    let body = resp.into_string()?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let tarball_url = v
        .get("dist")
        .and_then(|d| d.get("tarball"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("npm response missing dist.tarball"))?;
    // npm registries occasionally surface http:// URLs (mirrors, legacy
    // packages). Refuse plaintext — same MITM threat as direct
    // http://...tar.gz sources.
    if !tarball_url.to_ascii_lowercase().starts_with("https://") {
        bail!(
            "npm registry returned non-https tarball URL '{tarball_url}'; \
             refusing to fetch over plaintext."
        );
    }
    fetch_http(tarball_url)
}

/// Extract a tar (auto-detecting gzip by content magic bytes) into `dst`
/// using the safe iterator that rejects symlinks, hardlinks, devices,
/// absolute paths, and `..`-containing paths. Public so other modules
/// (import-profile, future bundle handlers) reuse the same defense rather
/// than duplicating `Archive::unpack` calls that follow symlinks.
pub fn unpack_tarball(archive: &Path, dst: &Path) -> Result<()> {
    // Magic-byte detection is more robust than extension matching: a
    // `.tar` file saved through fetch_http used to be sniffed as gzip
    // because the temp filename was hardcoded `.tgz`. Reading 2 bytes off
    // the front avoids that whole class of mismatch.
    let mut f =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut magic = [0u8; 2];
    use std::io::{Read, Seek, SeekFrom};
    let n = f.read(&mut magic).context("read tar magic")?;
    f.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", archive.display()))?;
    let is_gz = n == 2 && magic == [0x1f, 0x8b];
    if is_gz {
        unpack_safe(Archive::new(GzDecoder::new(f)), dst)
    } else {
        unpack_safe(Archive::new(f), dst)
    }
}

/// Manual safe extraction that:
///   - skips symlinks and hard links (would let an archive write outside `dst`)
///   - rejects entries with absolute paths or `..` components
///   - skips device/fifo/char-device entries
///   - drops uid/gid/mode bits we don't want to inherit
///
/// Defends against the classic tar symlink + dot-dot escape attacks.
fn unpack_safe<R: std::io::Read>(mut ar: Archive<R>, dst: &Path) -> Result<()> {
    use tar::EntryType;
    ar.set_preserve_permissions(false);
    ar.set_preserve_ownerships(false);
    ar.set_unpack_xattrs(false);
    let mut skipped = 0u32;
    for entry in ar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry header")?;
        let etype = entry.header().entry_type();
        match etype {
            EntryType::Regular
            | EntryType::Directory
            | EntryType::GNULongName
            | EntryType::XHeader => {}
            EntryType::Symlink | EntryType::Link => {
                skipped += 1;
                continue;
            }
            _ => {
                skipped += 1;
                continue;
            }
        }
        let path = entry.path().context("read tar entry path")?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            skipped += 1;
            continue;
        }
        let target = dst.join(&path);
        // Final defense: ensure the resolved target is still under `dst` even
        // if path normalization missed something (e.g. trailing dots).
        if !target.starts_with(dst) {
            skipped += 1;
            continue;
        }
        if !entry.unpack_in(dst).context("unpack tar entry")? {
            skipped += 1;
        }
    }
    if skipped > 0 {
        tracing::debug!("unpack: skipped {skipped} unsafe/non-regular tar entries");
    }
    Ok(())
}

/// Parse an npm package@version spec, correctly handling scoped packages
/// (`@scope/name@1.2.3`). The version separator is the *last* `@`, but for
/// scoped packages we must skip the leading `@` so the split happens at the
/// version boundary, not the scope boundary.
fn parse_npm_spec(body: &str) -> Result<(&str, &str)> {
    if body.is_empty() {
        bail!("npm spec is empty");
    }
    let (pkg, version) = if let Some(after) = body.strip_prefix('@') {
        // Scoped: `@scope/name@version`. Look for `@` after the scope part.
        match after.rsplit_once('@') {
            Some((rest, ver)) => (&body[..rest.len() + 1], ver),
            None => bail!("npm scoped spec missing version separator: '{body}'"),
        }
    } else {
        body.rsplit_once('@')
            .ok_or_else(|| anyhow!("npm spec needs @<version>: '{body}'"))?
    };
    if pkg.is_empty() || version.is_empty() {
        bail!("npm spec has empty package or version: '{body}'");
    }
    Ok((pkg, version))
}

/// If a tarball unpacks into a single top-level dir, return that. Otherwise return parent.
fn locate_unpacked_root(parent: &Path) -> Result<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(parent)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            // Ignore the archive file itself if still present.
            !e.path().is_file()
                || !e.file_name().to_string_lossy().ends_with(".tgz")
                    && !e.file_name().to_string_lossy().ends_with(".tar.gz")
                    && !e.file_name().to_string_lossy().ends_with(".tar")
        })
        .collect();
    entries.retain(|e| e.path().is_dir());
    if entries.len() == 1 {
        return Ok(entries.pop().unwrap().path());
    }
    Ok(parent.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn npm_unscoped_with_version() {
        assert_eq!(parse_npm_spec("foo@1.2.3").unwrap(), ("foo", "1.2.3"));
    }

    #[test]
    fn npm_scoped_with_version() {
        assert_eq!(
            parse_npm_spec("@scope/pkg@2.0.0").unwrap(),
            ("@scope/pkg", "2.0.0")
        );
    }

    #[test]
    fn npm_scoped_without_version_errors() {
        assert!(parse_npm_spec("@scope/pkg").is_err());
    }

    #[test]
    fn npm_no_at_errors() {
        assert!(parse_npm_spec("just-a-name").is_err());
    }

    #[test]
    fn npm_empty_errors() {
        assert!(parse_npm_spec("").is_err());
    }

    #[test]
    fn npm_empty_version_errors() {
        assert!(parse_npm_spec("foo@").is_err());
    }

    #[test]
    fn git_commit_sha_ref_fetches_detached_commit() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".claude-plugin")).unwrap();
        std::fs::write(
            repo.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"p","version":"0.1.0"}"#,
        )
        .unwrap();

        git(&repo, &["init"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "initial"]);
        let sha = git_output(&repo, &["rev-parse", "HEAD"]);

        let fetched = fetch(&format!("git+{}#{}", repo.display(), sha.trim())).unwrap();
        assert!(fetched
            .dir
            .join(".claude-plugin")
            .join("plugin.json")
            .is_file());
        assert!(!fetched.dir.join(".git").exists());
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = git_command(repo, args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_output(repo: &Path, args: &[&str]) -> String {
        let out = git_command(repo, args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn git_command(repo: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "aenv test")
            .env("GIT_AUTHOR_EMAIL", "aenv@example.invalid")
            .env("GIT_COMMITTER_NAME", "aenv test")
            .env("GIT_COMMITTER_EMAIL", "aenv@example.invalid");
        cmd
    }
}
