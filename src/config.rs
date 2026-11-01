use std::fs;
use std::io::{self, Write};
use std::path::Path;

use log::{debug, info, warn};
use tempfile::NamedTempFile;

use crate::core::task::TasksList;

impl TasksList {
    /// Reload the tasks from the user's `tasks.toml` config file, replacing the
    /// current contents.
    ///
    /// A missing config file is created empty and is not an error.
    pub fn load(&mut self) -> io::Result<()> {
        let config_path = Self::default_config_path()?;
        self.load_from(&config_path)
    }
    /// Write the current task list to the user's `tasks.toml` config file.
    ///
    /// The whole list is serialized as TOML and written atomically, replacing
    /// the file's contents.
    pub fn save(&self) -> io::Result<()> {
        let config_path = Self::default_config_path()?;
        self.save_to(&config_path)
    }
    /// Load tasks from `config_path`, replacing the current contents.
    ///
    /// A missing config file is created empty and is not an error.
    fn load_from(&mut self, config_path: &Path) -> io::Result<()> {
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
            task.validate()?;
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
    /// Write the current task list to `config_path` as TOML.
    ///
    /// Missing parent directories are created, so this also works before the
    /// config file has ever been loaded. The file is written atomically so a
    /// failed write never truncates the existing config.
    fn save_to(&self, config_path: &Path) -> io::Result<()> {
        // Refuse to persist an invalid list so a bad in-memory state cannot
        // corrupt the config file.
        for task in &self.tasks {
            task.validate()?;
        }

        // Serialize first so a serialization failure leaves the file untouched.
        let content = toml::to_string_pretty(self).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to serialize {}: {e}", config_path.display()),
            )
        })?;

        // Ensure the config directory exists so saving also works before the
        // file has ever been loaded.
        let parent = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;

        // Write to a temporary file next to the config and atomically replace
        // it, so a partial write cannot corrupt the existing config.
        let mut tmp = NamedTempFile::new_in(parent)?;
        tmp.write_all(content.as_bytes())?;
        tmp.persist(config_path).map_err(|e| e.error)?;

        debug!(
            "Saved {} task(s) to \"{}\"",
            self.tasks.len(),
            config_path.display()
        );

        Ok(())
    }
    /// Resolve the platform-specific path to the `tasks.toml` config file.
    fn default_config_path() -> io::Result<std::path::PathBuf> {
        // Resolve the platform-specific config directory for this application.
        let dirs = directories::ProjectDirs::from("", "", env!("CARGO_CRATE_NAME")).ok_or(
            io::Error::new(io::ErrorKind::NotFound, "Can't find the project directory."),
        )?;
        Ok(dirs.config_dir().join("tasks.toml"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::core::task::Task;

    /// Build a unique, per-test temp directory that is safe to write to and
    /// clean up afterwards.
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

    /// Render `path` for embedding in a TOML basic string.
    ///
    /// Windows paths contain backslashes, which are escape characters in TOML,
    /// so use forward slashes instead; `PathBuf` accepts them on every
    /// platform.
    fn toml_path(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "/")
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
            scan_contents = true

            [[tasks]]
            name = "docs"
            src = "/home/user/docs"
            dst = "/backup/docs"
            scan_contents = false
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
    fn save_creates_parent_directories_and_round_trips() {
        let dir = temp_dir("save_creates_parents");
        let config_path = dir.join("nested").join("deep").join("tasks.toml");
        let src = dir.join("photos");
        let dst = dir.join("backup").join("photos");

        let mut list = TasksList::default();
        list.tasks
            .push(Task::new("photos".into(), src.clone(), dst.clone(), true));
        list.save_to(&config_path).unwrap();

        assert!(config_path.exists());
        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("name = \"photos\""));
        assert!(content.contains("scan_contents = true"));

        // The written file must load back into an equivalent list.
        let mut reloaded = TasksList::default();
        reloaded.load_from(&config_path).unwrap();
        assert_eq!(reloaded.tasks.len(), 1);
        assert_eq!(reloaded.tasks[0].name(), "photos");
        assert_eq!(reloaded.tasks[0].src(), src.as_path());
        assert_eq!(reloaded.tasks[0].dst(), dst.as_path());
        assert_eq!(
            toml::to_string_pretty(&reloaded).unwrap(),
            toml::to_string_pretty(&list).unwrap()
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_overwrites_existing_file_and_saves_empty_list() {
        let dir = temp_dir("save_overwrites");
        let config_path = dir.join("tasks.toml");

        fs::create_dir_all(&dir).unwrap();
        fs::write(&config_path, "not [ valid toml").unwrap();

        let list = TasksList::default();
        list.save_to(&config_path).unwrap();

        // An empty list must load cleanly after replacing the invalid file.
        let mut reloaded = TasksList::default();
        reloaded.load_from(&config_path).unwrap();
        assert!(reloaded.tasks.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_rejects_invalid_task_without_writing() {
        let dir = temp_dir("save_rejects_invalid");
        let config_path = dir.join("tasks.toml");

        fs::create_dir_all(&dir).unwrap();
        let original = "# untouched\n";
        fs::write(&config_path, original).unwrap();

        let mut list = TasksList::default();
        list.tasks.push(Task::new(
            "photos".into(),
            PathBuf::from("relative/photos"),
            dir.join("backup"),
            true,
        ));

        let result = list.save_to(&config_path);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_to_string(&config_path).unwrap(), original);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_from_reads_existing_tasks() {
        let dir = temp_dir("reads_existing_tasks");
        let config_path = dir.join("tasks.toml");

        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &config_path,
            format!(
                r#"
                [[tasks]]
                name = "photos"
                src = "{}"
                dst = "{}"
                scan_contents = true
            "#,
                toml_path(&dir.join("photos")),
                toml_path(&dir.join("backup").join("photos")),
            ),
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
            format!(
                r#"
                [[tasks]]
                name = "photos"
                src = "{}"
                dst = "{}"
                scan_contents = true
            "#,
                toml_path(&dir.join("a")),
                toml_path(&dir.join("b")),
            ),
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
    fn save_writes_new_task() {
        let dir = temp_dir("save_new_task");
        let config_path = dir.join("tasks.toml");
        let src = dir.join("photos");
        let dst = dir.join("backup").join("photos");

        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();
        list.tasks
            .push(Task::new("photos".into(), src.clone(), dst.clone(), true));
        list.save_to(&config_path).unwrap();

        assert_eq!(list.tasks.len(), 1);

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("name = \"photos\""));
        assert!(content.contains("scan_contents = true"));

        // The written file must remain loadable.
        let mut reloaded = TasksList::default();
        reloaded.load_from(&config_path).unwrap();
        assert_eq!(reloaded.tasks.len(), 1);
        assert_eq!(reloaded.tasks[0].src(), src.as_path());
        assert_eq!(reloaded.tasks[0].dst(), dst.as_path());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_preserves_existing_tasks_when_adding() {
        let dir = temp_dir("save_preserves_existing");
        let config_path = dir.join("tasks.toml");
        fs::create_dir_all(&dir).unwrap();

        let existing_src = dir.join("docs");
        let existing_dst = dir.join("backup").join("docs");
        fs::write(
            &config_path,
            format!(
                r#"
                [[tasks]]
                name = "docs"
                src = "{}"
                dst = "{}"
                scan_contents = true
            "#,
                toml_path(&existing_src),
                toml_path(&existing_dst),
            ),
        )
        .unwrap();

        let src = dir.join("photos");
        let dst = dir.join("backup").join("photos");
        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();
        list.tasks.push(Task::new("photos".into(), src, dst, false));
        list.save_to(&config_path).unwrap();

        assert_eq!(list.tasks.len(), 2);

        let mut reloaded = TasksList::default();
        reloaded.load_from(&config_path).unwrap();
        assert_eq!(reloaded.tasks.len(), 2);

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("name = \"docs\""));
        assert!(content.contains("name = \"photos\""));
        assert!(content.contains("scan_contents = false"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_after_removing_task_keeps_other_tasks() {
        let dir = temp_dir("remove_only_selected");
        let config_path = dir.join("tasks.toml");
        fs::create_dir_all(&dir).unwrap();

        let first_src = dir.join("photos");
        let first_dst = dir.join("backup").join("photos");
        let second_src = dir.join("docs");
        let second_dst = dir.join("backup").join("docs");
        fs::write(
            &config_path,
            format!(
                r#"
                [[tasks]]
                name = "photos"
                src = "{}"
                dst = "{}"
                scan_contents = true

                [[tasks]]
                name = "docs"
                src = "{}"
                dst = "{}"
                scan_contents = false
            "#,
                toml_path(&first_src),
                toml_path(&first_dst),
                toml_path(&second_src),
                toml_path(&second_dst),
            ),
        )
        .unwrap();

        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();
        list.tasks.remove(0);
        list.save_to(&config_path).unwrap();

        assert_eq!(list.tasks.len(), 1);
        assert_eq!(list.tasks[0].name(), "docs");

        // The written file must remain loadable and contain only the survivor.
        let mut reloaded = TasksList::default();
        reloaded.load_from(&config_path).unwrap();
        assert_eq!(reloaded.tasks.len(), 1);
        assert_eq!(reloaded.tasks[0].name(), "docs");

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(!content.contains("name = \"photos\""));
        assert!(content.contains("name = \"docs\""));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_after_removing_last_task_writes_empty_list() {
        let dir = temp_dir("remove_last");
        let config_path = dir.join("tasks.toml");
        let src = dir.join("photos");
        let dst = dir.join("backup").join("photos");

        let mut list = TasksList::default();
        list.load_from(&config_path).unwrap();
        list.tasks
            .push(Task::new("photos".into(), src.clone(), dst.clone(), true));
        list.save_to(&config_path).unwrap();
        list.tasks.remove(0);
        list.save_to(&config_path).unwrap();

        assert!(list.tasks.is_empty());

        // An empty task list must still load cleanly.
        let mut reloaded = TasksList::default();
        reloaded.load_from(&config_path).unwrap();
        assert!(reloaded.tasks.is_empty());

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
    fn load_from_rejects_missing_scan_contents() {
        let dir = temp_dir("rejects_missing_scan_contents");
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
            format!(
                r#"
                [[tasks]]
                name = "photos"
                src = "photos"
                dst = "{}"
                scan_contents = true
            "#,
                toml_path(&dir.join("backup").join("photos")),
            ),
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
            format!(
                r#"
                [[tasks]]
                name = "photos"
                src = "{}"
                dst = "{}"
                scan_contents = true
            "#,
                toml_path(&dir.join("photos")),
                toml_path(&dir.join("photos").join("backup")),
            ),
        )
        .unwrap();

        let mut list = TasksList::default();
        let result = list.load_from(&config_path);

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_persist_failure_leaves_destination_unchanged() {
        let dir = temp_dir("save_persist_failure");
        let config_path = dir.join("tasks.toml");

        // Make the destination a directory so the atomic replace cannot
        // overwrite it with the config file.
        fs::create_dir_all(&config_path).unwrap();

        let list = TasksList::default();
        let result = list.save_to(&config_path);

        assert!(result.is_err());
        assert!(config_path.is_dir());

        fs::remove_dir_all(&dir).unwrap();
    }
}
