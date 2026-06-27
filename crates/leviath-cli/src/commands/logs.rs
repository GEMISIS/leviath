//! `lev logs` - Stream or print logs for a background run.

use clap::Args;
use crate::runstate::{run_dir, tail_log};

#[derive(Args)]
pub struct LogsArgs {
    /// Run ID to show logs for
    pub run_id: String,

    /// Follow the log (poll for new output)
    #[arg(short, long)]
    pub follow: bool,

    /// Number of bytes from the end to show initially (default: 32768)
    #[arg(short = 'n', long, default_value = "32768")]
    pub tail_bytes: u64,
}

pub async fn execute(args: LogsArgs) -> anyhow::Result<()> {
    let log_path = run_dir(&args.run_id).join("output.log");

    if !log_path.exists() {
        anyhow::bail!(
            "No log found for run '{}'. Check the run ID with: lev ps",
            args.run_id
        );
    }

    // Print initial tail
    let initial = tail_log(&args.run_id, args.tail_bytes);
    if !initial.is_empty() {
        print!("{}", initial);
    }

    if !args.follow {
        return Ok(());
    }

    // Follow mode: poll for new bytes
    let mut last_size = std::fs::metadata(&log_path)
        .map(|m| m.len())
        .unwrap_or(0);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let current_size = match std::fs::metadata(&log_path) {
            Ok(m) => m.len(),
            Err(_) => break,
        };

        if current_size > last_size {
            use std::io::{Read, Seek, SeekFrom};
            if let Ok(mut file) = std::fs::File::open(&log_path) {
                if file.seek(SeekFrom::Start(last_size)).is_ok() {
                    let mut buf = Vec::new();
                    let _ = file.read_to_end(&mut buf);
                    print!("{}", String::from_utf8_lossy(&buf));
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
            last_size = current_size;
        }

        // Check if the run is done
        if let Ok(meta) = crate::runstate::read_meta(&args.run_id) {
            match meta.status {
                crate::runstate::RunStatus::Complete
                | crate::runstate::RunStatus::Error
                | crate::runstate::RunStatus::Cancelled => break,
                _ => {}
            }
        }
    }

    Ok(())
}
