use std::fs;
use std::io;

use log::{debug, info, warn};
use serde::Deserialize;

use crate::core::Task;

/// The parsed top-level contents of the `tasks.toml` config file.
///
/// This only models the structure of the config file, so it lives here in
/// `config.rs` rather than with the task behavior in `core.rs`.
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
    pub fn new() -> io::Result<Self> {
        let mut list = Self::default();
        list.load()?;
        Ok(list)
    }
    /// Reload the tasks into `self` from the user's `tasks.toml` config file.
    ///
    /// The config file is created (empty) if it doesn't already exist, so a
    /// missing file is not an error. The current contents of `self` are
    /// replaced by whatever is loaded from the file.
    pub fn load(&mut self) -> io::Result<()> {
        let config_path = Self::config_path()?;
        self.load_from(&config_path)
    }
    /// Load tasks from the config file at `config_path`, replacing the current
    /// contents of `self`.
    ///
    /// The config file is created (empty) if it doesn't already exist, so a
    /// missing file is not an error.
    fn load_from(&mut self, config_path: &std::path::Path) -> io::Result<()> {
        debug!("Using config file at {}", config_path.display());

        // Ensure the config directory and file exist.
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !config_path.exists() {
            warn!(
                "Config file not found at \"{}\"; creating an empty one.",
                config_path.display()
            );
            fs::write(config_path, "")?;
        }

        // Read and parse the config file.
        let content = fs::read_to_string(config_path)?;
        let parsed: TasksList = toml::from_str(&content).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to parse {}: {e}", config_path.display()),
            )
        })?;

        // Reject invalid tasks (relative or nested/identical paths) before
        // committing them, so a bad config never replaces a previously-loaded
        // one.
        for task in &parsed.tasks {
            task._validate()?;
        }

        // Commit the parsed, validated tasks.
        *self = parsed;

        info!(
            "Loaded {} task(s) from \"{}\"",
            self.tasks.len(),
            config_path.display()
        );

        Ok(())
    }
    /// Resolve the platform-specific path to the `tasks.toml` config file.
    fn config_path() -> io::Result<std::path::PathBuf> {
        // Resolve the platform-specific config directory for this application.
        let dirs = directories::ProjectDirs::from("", "", env!("CARGO_CRATE_NAME")).ok_or(
            io::Error::new(io::ErrorKind::NotFound, "Can't find the project directory."),
        )?;
        Ok(dirs.config_dir().join("tasks.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    /// Build a unique, per-test directory under the system temp directory that
    /// is safe to write to and clean up afterwards.
    fn temp_dir(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datadock_test_{}_{}",
            std::process::id(),
            test_name
        ));
        // Start from a clean slate in case a previous run left files behind.
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn default_has_no_tasks() {
        let list = TasksList::default();
        assert!(list.tasks.is_empty());
    }

    #[test]
    fn deserializes_empty_toml() {
        let list: TasksList = toml::from_str("").unwrap();
        assert!(list.tasks.is_empty());
    }

    #[test]
    fn deserializes_missing_tasks_field() {
        let list: TasksList = toml::from_str("other = 42").unwrap();
        assert!(list.tasks.is_empty());
    }

    #[test]
    fn deserializes_multiple_tasks() {
        let toml = r#"
            [[tasks]]
            name = "photos"
            src = "/home/user/photos"
            dst = "/backup/photos"

            [[tasks]]
            name = "docs"
            src = "/home/user/docs"
            dst = "/backup/docs"
        "#;
        let list: TasksList = toml::from_str(toml).unwrap();
        assert_eq!(list.tasks.len(), 2);
    }

    #[test]
    fn load_from_creates_missing_file() {
        let dir = temp_dir("creates_missing_file");
        let config_path = dir.join("tasks.toml");

        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();

        assert!(config_path.exists());
        assert!(list.tasks.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_creates_parent_directories() {
        let dir = temp_dir("creates_parent_directories");
        let config_path = dir.join("nested").join("deep").join("tasks.toml");

        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();

        assert!(config_path.exists());
        assert!(list.tasks.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_reads_existing_tasks() {
        let dir = temp_dir("reads_existing_tasks");
        let config_path = dir.join("tasks.toml");

        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &config_path,
            r#"
                [[tasks]]
                name = "photos"
                src = "/home/user/photos"
                dst = "/backup/photos"
            "#,
        )
        .unwrap();

        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();

        assert_eq!(list.tasks.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_replaces_previous_contents() {
        let dir = temp_dir("replaces_previous_contents");
        let config_path = dir.join("tasks.toml");
        fs::create_dir_all(&dir).unwrap();

        fs::write(
            &config_path,
            r#"
                [[tasks]]
                name = "photos"
                src = "/a"
                dst = "/b"
            "#,
        )
        .unwrap();

        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();
        assert_eq!(list.tasks.len(), 1);

        // Overwrite the file with an empty config and reload; the previous
        // task list must be discarded.
        fs::write(&config_path, "").unwrap();
        list.load_from(&config_path).unwrap();
        assert!(list.tasks.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_errors_on_invalid_toml() {
        let dir = temp_dir("errors_on_invalid_toml");
        let config_path = dir.join("tasks.toml");

        fs::create_dir_all(&dir).unwrap();
        fs::write(&config_path, "not [ valid toml").unwrap();

        let mut list = TasksList::default();
        let result = list.load_from(&config_path);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_rejects_relative_path() {
        let dir = temp_dir("rejects_relative_path");
        let config_path = dir.join("tasks.toml");

        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &config_path,
            r#"
                [[tasks]]
                name = "photos"
                src = "photos"
                dst = "/backup/photos"
            "#,
        )
        .unwrap();

        let mut list = TasksList::default();
        let result = list.load_from(&config_path);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_rejects_nested_paths() {
        let dir = temp_dir("rejects_nested_paths");
        let config_path = dir.join("tasks.toml");

        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &config_path,
            r#"
                [[tasks]]
                name = "photos"
                src = "/home/user/photos"
                dst = "/home/user/photos/backup"
            "#,
        )
        .unwrap();

        let mut list = TasksList::default();
        let result = list.load_from(&config_path);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        fs::remove_dir_all(&dir).unwrap();
    }
}
