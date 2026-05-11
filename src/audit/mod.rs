//! Append-only audit log of mutating operations. Lightweight: one jsonl line
//! per operation with timestamp, kind, env, optional fields.
//!
//! Stored at `~/.aenv/audit.jsonl`. No rotation/policy yet — `aenv prune` /
//! manual rotation handles size.

use std::io::Write;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

pub fn log(
    kind: &str,
    env: Option<&str>,
    note: Option<String>,
    txn_id: Option<String>,
) -> Result<()> {
    let entry = AuditEntry {
        ts: chrono::Utc::now(),
        kind: kind.to_string(),
        env: env.map(|s| s.to_string()),
        note,
        txn_id,
        user: std::env::var("USER").ok(),
    };
    paths::ensure_dir(&paths::aenv_home()?)?;
    let p = paths::aenv_home()?.join("audit.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .with_context(|| format!("open {}", p.display()))?;
    let line = serde_json::to_string(&entry)?;
    writeln!(f, "{line}").with_context(|| format!("write {}", p.display()))?;
    Ok(())
}

pub fn read_all(limit: usize) -> Result<Vec<AuditEntry>> {
    let p = paths::aenv_home()?.join("audit.jsonl");
    if !p.is_file() {
        return Ok(vec![]);
    }
    let body = std::fs::read_to_string(&p)?;
    let mut entries: Vec<AuditEntry> = body
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit);
    Ok(entries)
}
