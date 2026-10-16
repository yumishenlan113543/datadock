mod config;
mod core;

use config::TasksList;
use log::{error, info, warn};

/// Configure a file-only logger that writes to `app.log` in the platform user
/// data directory (e.g. `~/.local/share/datadock/app.log` on Linux).
///
/// The log file is truncated at the start of each run. The level defaults to
/// `Info` and can be overridden via the `RUST_LOG` environment variable.
fn init_file_logger() -> std::io::Result<()> {
    // Resolve the application's platform-specific directories.
    let dirs = directories::ProjectDirs::from("", "", env!("CARGO_CRATE_NAME"))
        .ok_or_else(|| std::io::Error::other("Unable to determine the application directory."))?;
    // The data directory is where the log file is written.
    let dir = dirs.data_dir();
    std::fs::create_dir_all(dir)?;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join("app.log"))?;

    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();

    info!(
        "File logger initialized at {}",
        dir.join("app.log").display()
    );
    Ok(())
}

/// Load the configured tasks and run them, logging progress and results to the
/// log file while printing a short summary to the console.
fn main() {
    // Initialize the logger.
    init_file_logger().unwrap();

    // Load the tasks list.
    println!("Loading the tasks...");
    let tasks_list = TasksList::new().unwrap_or_else(|e| {
        error!("Failed to load tasks: {e}");
        eprintln!("Failed to load tasks: {e}");
        std::process::exit(1);
    });
    info!("Successfully loaded {} task(s)", tasks_list.tasks.len());

    // Nothing to do when no tasks are defined.
    if tasks_list.tasks.is_empty() {
        warn!("No tasks are configured; nothing to run.");
        println!("No tasks to run.");
        return;
    }

    // Run the tasks.
    println!("Running the tasks...");
    let (handles, errors) = core::run_tasks_list(&tasks_list);

    // Wait for every task thread and tally the results.
    let mut succeeded = 0u32;
    let mut failed = 0u32;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {
                // The thread finished and the internal logic executed successfully.
                succeeded += 1;
            }
            Ok(Err(e)) => {
                // The thread finished but the internal logic failed.
                error!("Task error during copying: {e}");
                failed += 1;
            }
            Err(e) => {
                // The thread panicked.
                error!("Thread panicked: {e:?}");
                failed += 1;
            }
        }
    }
    for error in errors {
        error!("Task error at startup: {error}");
        failed += 1;
    }

    info!("Run finished: {succeeded} succeeded, {failed} failed.");
    println!("Run finished: {succeeded} succeeded, {failed} failed.");
}
