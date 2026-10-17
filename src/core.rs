use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader};
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

/// Compare `file_a` and `file_b`, returning `Ok(true)` when the files differ.
///
/// The comparison is done by file size first (a cheap check) and falls back to
/// comparing the file contents in fixed-size chunks only when the sizes are
/// equal, so large files are not loaded fully into memory.
fn files_differ(file_a: &Path, file_b: &Path) -> io::Result<bool> {
    // Compare sizes first: a cheap way to reject most changed files without
    // reading their contents.
    if fs::metadata(file_a)?.len() != fs::metadata(file_b)?.len() {
        return Ok(true);
    }

    // Sizes are equal; compare the contents chunk by chunk.
    let mut a = BufReader::new(fs::File::open(file_a)?);
    let mut b = BufReader::new(fs::File::open(file_b)?);

    loop {
        let buf_a = a.fill_buf()?;
        let buf_b = b.fill_buf()?;
        let n = buf_a.len().min(buf_b.len());

        // Both readers are exhausted (sizes were equal and the same number
        // of bytes were consumed from each), so the files are identical.
        if n == 0 { 
            return Ok(false);
        }
        if buf_a[..n] != buf_b[..n] {
            return Ok(true);
        }

        a.consume(n);
        b.consume(n);
    }
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
    /// Compute the files under `src` that are missing from `dst` or whose
    /// `dst` differ from the corresponding files in `src`, then spawn
    /// a background thread to copy them.
    ///
    /// Returns the thread's [`thread::JoinHandle`], or an error if the source
    /// or destination directory cannot be scanned. It is normal for this to
    /// error when either `src` or `dst` does not exist, so such a task
    /// simply fails to start, not fails during running.
    fn run(&self) -> io::Result<thread::JoinHandle<io::Result<()>>> {
        // Determine which files to copy.
        let files_to_copy = self.compute_files_to_copy()?;
        if files_to_copy.is_empty() {
            info!(
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
    /// Compute the relative paths of every source file that must be copied to
    /// make `dst` mirror `src`.
    ///
    /// A file is copied when it is missing under `dst`, or when its destination
    /// copy differs from the source (compared by size first, then by content).
    /// The source file always overwrites the destination. Files that exist only
    /// under `dst` are left untouched.
    fn compute_files_to_copy(&self) -> io::Result<Vec<PathBuf>> {
        let src_files = collect_files(&self.src)?;
        let dst_files = collect_files(&self.dst)?;

        let mut to_copy = Vec::new();
        for src_path in src_files {
            // Every collected path is under `src`, so this cannot fail.
            let rel = src_path
                .strip_prefix(&self.src)
                .expect("collected path is not under the source root")
                .to_path_buf();
            let dst_path = self.dst.join(&rel);

            // Copy when the destination is missing the file, or when the
            // existing destination copy differs from the source (size, then
            // content).
            if !dst_files.contains(&dst_path) {
                debug!(
                    "Task \"{}\": \"{}\" is missing from \"{}\"",
                    self.name,
                    rel.display(),
                    self.dst.display()
                );
                to_copy.push(rel);
            } else if files_differ(&src_path, &dst_path)? {
                debug!(
                    "Task \"{}\": \"{}\" differs from its copy in \"{}\"",
                    self.name,
                    rel.display(),
                    self.dst.display()
                );
                to_copy.push(rel);
            }
        }

        debug!(
            "Task \"{}\": {} file(s) need to be copied",
            self.name,
            to_copy.len()
        );

        Ok(to_copy)
    }
}
