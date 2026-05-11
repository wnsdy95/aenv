//! Transactional wrappers for mutating operations on env directories.
//!
//! Pattern:
//!   1. Acquire global flock(`~/.aenv/.lock`).
//!   2. Snapshot relevant files into `~/.aenv/state/<iso-timestamp>/`.
//!   3. Run the operation.
//!   4. On error: restore from snapshot.
//!   5. On success: write `manifest.json` describing the txn.
//!
//! `aenv rollback` reads the most recent snapshot and restores it.
//! `aenv history` lists all snapshots.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::paths;

/// Holds the global flock until dropped.
pub struct GlobalLock {
    _file: File,
}

impl GlobalLock {
    pub fn acquire() -> Result<Self> {
        paths::ensure_dir(&paths::aenv_home()?)?;
        let p = paths::aenv_home()?.join(".lock");
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&p)
            .with_context(|| format!("open {}", p.display()))?;
        // Try non-blocking first so the user gets a clear "another aenv
        // process is running" message instead of an indefinite hang. Then
        // fall back to a blocking acquire — flock(2) is interruptible by
        // signals so Ctrl+C still works.
        match f.try_lock_exclusive() {
            Ok(()) => Ok(Self { _file: f }),
            Err(_) => {
                eprintln!("aenv: another aenv process holds {}; waiting…", p.display());
                f.lock_exclusive()
                    .with_context(|| format!("flock {}", p.display()))?;
                Ok(Self { _file: f })
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxManifest {
    pub id: String,
    pub started: chrono::DateTime<chrono::Utc>,
    pub finished: Option<chrono::DateTime<chrono::Utc>>,
    pub kind: String,
    pub env: Option<String>,
    /// List of (relative path within env, snapshot path) pairs that we backed up.
    pub backed_up: Vec<BackupEntry>,
    pub status: TxStatus,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupEntry {
    pub original: PathBuf,
    pub snapshot: PathBuf,
    pub kind: BackupKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackupKind {
    File,
    Dir,
    Missing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TxStatus {
    Pending,
    Committed,
    RolledBack,
}

pub struct Transaction {
    pub manifest: TxManifest,
    pub dir: PathBuf,
    _lock: GlobalLock,
}

impl Transaction {
    pub fn begin(kind: &str, env: Option<&str>, note: Option<String>) -> Result<Self> {
        let lock = GlobalLock::acquire()?;
        let id = format!(
            "{}-{:x}",
            Utc::now().format("%Y%m%dT%H%M%SZ"),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );
        let dir = paths::state_dir()?.join(&id);
        paths::ensure_dir(&dir)?;
        let manifest = TxManifest {
            id,
            started: Utc::now(),
            finished: None,
            kind: kind.to_string(),
            env: env.map(|s| s.to_string()),
            backed_up: vec![],
            status: TxStatus::Pending,
            note,
        };
        let tx = Self {
            manifest,
            dir,
            _lock: lock,
        };
        tx.write_manifest()?;
        Ok(tx)
    }

    /// Snapshot a path into the transaction. If the path doesn't exist, this is
    /// recorded so rollback can re-delete it after a failed create.
    pub fn capture(&mut self, original: &Path) -> Result<()> {
        let kind = if !original.exists() {
            BackupKind::Missing
        } else if original.is_dir() {
            BackupKind::Dir
        } else {
            BackupKind::File
        };
        let snap_name = path_to_snap_name(original);
        let snap_path = self.dir.join(&snap_name);
        match kind {
            BackupKind::Missing => {}
            BackupKind::File => {
                std::fs::copy(original, &snap_path).with_context(|| {
                    format!("snapshot {} -> {}", original.display(), snap_path.display())
                })?;
            }
            BackupKind::Dir => {
                paths::ensure_dir(&snap_path)?;
                crate::env::copy_tree(original, &snap_path)?;
            }
        }
        self.manifest.backed_up.push(BackupEntry {
            original: original.to_path_buf(),
            snapshot: snap_path,
            kind,
        });
        self.write_manifest()?;
        Ok(())
    }

    pub fn commit(mut self) -> Result<()> {
        self.manifest.finished = Some(Utc::now());
        self.manifest.status = TxStatus::Committed;
        self.write_manifest()?;
        Ok(())
    }

    /// Restore all captured snapshots (used on operation failure).
    pub fn rollback(mut self) -> Result<()> {
        let entries = self.manifest.backed_up.clone();
        for be in entries.iter().rev() {
            restore_one(be)?;
        }
        self.manifest.finished = Some(Utc::now());
        self.manifest.status = TxStatus::RolledBack;
        self.write_manifest()?;
        Ok(())
    }

    fn write_manifest(&self) -> Result<()> {
        let p = self.dir.join("manifest.json");
        let body = serde_json::to_string_pretty(&self.manifest)?;
        crate::paths::write_atomic(&p, body.as_bytes())?;
        Ok(())
    }
}

fn restore_one(be: &BackupEntry) -> Result<()> {
    match be.kind {
        BackupKind::Missing => {
            // The original didn't exist before the txn — remove anything that exists now.
            if be.original.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&be.original) {
                    eprintln!(
                        "aenv: warn: rollback could not remove dir {}: {e}",
                        be.original.display()
                    );
                }
            } else if be.original.exists() {
                if let Err(e) = std::fs::remove_file(&be.original) {
                    eprintln!(
                        "aenv: warn: rollback could not remove file {}: {e}",
                        be.original.display()
                    );
                }
            }
        }
        BackupKind::File => {
            if let Some(parent) = be.original.parent() {
                paths::ensure_dir(parent)?;
            }
            if be.original.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&be.original) {
                    eprintln!(
                        "aenv: warn: rollback could not remove dir-blocking-file {}: {e}",
                        be.original.display()
                    );
                }
            }
            std::fs::copy(&be.snapshot, &be.original).with_context(|| {
                format!(
                    "restore {} <- {}",
                    be.original.display(),
                    be.snapshot.display()
                )
            })?;
        }
        BackupKind::Dir => {
            if be.original.exists() {
                if let Err(e) = std::fs::remove_dir_all(&be.original) {
                    eprintln!(
                        "aenv: warn: rollback could not remove existing dir {}: {e}",
                        be.original.display()
                    );
                }
            }
            paths::ensure_dir(&be.original)?;
            crate::env::copy_tree(&be.snapshot, &be.original)?;
        }
    }
    Ok(())
}

fn path_to_snap_name(p: &Path) -> String {
    p.to_string_lossy().replace('/', "%").replace(':', "_")
}

/// List all transactions in `~/.aenv/state/`, newest first.
pub fn list_transactions() -> Result<Vec<TxManifest>> {
    let dir = paths::state_dir()?;
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let m = entry.path().join("manifest.json");
        if !m.is_file() {
            continue;
        }
        // Surface read/parse failures so a corrupt snapshot isn't silently
        // hidden from `aenv history` / `aenv rollback`. We still skip the
        // entry rather than aborting — one bad snapshot must not block
        // recovery of the others.
        let body = match std::fs::read_to_string(&m) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("aenv: warn: cannot read tx manifest {}: {e}", m.display());
                continue;
            }
        };
        match serde_json::from_str::<TxManifest>(&body) {
            Ok(tm) => out.push(tm),
            Err(e) => {
                eprintln!("aenv: warn: cannot parse tx manifest {}: {e}", m.display());
            }
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.started));
    Ok(out)
}

/// Restore the most recent committed transaction's snapshots. If
/// `include_pending` is true, also matches Pending transactions — used to
/// recover from a process killed mid-operation.
pub fn rollback_latest(include_pending: bool) -> Result<TxManifest> {
    let _lock = GlobalLock::acquire()?;
    let txns = list_transactions()?;
    let last = txns
        .into_iter()
        .find(|t| {
            t.status == TxStatus::Committed || (include_pending && t.status == TxStatus::Pending)
        })
        .ok_or_else(|| {
            if include_pending {
                anyhow!("no committed or pending transaction to roll back")
            } else {
                anyhow!(
                    "no committed transaction to roll back. \
                     If a previous run was killed mid-operation, try \
                     `aenv rollback --pending`."
                )
            }
        })?;
    for be in last.backed_up.iter().rev() {
        restore_one(be)?;
    }
    let mut updated = last.clone();
    updated.status = TxStatus::RolledBack;
    updated.finished = Some(Utc::now());
    let p = paths::state_dir()?.join(&last.id).join("manifest.json");
    crate::paths::write_atomic(&p, serde_json::to_string_pretty(&updated)?.as_bytes())?;
    Ok(updated)
}

/// Count of Pending transactions on disk — used by `doctor` to flag possible
/// killed-mid-operation state that needs `aenv rollback --pending`.
pub fn count_pending() -> usize {
    list_transactions()
        .unwrap_or_default()
        .iter()
        .filter(|t| t.status == TxStatus::Pending)
        .count()
}

/// Delete txns older than `keep_days` or beyond `keep_count` (whichever first).
/// Acquires the global lock so it doesn't race with concurrent install/rollback.
pub fn prune(keep_count: usize, keep_days: i64) -> Result<usize> {
    let _lock = GlobalLock::acquire()?;
    let txns = list_transactions()?;
    let cutoff = Utc::now() - chrono::Duration::days(keep_days);
    let mut removed = 0;
    for (i, t) in txns.iter().enumerate() {
        let too_old = t.started < cutoff;
        let over_count = i >= keep_count;
        if too_old || over_count {
            let dir = paths::state_dir()?.join(&t.id);
            if dir.is_dir() {
                std::fs::remove_dir_all(&dir).ok();
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Run a closure inside a transaction. On Err, rollback. On Ok, commit.
pub fn with_tx<F, T>(
    kind: &str,
    env: Option<&str>,
    paths_to_capture: &[PathBuf],
    note: Option<String>,
    f: F,
) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let mut tx = Transaction::begin(kind, env, note)?;
    for p in paths_to_capture {
        tx.capture(p)?;
    }
    let txn_id = tx.manifest.id.clone();
    match f() {
        Ok(v) => {
            let env_str = tx.manifest.env.clone();
            let note_str = tx.manifest.note.clone();
            tx.commit()?;
            // Audit-log failure must not abort a successful txn — the work
            // is already on disk and rolling back here would itself need
            // another audit entry. Surface the failure so an unwriteable
            // log path doesn't silently break the audit trail.
            if let Err(e) =
                crate::audit::log(kind, env_str.as_deref(), note_str, Some(txn_id.clone()))
            {
                eprintln!("aenv: warn: audit log write failed for txn {txn_id}: {e:#}");
            }
            Ok(v)
        }
        Err(e) => {
            let env_str = tx.manifest.env.clone();
            let note_str = tx.manifest.note.clone();
            if let Err(re) = tx.rollback() {
                bail!(
                    "operation failed AND rollback failed (txn {txn_id}): orig={e:#}, rollback={re:#}"
                );
            }
            if let Err(le) = crate::audit::log(
                &format!("{kind}:rollback"),
                env_str.as_deref(),
                Some(format!("error: {e:#}; {}", note_str.unwrap_or_default())),
                Some(txn_id.clone()),
            ) {
                eprintln!(
                    "aenv: warn: audit log write failed for rolled-back txn {txn_id}: {le:#}"
                );
            }
            Err(e)
        }
    }
}
