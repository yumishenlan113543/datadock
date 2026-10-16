use std::path::PathBuf;

use serde::Deserialize;

/// A single backup/copy task, deserialized directly from a `tasks.toml` entry.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct Task {
    /// Human-readable name of the task.
    name: String,
    /// Source path to copy from.
    src: PathBuf,
    /// Destination path to copy to.
    dst: PathBuf,
}
