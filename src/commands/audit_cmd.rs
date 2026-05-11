use anyhow::Result;

use crate::audit;
use crate::cli::AuditArgs;

pub fn run(args: AuditArgs) -> Result<u8> {
    let entries = audit::read_all(args.limit)?;
    if args.json {
        for e in entries {
            println!("{}", serde_json::to_string(&e)?);
        }
    } else if entries.is_empty() {
        println!("(no audit entries)");
    } else {
        for e in entries {
            println!(
                "{}  {:<22}  env={:<10}  {}",
                e.ts.format("%Y-%m-%d %H:%M:%SZ"),
                e.kind,
                e.env.unwrap_or_else(|| "-".into()),
                e.note.unwrap_or_default()
            );
        }
    }
    Ok(0)
}
