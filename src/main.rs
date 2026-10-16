mod config;
mod core;

use config::TasksList;
use log::{error, info};

fn main() {
    env_logger::init();
    
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
