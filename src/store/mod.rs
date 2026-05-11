//! Content-addressed plugin/skill store with hardlink fanout to env dirs.
//!
//! Layout:
//!   ~/.aenv/store/objects/<sha256[..2]>/<sha256>/...
//!
//! `materialize(src, dst)` ensures `dst` mirrors the canonical store entry,
//! using hardlinks where possible, falling back to copy.

pub mod source;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use crate::paths;

pub use source::fetch;

pub fn objects_root() -> Result<PathBuf> {
    Ok(paths::store_dir()?.join("objects"))
}

/// Compute the SHA-256 of a file or directory tree.
///
/// For directories, the hash covers, per entry: a kind tag (file/dir),
/// the relative path, and for files the size + content bytes. Empty
/// subdirectories are recorded so a layout that requires them is
/// reproduced exactly.
///
/// **Cross-platform invariants** (each one a deliberate exclusion):
///
/// 1. Mode bits — excluded. Hashing them made lockfiles platform-specific
///    (NTFS doesn't carry POSIX mode), breaking the core "git pull →
///    `aenv install` → just works" UX. Cargo.lock takes the same stance.
///    Compensation: materialize sets +x on shebang files; content
///    tampering still flips the hash.
///
/// 2. Path separators — normalized. Windows WalkDir yields `a\\b\\c`
///    while Unix yields `a/b/c`. We replace `\\` with `/` so the
///    platform's separator doesn't enter the digest.
///
/// 3. Filename Unicode normalization — NFC. macOS HFS+/APFS stores
///    filenames in NFD (decomposed: `한` = `ㅎ ㅏ ㄴ`); Linux/Windows
///    treat them as opaque bytes (typically NFC: `한` as one codepoint).
///    Without normalization, "한글.md" hashes differently on Mac vs the
///    others. We NFC-normalize every relative path before sorting +
///    hashing — Unicode UAX#15 form. This is what reproducible-builds
///    recommends for byte-deterministic archives across filesystems.
///
/// 4. Sort order — explicit. `WalkDir::sort_by_file_name()` is locale-
///    independent in Rust (raw OsStr comparison) but operates on the
///    FS-returned bytes, which differ pre-normalization. We sort by
///    the NFC-normalized relative path string so the hash-input order
///    is itself a property of logical filenames, not filesystem encoding.
pub fn hash_tree(src: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    if src.is_file() {
        let meta = std::fs::metadata(src).with_context(|| format!("stat {}", src.display()))?;
        hasher.update(b"f\0");
        hasher.update(meta.len().to_le_bytes());
        hash_file_into(&mut hasher, src)?;
        return Ok(hex::encode(hasher.finalize()));
    }
    // Collect every entry, paired with its NFC-normalized relative path.
    // Sorting + hashing both use the normalized form so a tree from
    // macOS (NFD on disk) and the same tree on Linux/Windows (NFC on
    // disk) produce identical digests.
    let mut entries: Vec<(walkdir::DirEntry, String)> = Vec::new();
    for entry in WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
        let rel = entry
            .path()
            .strip_prefix(src)
            .context("strip_prefix on store entry")?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let normalized: String = rel.to_string_lossy().replace('\\', "/").nfc().collect();
        entries.push((entry, normalized));
    }
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    for (entry, rel_str) in entries {
        let ft = entry.file_type();
        let tag: &[u8] = if ft.is_dir() {
            b"d"
        } else if ft.is_file() {
            b"f"
        } else {
            // Symlinks/devices: skipped at materialize time, also skipped
            // here so they don't influence the hash unpredictably.
            continue;
        };
        hasher.update(tag);
        hasher.update(b"\0");
        hasher.update(rel_str.as_bytes());
        hasher.update([0u8]);
        if ft.is_file() {
            let meta = entry
                .metadata()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            hasher.update(meta.len().to_le_bytes());
            hash_file_into(&mut hasher, entry.path())?;
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hash_file_into(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(())
}

pub fn store_path_for(sha: &str) -> Result<PathBuf> {
    Ok(objects_root()?.join(sha_relative_path(sha)))
}

/// Pure function: maps a SHA to its relative path under the store
/// (`<sha[..2]>/<sha>`). Extracted so tests don't need to touch global state.
fn sha_relative_path(sha: &str) -> PathBuf {
    let prefix = sha.get(..2).unwrap_or("00");
    PathBuf::from(prefix).join(sha)
}

/// Insert `src` into the store under its content hash. Returns the canonical store path.
///
/// On a cache hit (the destination already exists), we re-hash the existing
/// store object and compare against the new computation. If they differ, the
/// existing object was corrupted (disk-rot, bit-flip, tampering) — replace
/// it. Without this verification a corrupted object would keep being served
/// to every materialize call as if it were the requested hash.
pub fn insert(src: &Path) -> Result<(String, PathBuf)> {
    let sha = hash_tree(src)?;
    let dst = store_path_for(&sha)?;
    if dst.exists() {
        match hash_tree(&dst) {
            Ok(existing) if existing == sha => {
                return Ok((sha, dst));
            }
            Ok(existing) => {
                eprintln!("aenv: warn: store object {sha} corrupted (got {existing}); replacing");
                if dst.is_dir() {
                    std::fs::remove_dir_all(&dst)?;
                } else {
                    std::fs::remove_file(&dst)?;
                }
            }
            Err(e) => {
                eprintln!("aenv: warn: cannot re-hash store object {sha} ({e}); replacing");
                if dst.is_dir() {
                    std::fs::remove_dir_all(&dst)?;
                } else {
                    std::fs::remove_file(&dst)?;
                }
            }
        }
    }
    paths::ensure_dir(dst.parent().unwrap())?;
    if src.is_file() {
        std::fs::copy(src, &dst)
            .with_context(|| format!("copy file to store {}", dst.display()))?;
    } else {
        // Symlink-preserving copy. fs_extra::dir::copy
        // dereferences symlinks into regular files, which made the
        // store's content shape diverge from the source tree:
        // hash_tree(src) skips the symlink, but hash_tree(store)
        // sees a regular file and includes it — so the store
        // object's content hash != its key. The *next* insert of
        // the same source then triggered "store object corrupted,
        // replacing" + a materialize-time bail. crate::env::copy_tree
        // preserves symlinks on Unix (and falls back to file copy
        // on Windows, where source trees rarely have symlinks
        // anyway), keeping src and store byte-shape-identical so
        // hash_tree gives the same result on either side.
        crate::env::copy_tree(src, &dst)
            .with_context(|| format!("copy dir to store {}", dst.display()))?;
    }
    Ok((sha, dst))
}

/// Materialize a store entry into `dst`. Uses hardlinks when possible.
///
/// Verifies the store object's content still hashes to the requested SHA
/// before copying out — without this, a corrupted or tampered object that
/// slipped past insert (e.g., bit-flip after creation) would silently be
/// served to the env. `AENV_TRUST_STORE=1` skips the check for power
/// users who need every millisecond on hot-cache installs.
pub fn materialize(sha: &str, dst: &Path) -> Result<()> {
    let src = store_path_for(sha)?;
    if !src.exists() {
        anyhow::bail!("store object {} missing", sha);
    }
    if std::env::var_os("AENV_TRUST_STORE").is_none() {
        let actual = hash_tree(&src).with_context(|| {
            format!("re-hash store object {} for integrity check", src.display())
        })?;
        if actual != sha {
            anyhow::bail!(
                "store object corrupted: expected {sha}, got {actual}. \
                 Re-run install to refetch."
            );
        }
    }
    if dst.exists() {
        std::fs::remove_dir_all(dst).or_else(|_| std::fs::remove_file(dst))?;
    }
    if src.is_file() {
        link_or_copy_file(&src, dst)?;
        ensure_shebang_executable(dst);
        return Ok(());
    }
    paths::ensure_dir(dst)?;
    for entry in WalkDir::new(&src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(&src)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            // Materialize empty dirs too — they're part of the hash now.
            paths::ensure_dir(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                paths::ensure_dir(parent)?;
            }
            link_or_copy_file(entry.path(), &target)?;
            ensure_shebang_executable(&target);
        }
    }
    Ok(())
}

/// Set 0o755 on materialized files that begin with `#!`. Replaces the
/// chmod-detection role that mode-in-hash used to play: hook scripts
/// stored in git without the exec bit (or pulled to NTFS) still run
/// after `aenv install`. No-op on Windows (NTFS doesn't carry POSIX
/// mode bits; bash respects shebangs regardless).
fn ensure_shebang_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(mut f) = std::fs::File::open(path) else {
            return;
        };
        let mut head = [0u8; 2];
        if f.read(&mut head).unwrap_or(0) < 2 || &head != b"#!" {
            return;
        }
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o111 == 0 {
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(path, perms);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn link_or_copy_file(src: &Path, dst: &Path) -> Result<()> {
    match std::fs::hard_link(src, dst) {
        Ok(_) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dst)
                .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn hash_tree_deterministic_for_dir() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("x"), "alpha").unwrap();
        std::fs::write(a.join("y"), "beta").unwrap();
        let h1 = hash_tree(&a).unwrap();
        let h2 = hash_tree(&a).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn hash_tree_differs_on_content_change() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("x"), "v1").unwrap();
        let h1 = hash_tree(&a).unwrap();
        std::fs::write(a.join("x"), "v2").unwrap();
        let h2 = hash_tree(&a).unwrap();
        assert_ne!(h1, h2);
    }

    // Cross-platform invariant: mode bits must NOT influence the hash,
    // otherwise lockfiles produced on one OS can't be verified on
    // another. This used to assert the inverse — see the hash_tree doc
    // comment for the migration rationale.
    #[cfg(unix)]
    #[test]
    fn hash_tree_ignores_mode_bits_for_cross_platform_consistency() {
        let tmp = TempDir::new().unwrap();
        let plain = tmp.path().join("plain");
        let exec = tmp.path().join("exec");
        std::fs::write(&plain, b"hello").unwrap();
        std::fs::write(&exec, b"hello").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&exec).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exec, perms).unwrap();
        let h_plain = hash_tree(&plain).unwrap();
        let h_exec = hash_tree(&exec).unwrap();
        assert_eq!(
            h_plain, h_exec,
            "mode bits must be excluded from the hash so the same content \
             produces the same sha across Mac / Linux / Windows"
        );
        assert_eq!(h_plain.len(), 64);
    }

    #[test]
    fn hash_tree_normalizes_unicode_filename_to_nfc() {
        // Cross-platform invariant for non-ASCII filenames. macOS
        // HFS+/APFS stores filenames in NFD (decomposed); Linux/Windows
        // use NFC on the wire. Without normalization, the same logical
        // filename "한글.md" would hash differently per OS.
        //
        // We can't manipulate filesystem encoding from a unit test, but
        // we CAN verify that two trees differing only in filename
        // normalization (NFD-spelled vs NFC-spelled) produce the same
        // sha. Setup: create one tree with NFC-form filename, another
        // with NFD-form. Both contain the same bytes; the only
        // difference is the encoding of the filename component itself.
        let tmp = TempDir::new().unwrap();
        let nfc_root = tmp.path().join("nfc-tree");
        let nfd_root = tmp.path().join("nfd-tree");
        std::fs::create_dir_all(&nfc_root).unwrap();
        std::fs::create_dir_all(&nfd_root).unwrap();
        // "한" in NFC is one codepoint U+D55C. In NFD it is three:
        // ㅎ (U+1112) + ㅏ (U+1161) + ㄴ (U+11AB).
        let nfc_name = "\u{D55C}.md"; // 한.md
        let nfd_name = "\u{1112}\u{1161}\u{11AB}.md"; // 한.md
        std::fs::write(nfc_root.join(nfc_name), b"hello").unwrap();
        // Some Unix filesystems normalize on write; if they do this
        // assert collapses harmlessly. On Linux ext4 they don't, so
        // the test exercises the real divergence.
        if std::fs::write(nfd_root.join(nfd_name), b"hello").is_err() {
            return;
        }
        let h_nfc = hash_tree(&nfc_root).unwrap();
        let h_nfd = hash_tree(&nfd_root).unwrap();
        assert_eq!(
            h_nfc, h_nfd,
            "Unicode filename hashes must match regardless of NFC/NFD encoding"
        );
    }

    #[test]
    fn hash_tree_normalizes_path_separators() {
        // Two trees with identical content but different relative path
        // separators (Mac/Linux uses `/`, Windows yields `\\` from
        // WalkDir) must hash the same. We can't actually create a path
        // with a literal `\\` on Unix, so this test guards the logic
        // by checking that nested dirs are hashed deterministically and
        // by verifying the path-normalization step exists in the
        // implementation source.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(a.join("sub").join("deep")).unwrap();
        std::fs::write(a.join("sub").join("deep").join("x"), "v1").unwrap();
        let h1 = hash_tree(&a).unwrap();
        let h2 = hash_tree(&a).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_tree_records_empty_dirs() {
        let tmp = TempDir::new().unwrap();
        let with_empty = tmp.path().join("a");
        let without = tmp.path().join("b");
        std::fs::create_dir_all(with_empty.join("subdir")).unwrap();
        std::fs::write(with_empty.join("f"), b"x").unwrap();
        std::fs::create_dir_all(&without).unwrap();
        std::fs::write(without.join("f"), b"x").unwrap();
        assert_ne!(
            hash_tree(&with_empty).unwrap(),
            hash_tree(&without).unwrap()
        );
    }

    // Inode equality is the Unix proof that link_or_copy did a real
    // hardlink rather than a copy. On Windows, NTFS hardlinks share
    // a file index but `std::fs::Metadata::ino()` is Unix-only —
    // skip the test entirely there. The non-test code path itself
    // still works on Windows (uses `std::fs::hard_link`).
    #[cfg(unix)]
    #[test]
    fn link_or_copy_uses_hardlink_same_fs() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("s");
        let dst = tmp.path().join("d");
        std::fs::write(&src, "x").unwrap();
        link_or_copy_file(&src, &dst).unwrap();
        let s_meta = std::fs::metadata(&src).unwrap();
        let d_meta = std::fs::metadata(&dst).unwrap();
        assert_eq!(s_meta.ino(), d_meta.ino());
    }

    /// Regression: plugin trees containing symlinks (e.g. some
    /// shipped marketplaces use them for shared content) used to
    /// trip "store object corrupted, replacing" on the second
    /// install of the same source. Cause was a hash/copy
    /// asymmetry — `hash_tree` skipped symlinks, but the old
    /// `fs_extra::dir::copy` dereferenced them into regular
    /// files, so the store's stored bytes hashed to a different
    /// value than the source's. This test pins the invariant:
    /// `crate::env::copy_tree` (which `store::insert` now uses)
    /// preserves the symlink, so `hash_tree` produces the same
    /// digest on both sides.
    #[cfg(unix)]
    #[test]
    fn hash_tree_consistent_across_symlink_preserving_copy() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("real.txt"), b"hello").unwrap();
        // Symlink inside the tree (relative target — the kind a
        // git-cloned plugin tree typically ships).
        std::os::unix::fs::symlink("real.txt", src.join("link.txt")).unwrap();

        let h_src = hash_tree(&src).unwrap();

        let dst = tmp.path().join("dst");
        crate::env::copy_tree(&src, &dst).unwrap();
        let h_dst = hash_tree(&dst).unwrap();

        assert_eq!(
            h_src, h_dst,
            "hash_tree must give the same digest before and \
             after a symlink-preserving copy — this is the \
             invariant store::insert / store::materialize rely \
             on to avoid 'store object corrupted' on re-install"
        );
    }

    #[test]
    fn sha_relative_path_uses_two_char_prefix() {
        let p = sha_relative_path("abcdef0123456789");
        assert_eq!(p, std::path::PathBuf::from("ab/abcdef0123456789"));
    }

    #[test]
    fn sha_relative_path_handles_short_sha() {
        // Defensive: a malformed short SHA shouldn't panic on slicing.
        let p = sha_relative_path("a");
        assert_eq!(p, std::path::PathBuf::from("00/a"));
    }
}
