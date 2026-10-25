mod config;
mod core;

use indicatif::MultiProgress;
use log::{error, info, warn};
use terminal_size::{Width, terminal_size};

use core::task::TasksList;

/// Configure a file-only logger that writes to `app.log` in the platform user
/// data directory (e.g. `~/.local/share/datadock/app.log` on Linux).
///
/// The log file is truncated each run. The level defaults to `Info` and can be
/// overridden with `RUST_LOG`.
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

    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();

    info!(
        "File logger initialized at {}",
        dir.join("app.log").display()
    );
    Ok(())
}

/// Print a full-width divider, falling back to 80 columns if the terminal
/// width is unavailable.
fn print_divider() {
    let width = terminal_size()
        .map(|(Width(w), _)| w as usize)
        .unwrap_or(80);
    println!("{}", "─".repeat(width));
}

/// Load the configured tasks and run them, logging progress and results to the
/// log file and printing a summary to the console.
fn main() {
    // Initialize the logger.
    init_file_logger().unwrap();

    // Parse command-line arguments.
    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|arg| arg == "--dry-run");

    // Load the tasks list.
    println!("Loading the tasks...");
    let mut tasks_list = TasksList::default();
    tasks_list.load().unwrap_or_else(|e| {
        error!("Failed to load tasks: {e}");
        eprintln!("Failed to load tasks: {e}");
        std::process::exit(1);
    });
    info!("Successfully loaded {} task(s)", tasks_list.tasks.len());
    println!("Successfully loaded {} task(s)", tasks_list.tasks.len());

    // Print a divider.
    print_divider();

    // Nothing to do when no tasks are defined.
    if tasks_list.tasks.is_empty() {
        warn!("No tasks are configured; nothing to run.");
        println!("No tasks to run.");
        return;
    }

    // Run the tasks or perform a dry run.
    match dry_run {
        true => {
            // Perform a dry run of every task, printing the results to the console.
            println!("Performing a dry run of every task...");
            tasks_list.dry_run(&mut std::io::stdout());
        }
        false => {
            // Run the tasks.
            println!("Running the tasks...");
            let progress = MultiProgress::new();
            let (handles, startup_errors) = tasks_list.run(&progress);
            let mut copy_errors = Vec::new();
            let mut thread_panics = Vec::new();
            let mut failed_files = Vec::new();

            // Wait for every task thread and tally the results.
            let mut succeeded = 0u32;
            let mut failed = 0u32;
            for handle in handles {
                match handle.join() {
                    Ok(Ok(failures)) => {
                        // The thread finished and the internal logic executed successfully.
                        failed_files.extend(failures);
                        succeeded += 1;
                    }
                    Ok(Err(e)) => {
                        // The thread finished but the internal logic failed.
                        copy_errors.push(e);
                    }
                    Err(e) => {
                        // The thread panicked.
                        thread_panics.push(e);
                    }
                }
            }

            // Handle task errors.
            for error in startup_errors {
                error!("Task error at startup: {error}");
                eprintln!("Task error at startup: {error}");
                failed += 1;
            }
            for error in copy_errors {
                error!("Task error during copying: {error}");
                eprintln!("Task error during copying: {error}");
                failed += 1;
            }
            for panic in thread_panics {
                error!("Thread panicked: {panic:?}");
                eprintln!("Thread panicked: {panic:?}");
                failed += 1;
            }

            // Files that failed to copy are reported individually, but the task that
            // owned them still counts as succeeded.
            for (failed_file, error_message) in failed_files {
                eprintln!("Failed to copy file \"{:?}\": {error_message}", failed_file)
            }

            // Clear the progress bars so the final summary prints on a clean line.
            let _ = progress.clear();

            // Print a divider.
            print_divider();

            info!("Run finished: {succeeded} succeeded, {failed} failed");
            println!("Run finished: {succeeded} succeeded, {failed} failed.");
        }
    }
}
