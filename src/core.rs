use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;

use log::{debug, info, warn};
use serde::Deserialize;

use crate::config::TasksList;

/// Schedule every task in `tasks_list` on its own background thread and return
/// the [`thread::JoinHandle`]s so the caller can wait for them to finish.
///
/// A task that fails to be scheduled (e.g. its source directory cannot be
/// scanned) is collected into the returned error list and skipped; the
/// remaining tasks still run.
pub fn run_tasks_list(
    tasks_list: &TasksList,
) -> (Vec<thread::JoinHandle<io::Result<()>>>, Vec<io::Error>) {
    let mut join_handles = Vec::new();
    let mut errors = Vec::new();

    for task in &tasks_list.tasks {
        info!("Starting task \"{}\"", task.name);
        match task.run() {
            Ok(join_handle) => {
                join_handles.push(join_handle);
                info!("Task \"{}\" scheduled on a background thread", task.name);
            }
            Err(e) => {
                warn!("Task \"{}\" failed to start: {e}", task.name);
                errors.push(e);
            }
        }
    }

    (join_handles, errors)
}

/// Recursively collect every regular file under `root` into a set of paths.
///
/// Symlinks are intentionally not followed (`follow_links(false)`) and are
/// skipped. Directories are descended into automatically by `WalkDir`, and any
/// other entry type (socket, FIFO, etc.) is also skipped.
fn collect_files(root: &Path) -> io::Result<HashSet<PathBuf>> {
    let mut files = HashSet::new();

    debug!("Collecting files under \"{}\"", root.display());

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

    debug!(
        "Collected {} file(s) under \"{}\"",
        files.len(),
        root.display()
    );

    Ok(files)
}

/// Rewrite every path in `files` to be relative to `root`, dropping any path
/// that is not under `root`. Used to compare two directory trees by filename
/// instead of by their (different) absolute paths.
fn relative_paths(root: &Path, files: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    files
        .iter()
        .filter_map(|path| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .collect()
}

/// Copy every file in `rel_paths` from under `src` to the matching location
/// under `dst`, preserving the relative directory structure.
///
/// Each entry in `rel_paths` is a path relative to `src`. Parent directories
/// are created as needed, and each file's target path is `dst` joined with its
/// relative path.
fn copy_file_list(src: &Path, dst: &Path, rel_paths: &[PathBuf]) -> io::Result<()> {
    info!(
        "Copying {} file(s) from \"{}\" to \"{}\"",
        rel_paths.len(),
        src.display(),
        dst.display()
    );

    for rel in rel_paths {
        // Source path = src / relative path, target = dst / relative path.
        let src_path = src.join(rel);
        let target = dst.join(rel);

        // Create parent directory (keep directory structure)
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }

        // Copy file
        debug!(
            "Copying \"{}\" -> \"{}\"",
            src_path.display(),
            target.display()
        );
        fs::copy(&src_path, &target)?;
    }

    info!("Finished copying to \"{}\"", dst.display());
    Ok(())
}

/// A single backup/copy task, deserialized directly from a `tasks.toml` entry.
///
/// This encapsulates the behavior of a single task (scanning and copying its
/// files), so it lives here in `core.rs` rather than with the config types in
/// `config.rs`.
#[derive(Debug, Deserialize)]
pub struct Task {
    /// Human-readable name of the task.
    name: String,
    /// Source path to copy from.
    src: PathBuf,
    /// Destination path to copy to.
    dst: PathBuf,
}

impl Task {
    /// Compute the files that exist under `src` but are missing under `dst`,
    /// then spawn a background thread to copy them.
    ///
    /// Returns the thread's [`thread::JoinHandle`], or an error if the source
    /// or destination directory cannot be scanned. It is normal for this to
    /// error when either `src` or `dst` does not exist, so such a task
    /// simply fails to start, not fails during running.
    fn run(&self) -> io::Result<thread::JoinHandle<io::Result<()>>> {
        // Get relative paths under `src` and `dst`.
        // Absolute paths under `src` and `dst` never match, so compare the two
        // trees by relative paths, i.e. by each file's path relative to its root.
        let src_rel = relative_paths(&self.src, &collect_files(&self.src)?);
        let dst_rel = relative_paths(&self.dst, &collect_files(&self.dst)?);

        // Get files to copy from `src_rel` and `dst_rel`.
        let files_to_copy: Vec<PathBuf> = (&src_rel - &dst_rel).into_iter().collect();
        if files_to_copy.is_empty() {
            warn!(
                "Task \"{}\": nothing to copy from \"{}\" (destination is up to date)",
                self.name,
                self.src.display()
            );
        } else {
            info!(
                "Task \"{}\": {} file(s) to copy from \"{}\" to \"{}\"",
                self.name,
                files_to_copy.len(),
                self.src.display(),
                self.dst.display()
            );
        }

        // Clone `src` and `dst`, so the copy thread can use them.
        let src = self.src.clone();
        let dst = self.dst.clone();

        Ok(thread::spawn(move || {
            copy_file_list(&src, &dst, &files_to_copy)
        }))
    }
}
