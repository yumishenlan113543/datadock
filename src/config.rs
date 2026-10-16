use std::fs;

use log::{debug, info, warn};
use serde::Deserialize;

use crate::core::Task;

/// The parsed top-level contents of the `tasks.toml` config file.
#[derive(Debug, Default, Deserialize)]
pub struct TasksList {
    /// The list of tasks defined in the config file.
    ///
    /// Defaults to an empty list so a freshly created (empty) config
    /// file deserializes cleanly.
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl TasksList {
    /// Create a new [`TasksList`] and load it from the user's config file.
    ///
    /// The config file is created (empty) if it doesn't already exist, so a
    /// missing file is not an error. The returned `TasksList` may therefore
    /// hold an empty task list.
    pub fn new() -> std::io::Result<Self> {
        let mut list = Self::default();
        list.load()?;
        Ok(list)
    }

    /// Reload the tasks into `self` from the user's `tasks.toml` config file.
    ///
    /// The config file is created (empty) if it doesn't already exist, so a
    /// missing file is not an error. The current contents of `self` are
    /// replaced by whatever is loaded from the file.
    pub fn load(&mut self) -> std::io::Result<()> {
        // Resolve the platform-specific config directory for this application.
        let dirs = directories::ProjectDirs::from("", "", env!("CARGO_CRATE_NAME")).ok_or(
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Can't find the project directory.",
            ),
        )?;
        let config_path = dirs.config_dir().join("tasks.toml");
        debug!("Using config file at {}", config_path.display());

        // Ensure the config directory and file exist.
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !config_path.exists() {
            warn!("Config file not found; creating an empty one.");
            fs::write(&config_path, "")?;
        }

        // Read and parse the config file, replacing the current contents.
        let content = fs::read_to_string(&config_path)?;
        *self = toml::from_str(&content).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to parse {}: {e}", config_path.display()),
            )
        })?;

        info!(
            "Loaded {} task(s) from {}",
            self.tasks.len(),
            config_path.display()
        );

        Ok(())
    }
}
