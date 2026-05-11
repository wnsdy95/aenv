use anyhow::Result;

use crate::cli::PruneArgs;
use crate::tx;

pub fn run(args: PruneArgs) -> Result<u8> {
    let removed = tx::prune(args.keep_count, args.keep_days)?;
    println!("pruned {removed} snapshots");
    Ok(0)
}
