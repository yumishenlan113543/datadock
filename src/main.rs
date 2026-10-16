mod config;
mod core;

use config::TasksList;
use log::{error, info};

fn init_file_logger() -> std::io::Result<()> {
    // Get the project directories
    let dirs = directories::ProjectDirs::from("", "", env!("CARGO_CRATE_NAME"))
        .ok_or_else(|| std::io::Error::other("Unable to determine the application directory."))?;
    // Get the data directory to store log
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

    info!("File logger initialized.");
    Ok(())
}

fn main() {
    // Initialize the logger
    init_file_logger().unwrap();

    match TasksList::new() {
        Ok(tasks_list) => {
            info!("Successfully loaded {} task(s)", tasks_list.tasks.len());
            println!("{:#?}", tasks_list);
        }
        Err(err) => {
            error!("Failed to load tasks: {err}");
            std::process::exit(1);
        }
    }
}
