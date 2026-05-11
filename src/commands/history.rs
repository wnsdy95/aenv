use anyhow::Result;

use crate::cli::HistoryArgs;
use crate::tx;

pub fn run(args: HistoryArgs) -> Result<u8> {
    let txns = tx::list_transactions()?;
    if txns.is_empty() {
        println!("(no transactions)");
        return Ok(0);
    }
    for t in txns.into_iter().take(args.limit) {
        println!(
            "{}  {:<14}  env={:<12}  status={:?}  {}",
            t.started.format("%Y-%m-%d %H:%M:%SZ"),
            t.kind,
            t.env.unwrap_or_else(|| "-".into()),
            t.status,
            t.note.unwrap_or_default()
        );
    }
    Ok(0)
}
