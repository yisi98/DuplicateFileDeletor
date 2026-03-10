use anyhow::Result;
use duplicate_file_deletor::{run, usage, OperationMode, RunConfig};
use std::env;

fn main() -> Result<()> {
    env_logger::init();
    let args: Vec<String> = env::args().collect();
    let binary = args
        .first()
        .cloned()
        .unwrap_or_else(|| "duplicate-file-deletor-cli".to_string());

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", usage(&binary));
        return Ok(());
    }

    let config = RunConfig::from_args(args)?;
    let result = run(&config)?;

    println!("Mode: {}", result.summary.mode.as_str());
    println!("Scanned files: {}", result.summary.scanned_files);
    println!("Kept files: {}", result.summary.kept_files);
    println!("Planned deletions: {}", result.summary.planned_deletions);
    if result.summary.mode == OperationMode::Apply {
        println!("Deleted files: {}", result.summary.deleted_files);
        println!("Failed deletions: {}", result.summary.failed_deletions);
    } else {
        println!(
            "Dry-run complete. Review {}",
            result
                .summary
                .output_dir
                .join("planned_deletions.csv")
                .display()
        );
    }

    Ok(())
}
