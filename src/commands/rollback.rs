use anyhow::Result;

use crate::cli::RollbackArgs;
use crate::tx;

pub fn run(args: RollbackArgs) -> Result<u8> {
    let m = tx::rollback_latest(args.pending)?;
    println!("rolled back txn {} ({})", m.id, m.kind);
    Ok(0)
}
