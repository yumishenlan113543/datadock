mod config;
mod core;

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use dialoguer::{Confirm, Input, Select};
use indicatif::MultiProgress;
use log::{error, info, warn};
use terminal_size::{Width, terminal_size};

use core::task::{Task, TasksList};

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

/// Command-line arguments for `datadock`.
#[derive(Parser, Debug)]
#[command(
    name = env!("CARGO_CRATE_NAME"),
    version,
    about = "Back up source directories to destination directories."
)]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Option<Commands>,
    /// List the files that would be copied without changing anything.
    #[arg(long)]
    dry_run: bool,
}

/// Subcommands for `datadock`.
#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage the task configuration.
    Config {
        /// The configuration action to perform.
        #[command(subcommand)]
        action: ConfigAction,
    },
}

/// Configuration actions for `datadock config`.
#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Add a task to the config file interactively.
    Add,
    /// Remove a task from the config file interactively.
    Remove,
}

/// Add a task to the config file interactively.
fn add_task_interactively(tasks_list: &mut TasksList) {
    // Ask user for the fields of a new task.
    let name: String = Input::new()
        .with_prompt("Enter the name of the new task")
        .interact_text()
        .unwrap();
    let src = PathBuf::from(
        Input::<String>::new()
            .with_prompt("Enter the source directory")
            .interact_text()
            .unwrap(),
    );
    let dst = PathBuf::from(
        Input::<String>::new()
            .with_prompt("Enter the destination directory")
            .interact_text()
            .unwrap(),
    );
    let scan_contents: bool = Confirm::new()
        .with_prompt("Scan contents of source directory?")
        .default(false)
        .interact()
        .unwrap();

    // Print a divider before confirming the task addition.
    print_divider();

    // Confirm with the user before adding the task to the list.
    if Confirm::new()
        .with_prompt(format!(
            "Adding task \"{}\" with source \"{}\" and destination \"{}\", {}. Continue?",
            name,
            src.display(),
            dst.display(),
            if scan_contents {
                "scanning contents"
            } else {
                "not scanning contents"
            }
        ))
        .default(false)
        .interact()
        .unwrap()
    {
        // Add the task directly to the list and persist it.
        tasks_list
            .tasks
            .push(Task::new(name, src, dst, scan_contents));
        if let Err(e) = tasks_list.save() {
            error!("Failed to add task: {e}");
            eprintln!("Error adding task: {e}");
            std::process::exit(1);
        } else {
            println!("Task added successfully.");
        }
    }
}

/// Remove a task from the config file interactively.
fn remove_task_interactively(tasks_list: &mut TasksList) {
    // Nothing to remove when no tasks are defined.
    if tasks_list.tasks.is_empty() {
        warn!("No tasks are configured; nothing to remove.");
        println!("No tasks to remove.");
        return;
    }

    // Label each task with its paths so same-named tasks stay distinguishable.
    let items: Vec<String> = tasks_list
        .tasks
        .iter()
        .map(|task| {
            format!(
                "{}    ({} -> {})",
                task.name(),
                task.src().display(),
                task.dst().display()
            )
        })
        .collect();

    // Let the user pick the task to remove.
    let selection = Select::new()
        .with_prompt("Select the task to remove")
        .items(&items)
        .default(0)
        .interact()
        .unwrap();

    // Confirm with the user before removing the task from the list.
    if Confirm::new()
        .with_prompt(format!("Remove task \"{}\"?", items[selection]))
        .default(false)
        .interact()
        .unwrap()
    {
        // Remove the task directly and persist the updated list.
        tasks_list.tasks.remove(selection);
        if let Err(e) = tasks_list.save() {
            error!("Failed to remove task: {e}");
            eprintln!("Error removing task: {e}");
            std::process::exit(1);
        } else {
            println!("Task removed successfully.");
        }
    }
}

/// Run the configured tasks or perform a dry run of them, logging progress and
/// results to the log file and printing a summary to the console.
fn run(tasks_list: &TasksList, dry_run: bool) {
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
            let scan_failures = tasks_list.dry_run(&mut std::io::stdout());
            if scan_failures > 0 {
                // A task whose scan failed was already reported; make the
                // failure visible to scripts and the caller as well.
                let _ = std::io::stdout().flush();
                std::process::exit(1);
            }
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
            let mut succeeded = 0usize;
            let mut failed: usize = startup_errors.len();
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
                        failed += 1;
                    }
                    Err(e) => {
                        // The thread panicked.
                        thread_panics.push(e);
                        failed += 1;
                    }
                }
            }
            let failed_file_count = failed_files.len();

            // Clear the progress bars so the final summary prints on a clean line.
            let _ = progress.clear();
            // Print a divider if there were any errors or failed files.
            if !startup_errors.is_empty()
                || !copy_errors.is_empty()
                || !thread_panics.is_empty()
                || !failed_files.is_empty()
            {
                print_divider()
            };

            // Handle task errors.
            for error in startup_errors {
                error!("Task error at startup: {error}");
                eprintln!("Task error at startup: {error}");
            }
            for error in copy_errors {
                error!("Task error during copying: {error}");
                eprintln!("Task error during copying: {error}");
            }
            for panic in thread_panics {
                error!("Thread panicked: {panic:?}");
                eprintln!("Thread panicked: {panic:?}");
            }
            for (failed_file, error_message) in failed_files {
                eprintln!(
                    "Failed to copy file \"{}\": {error_message}",
                    failed_file.display()
                )
            }

            // Print a divider and a summary of the run results.
            print_divider();
            info!(
                "Run finished: {succeeded} task(s) succeeded, \
                        {failed} task(s) failed, {failed_file_count} file(s) failed"
            );
            println!(
                "Run finished: {succeeded} task(s) succeeded, \
                        {failed} task(s) failed, {failed_file_count} file(s) failed."
            );

            // Any failed task or file must be visible to scripts and the caller,
            // even though the remaining tasks were still attempted.
            if failed > 0 || failed_file_count > 0 {
                let _ = std::io::stdout().flush();
                std::process::exit(1);
            }
        }
    }
}

/// Run a `datadock` command: either add or remove a task in the config file, or
/// load the configured tasks and run/dry-run them, logging progress and results
/// to the log file and printing a summary to the console.
fn main() {
    // Parse command-line arguments.
    let cli = Cli::parse();

    // Initialize the file logger.
    if let Err(e) = init_file_logger() {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

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

    // Handle the subcommand or run the tasks.
    match cli.command {
        // Add or remove tasks in the config file.
        Some(Commands::Config { action }) => match action {
            ConfigAction::Add => add_task_interactively(&mut tasks_list),
            ConfigAction::Remove => remove_task_interactively(&mut tasks_list),
        },
        // Run the configured tasks or perform a dry run of them if there is no subcommand specified.
        None => run(&tasks_list, cli.dry_run),
    }
}
