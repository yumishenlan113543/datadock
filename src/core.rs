use std::collections::HashSet;
use std::path::PathBuf;

use serde::Deserialize;

/// Recursively collect every regular file under `root` into a set of absolute paths.
///
/// Symlinks are intentionally not followed (`follow_links(false)`) and are skipped.
/// Any entry that is neither a file, a directory, nor a symlink (e.g. a socket or
/// FIFO) is treated as an unsupported type and aborts with an error.
fn collect_files(root: &PathBuf) -> std::io::Result<HashSet<PathBuf>> {
    let mut files = HashSet::new();

    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        match entry {
            // Regular file: record its path.
            f if f.file_type().is_file() => {
                files.insert(f.into_path());
            }
            // Directory: `WalkDir` already descends into it recursively, so there is nothing to do here.
            // Anything else (symlink, socket, device, etc.): skipped.
            _ => {}
        };
    }

    Ok(files)
}

/// A single backup/copy task, deserialized directly from a `tasks.toml` entry.
/// It's used for handling and executing a single task, so it's not placed in `config.rs`.
#[derive(Debug, Deserialize)]
pub struct Task {
    /// Human-readable name of the task.
    name: String,
    /// Source path to copy from.
    src: PathBuf,
    /// Destination path to copy to.
    dst: PathBuf,
}
