use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, info, warn};
use serde::Deserialize;
use tempfile::NamedTempFile;

use crate::config::TasksList;

/// The list of files that failed to copy, each paired with the error message
/// explaining why the copy failed.
type FailedFiles = Vec<(PathBuf, String)>;

/// Schedule every task in `tasks_list` on its own background thread and return
/// the [`thread::JoinHandle`]s so the caller can wait for them to finish.
///
/// Each handle resolves to a `(path, error message)` pair for each file that
/// failed to copy for that task. Each task registers its own [`ProgressBar`]
/// under `mp`, which the caller owns and uses to render the whole set of bars
/// together.
///
/// A task whose source or destination directory is missing is skipped (with a
/// message) rather than scheduled. A task that fails to be scheduled for any
/// other reason is collected into the returned error list. In both cases the
/// remaining tasks still run.
pub fn run_tasks_list(
    tasks_list: &TasksList,
    mp: &MultiProgress,
) -> (
    Vec<thread::JoinHandle<io::Result<FailedFiles>>>,
    Vec<io::Error>,
) {
    let mut join_handles = Vec::new();
    let mut errors = Vec::new();

    for task in &tasks_list.tasks {
        if task.directories_exist() {
            info!("Starting task \"{}\"", task.name);
            let _ = mp.println(format!("Starting task \"{}\"", task.name));
            match task.run(mp) {
                Ok(join_handle) => {
                    join_handles.push(join_handle);
                    info!("Task \"{}\" scheduled on a background thread", task.name);
                }
                Err(e) => {
                    warn!("Task \"{}\" failed to start: {e}", task.name);
                    errors.push(e);
                }
            }
        } else {
            info!(
                "At least one folder for task \"{}\" doesn't exist, skipping...",
                task.name
            );
            let _ = mp.println(format!(
                "At least one folder for task \"{}\" doesn't exist, skipping...",
                task.name
            ));
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

/// Copy every file in `files` from under `src` to the matching location under
/// `dst`, preserving the relative directory structure.
///
/// Each entry in `files` is a `(path, size)` pair where `path` is relative to
/// `src` and `size` is the file's length in bytes. Parent directories are
/// created as needed, and each file's target path is `dst` joined with its
/// relative path. Progress is reported to `bar` in bytes.
///
/// A file that fails to copy is skipped (its existing destination, if any, is
/// left intact) and its absolute source path and error message are collected;
/// the remaining files are still copied. The returned list holds a
/// `(path, error message)` pair for each failure, so a single copy error does
/// not abort the whole task.
fn copy_file_list(
    src: &Path,
    dst: &Path,
    files: &[(PathBuf, u64)],
    bar: &ProgressBar,
) -> io::Result<FailedFiles> {
    info!(
        "Copying {} file(s) from \"{}\" to \"{}\"",
        files.len(),
        src.display(),
        dst.display()
    );
    let mut failures = Vec::new();

    for (rel, _size) in files {
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
        bar.set_message(
            rel.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| rel.display().to_string()),
        );
        if let Err(error) = copy_file_stream(&src_path, &target, bar) {
            // The file failed to copy: record it and continue with the rest.
            // The copy is written to a temporary file and only moved into
            // place on success, so an existing destination is left intact.
            let abs = src.join(rel);
            warn!("Failed to copy \"{}\": {error}; skipping it", abs.display());
            // Push the failed file path and its error message to `failures`.
            failures.push((abs, error.to_string()));
        }
    }

    bar.finish();
    info!("Finished copying to \"{}\"", dst.display());
    Ok(failures)
}

/// Copy a single file from `src` to `dst` in fixed-size chunks, reporting each
/// chunk's byte count to `bar`.
///
/// Unlike `fs::copy`, this streams the file so progress can be measured, and it
/// preserves the source file's permission bits to match `fs::copy`'s behavior.
///
/// The copy is written to a temporary file in the destination directory and
/// atomically renamed into place only on success, so a failed copy never
/// truncates or corrupts an existing destination file.
fn copy_file_stream(src: &Path, dst: &Path, bar: &ProgressBar) -> io::Result<()> {
    let mut reader = BufReader::new(fs::File::open(src)?);

    // The temporary file lives next to `dst` (same directory) so the final
    // rename is atomic; it is removed automatically if anything goes wrong.
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    let mut buf = [0u8; 64 * 1024];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        tmp.write_all(&buf[..n])?;
        bar.inc(n as u64);
    }

    // `fs::copy` also copies the permission bits; mirror that here (on the
    // temporary file, before it is moved into place).
    fs::set_permissions(tmp.path(), fs::metadata(src)?.permissions())?;

    // Atomically replace `dst` with the fully-written temporary file.
    tmp.persist(dst).map_err(|e| e.error)?;

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
    /// Validate this task's configuration, returning an error when it is
    /// invalid.
    ///
    /// A task is rejected when either path is relative, or when the source and
    /// destination are identical or nested within one another (which would back
    /// a directory up into itself).
    pub(crate) fn _validate(&self) -> io::Result<()> {
        if !self.src.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Task \"{}\": source path \"{}\" must be absolute",
                    self.name,
                    self.src.display()
                ),
            ));
        }
        if !self.dst.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Task \"{}\": destination path \"{}\" must be absolute",
                    self.name,
                    self.dst.display()
                ),
            ));
        }

        // Resolve symlinks where possible (falling back to the already-absolute
        // path when it doesn't exist yet) so nesting through a symlink is also
        // caught.
        let src = fs::canonicalize(&self.src).unwrap_or_else(|_| self.src.clone());
        let dst = fs::canonicalize(&self.dst).unwrap_or_else(|_| self.dst.clone());
        if dst.starts_with(&src) || src.starts_with(&dst) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Task \"{}\": source \"{}\" and destination \"{}\" must not be identical or nested",
                    self.name,
                    self.src.display(),
                    self.dst.display()
                ),
            ));
        }

        Ok(())
    }
    /// Return whether both the source and destination directories exist on
    /// disk.
    ///
    /// A task is only run when both exist, so [`run_tasks_list`] skips a task
    /// whose directories are missing (e.g. a removable drive that isn't
    /// mounted) instead of scheduling it.
    fn directories_exist(&self) -> bool {
        self.src.exists() && self.dst.exists()
    }
    /// Compute the files under `src` that are missing from `dst` or whose
    /// `dst` differ from the corresponding files in `src`, then spawn a
    /// background thread to copy them while reporting progress to a bar
    /// registered under `mp`.
    ///
    /// Returns the thread's [`thread::JoinHandle`], which resolves to a
    /// `(path, error message)` pair for each file that failed to copy, or an
    /// error if the source or destination directory cannot be scanned. Callers
    /// normally check [`Task::directories_exist`] first, so this runs only when
    /// both directories are present.
    fn run(&self, mp: &MultiProgress) -> io::Result<thread::JoinHandle<io::Result<FailedFiles>>> {
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

        // Register a progress bar for this task and size it in bytes.
        let total_bytes: u64 = files_to_copy.iter().map(|(_, size)| *size).sum();
        let bar = mp.add(ProgressBar::new(total_bytes));
        bar.set_style(
            ProgressStyle::with_template(
                "{prefix} [{elapsed_precise}/{eta}] {bar:40.cyan/blue} {bytes}/{total_bytes} {msg}",
            )
            .expect("valid progress bar template"),
        );
        bar.set_prefix(self.name.clone());

        // Clone `src`, `dst` and the bar so the copy thread can use them.
        let src = self.src.clone();
        let dst = self.dst.clone();
        let bar = bar.clone();

        Ok(thread::spawn(move || {
            copy_file_list(&src, &dst, &files_to_copy, &bar)
        }))
    }
    /// Compute the relative paths and byte sizes of every source file that
    /// must be copied to make `dst` mirror `src`.
    ///
    /// A file is copied when it is missing under `dst`, or when its destination
    /// copy differs from the source (compared by size first, then by content).
    /// The source file always overwrites the destination. Files that exist only
    /// under `dst` are left untouched. Each returned entry is a `(relative
    /// path, size in bytes)` pair.
    fn compute_files_to_copy(&self) -> io::Result<Vec<(PathBuf, u64)>> {
        info!("Computing files in task \"{}\" to copy...", self.name);

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
            let should_copy = if !dst_files.contains(&dst_path) {
                debug!(
                    "Task \"{}\": \"{}\" is missing from \"{}\"",
                    self.name,
                    rel.display(),
                    self.dst.display()
                );
                true
            } else if files_differ(&src_path, &dst_path)? {
                debug!(
                    "Task \"{}\": \"{}\" differs from its copy in \"{}\"",
                    self.name,
                    rel.display(),
                    self.dst.display()
                );
                true
            } else {
                false
            };

            if should_copy {
                let size = fs::metadata(&src_path)?.len();
                to_copy.push((rel, size));
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

#[cfg(test)]
mod tests {
    use super::*;

    use indicatif::ProgressDrawTarget;

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

    /// Create a file at `path` (and its parent directories) with `contents`.
    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Build a [`Task`] pointing at the given source and destination paths.
    fn task(name: &str, src: &Path, dst: &Path) -> Task {
        Task {
            name: name.to_string(),
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
        }
    }

    /// A progress bar that never renders, keeping test output clean.
    fn hidden_bar() -> ProgressBar {
        ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden())
    }

    /// A [`MultiProgress`] whose bars never render, keeping test output clean.
    fn hidden_multi_progress() -> MultiProgress {
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
    }

    #[test]
    fn collect_files_returns_empty_for_empty_dir() {
        let dir = temp_dir("collect_empty");
        fs::create_dir_all(&dir).unwrap();

        let files = collect_files(&dir).unwrap();
        assert!(files.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn collect_files_collects_files_recursively() {
        let dir = temp_dir("collect_recursive");
        write_file(&dir.join("a.txt"), b"a");
        write_file(&dir.join("sub").join("b.txt"), b"b");
        write_file(&dir.join("sub").join("deep").join("c.txt"), b"c");

        let mut files: Vec<PathBuf> = collect_files(&dir).unwrap().into_iter().collect();
        files.sort();

        assert_eq!(
            files,
            vec![
                dir.join("a.txt"),
                dir.join("sub").join("b.txt"),
                dir.join("sub").join("deep").join("c.txt"),
            ]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn collect_files_errors_on_missing_root() {
        let dir = temp_dir("collect_missing");
        assert!(collect_files(&dir).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn collect_files_skips_symlinks() {
        let dir = temp_dir("collect_symlinks");
        write_file(&dir.join("real.txt"), b"real");
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();

        let files = collect_files(&dir).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files.contains(&dir.join("real.txt")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_differ_identical_contents_is_false() {
        let dir = temp_dir("differ_same");
        let a = dir.join("a");
        let b = dir.join("b");
        write_file(&a, b"same content");
        write_file(&b, b"same content");

        assert!(!files_differ(&a, &b).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_differ_empty_files_are_identical() {
        let dir = temp_dir("differ_empty");
        let a = dir.join("a");
        let b = dir.join("b");
        write_file(&a, b"");
        write_file(&b, b"");

        assert!(!files_differ(&a, &b).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_differ_different_size_is_true() {
        let dir = temp_dir("differ_size");
        let a = dir.join("a");
        let b = dir.join("b");
        write_file(&a, b"short");
        write_file(&b, b"a much longer content");

        assert!(files_differ(&a, &b).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_differ_different_content_same_size_is_true() {
        let dir = temp_dir("differ_content");
        let a = dir.join("a");
        let b = dir.join("b");
        write_file(&a, b"aaaa");
        write_file(&b, b"bbbb");

        assert!(files_differ(&a, &b).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_differ_large_files_differing_at_the_end() {
        let dir = temp_dir("differ_large");
        let a = dir.join("a");
        let b = dir.join("b");

        // Larger than the BufReader's default buffer, so the comparison loops
        // over multiple chunks; the files differ only in the last byte.
        let mut content_a = vec![b'x'; 64 * 1024];
        let mut content_b = content_a.clone();
        *content_a.last_mut().unwrap() = b'a';
        *content_b.last_mut().unwrap() = b'b';
        write_file(&a, &content_a);
        write_file(&b, &content_b);

        assert!(files_differ(&a, &b).unwrap());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_differ_errors_on_missing_file() {
        let dir = temp_dir("differ_missing");
        let a = dir.join("a");
        write_file(&a, b"x");

        assert!(files_differ(&a, &dir.join("nope")).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_file_stream_copies_contents() {
        let dir = temp_dir("copy_stream");
        let src = dir.join("src.txt");
        let dst = dir.join("dst.txt");
        write_file(&src, b"hello world");

        copy_file_stream(&src, &dst, &hidden_bar()).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"hello world");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_file_stream_copies_large_files() {
        let dir = temp_dir("copy_stream_large");
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");

        // Larger than the 64 KiB copy buffer, forcing several read/write loops.
        let content = vec![0xABu8; 128 * 1024];
        write_file(&src, &content);

        copy_file_stream(&src, &dst, &hidden_bar()).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), content);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_file_stream_errors_on_missing_source() {
        let dir = temp_dir("copy_stream_missing");
        fs::create_dir_all(&dir).unwrap();

        assert!(copy_file_stream(&dir.join("nope"), &dir.join("dst"), &hidden_bar()).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn copy_file_stream_preserves_existing_destination_on_failure() {
        let dir = temp_dir("copy_stream_preserve");
        let src = dir.join("src");
        let dst = dir.join("dst");
        // A directory passed as a "file" opens successfully on Unix but fails
        // on read (EISDIR), after the temporary file has been created.
        fs::create_dir_all(src.join("dir_a")).unwrap();
        // A pre-existing destination that must survive the failed copy intact.
        write_file(&dst, b"original");

        let bar = hidden_bar();
        let result = copy_file_stream(&src.join("dir_a"), &dst, &bar);

        assert!(result.is_err());
        assert_eq!(fs::read(&dst).unwrap(), b"original");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_file_list_preserves_relative_structure() {
        let dir = temp_dir("copy_list");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("a.txt"), b"a");
        write_file(&src.join("sub").join("b.txt"), b"b");

        let files = vec![
            (PathBuf::from("a.txt"), 1),
            (PathBuf::from("sub").join("b.txt"), 1),
        ];
        copy_file_list(&src, &dst, &files, &hidden_bar()).unwrap();

        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(dst.join("sub").join("b.txt")).unwrap(), b"b");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn copy_file_list_records_failed_files_and_continues() {
        let dir = temp_dir("copy_list_fail");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("good.txt"), b"ok");
        fs::create_dir_all(&dst).unwrap();

        // The second entry's source does not exist, so that copy fails, but the
        // remaining files are still copied and the bar is finished normally.
        let files = vec![
            (PathBuf::from("good.txt"), 2),
            (PathBuf::from("missing.txt"), 0),
        ];
        let bar = hidden_bar();
        let failures = copy_file_list(&src, &dst, &files, &bar).unwrap();

        // The good file is copied and the missing one is recorded as a failure
        // together with its error message.
        assert_eq!(fs::read(dst.join("good.txt")).unwrap(), b"ok");
        assert_eq!(failures.len(), 1);
        let (failed_path, error_message) = &failures[0];
        assert_eq!(failed_path, &src.join("missing.txt"));
        assert!(!error_message.is_empty());
        assert!(bar.is_finished());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn copy_file_list_preserves_destination_on_failed_copy() {
        let dir = temp_dir("copy_list_preserve");
        let src = dir.join("src");
        let dst = dir.join("dst");
        // A directory passed as a "file" opens successfully on Unix but fails
        // on read (EISDIR), after the temporary file has been created.
        fs::create_dir_all(src.join("dir_a")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        // A pre-existing destination that must survive the failed copy intact.
        write_file(&dst.join("dir_a"), b"original");

        let files = vec![(PathBuf::from("dir_a"), 0)];
        let bar = hidden_bar();
        let failures = copy_file_list(&src, &dst, &files, &bar).unwrap();

        // The failed copy is recorded (path and error message), and the
        // existing destination is intact.
        assert_eq!(failures.len(), 1);
        let (failed_path, error_message) = &failures[0];
        assert_eq!(failed_path, &src.join("dir_a"));
        assert!(!error_message.is_empty());
        assert_eq!(fs::read(dst.join("dir_a")).unwrap(), b"original");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compute_files_to_copy_copies_missing_files() {
        let dir = temp_dir("compute_missing");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("a.txt"), b"a");
        fs::create_dir_all(&dst).unwrap();

        let to_copy = task("t", &src, &dst).compute_files_to_copy().unwrap();

        assert_eq!(to_copy, vec![(PathBuf::from("a.txt"), 1)]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compute_files_to_copy_copies_changed_files() {
        let dir = temp_dir("compute_changed");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("changed.txt"), b"new content");
        write_file(&dst.join("changed.txt"), b"old");
        write_file(&src.join("same.txt"), b"identical");
        write_file(&dst.join("same.txt"), b"identical");

        let to_copy = task("t", &src, &dst).compute_files_to_copy().unwrap();

        assert_eq!(to_copy, vec![(PathBuf::from("changed.txt"), 11)]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compute_files_to_copy_ignores_dst_only_files() {
        let dir = temp_dir("compute_dst_only");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("a.txt"), b"a");
        write_file(&dst.join("a.txt"), b"a");
        write_file(&dst.join("extra.txt"), b"extra");

        let to_copy = task("t", &src, &dst).compute_files_to_copy().unwrap();

        assert!(to_copy.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compute_files_to_copy_errors_when_src_missing() {
        let dir = temp_dir("compute_src_missing");
        let dst = dir.join("dst");
        fs::create_dir_all(&dst).unwrap();

        assert!(
            task("t", &dir.join("src"), &dst)
                .compute_files_to_copy()
                .is_err()
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compute_files_to_copy_errors_when_dst_missing() {
        let dir = temp_dir("compute_dst_missing");
        let src = dir.join("src");
        write_file(&src.join("a.txt"), b"a");

        assert!(
            task("t", &src, &dir.join("dst"))
                .compute_files_to_copy()
                .is_err()
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validate_rejects_relative_src() {
        let t = task("t", Path::new("photos"), Path::new("/backup/photos"));
        assert!(t._validate().is_err());
    }

    #[test]
    fn validate_rejects_relative_dst() {
        let t = task("t", Path::new("/home/user/photos"), Path::new("backup"));
        assert!(t._validate().is_err());
    }

    #[test]
    fn validate_rejects_identical_paths() {
        let t = task(
            "t",
            Path::new("/home/user/photos"),
            Path::new("/home/user/photos"),
        );
        assert!(t._validate().is_err());
    }

    #[test]
    fn validate_rejects_dst_nested_in_src() {
        let t = task(
            "t",
            Path::new("/home/user/photos"),
            Path::new("/home/user/photos/backup"),
        );
        assert!(t._validate().is_err());
    }

    #[test]
    fn validate_rejects_src_nested_in_dst() {
        let t = task(
            "t",
            Path::new("/home/user/photos/sub"),
            Path::new("/home/user/photos"),
        );
        assert!(t._validate().is_err());
    }

    #[test]
    fn validate_accepts_sibling_paths() {
        let t = task(
            "t",
            Path::new("/home/user/photos"),
            Path::new("/backup/photos"),
        );
        assert!(t._validate().is_ok());
    }

    #[test]
    fn directories_exist_requires_both_directories() {
        let dir = temp_dir("dirs_exist");
        let src = dir.join("src");
        let dst = dir.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        // Neither exists.
        assert!(!task("t", &dir.join("other_src"), &dir.join("other_dst")).directories_exist());
        // Only `src` exists.
        assert!(!task("t", &src, &dir.join("other_dst")).directories_exist());
        // Only `dst` exists.
        assert!(!task("t", &dir.join("other_src"), &dst).directories_exist());
        // Both exist.
        assert!(task("t", &src, &dst).directories_exist());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_copies_files() {
        let dir = temp_dir("run_copies");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("a.txt"), b"hello");
        fs::create_dir_all(&dst).unwrap();

        let mp = hidden_multi_progress();
        let handle = task("t", &src, &dst).run(&mp).unwrap();
        handle.join().unwrap().unwrap();

        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_tasks_list_runs_all_tasks() {
        let dir = temp_dir("run_list");
        let src1 = dir.join("src1");
        let dst1 = dir.join("dst1");
        let src2 = dir.join("src2");
        let dst2 = dir.join("dst2");
        write_file(&src1.join("a.txt"), b"a");
        write_file(&src2.join("b.txt"), b"b");
        fs::create_dir_all(&dst1).unwrap();
        fs::create_dir_all(&dst2).unwrap();

        let tasks = TasksList {
            tasks: vec![task("t1", &src1, &dst1), task("t2", &src2, &dst2)],
        };

        let mp = hidden_multi_progress();
        let (handles, errors) = run_tasks_list(&tasks, &mp);

        assert!(errors.is_empty());
        assert_eq!(handles.len(), 2);
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        assert_eq!(fs::read(dst1.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(dst2.join("b.txt")).unwrap(), b"b");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_tasks_list_skips_tasks_with_missing_directories() {
        let dir = temp_dir("run_list_skip");
        fs::create_dir_all(&dir).unwrap();

        let tasks = TasksList {
            tasks: vec![task("t", &dir.join("missing"), &dir.join("dst"))],
        };

        let mp = hidden_multi_progress();
        let (handles, errors) = run_tasks_list(&tasks, &mp);

        // A task whose source directory is missing is skipped, not reported as
        // an error, so neither a handle nor a startup error is produced.
        assert!(handles.is_empty());
        assert!(errors.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_tasks_list_runs_remaining_tasks_when_one_is_skipped() {
        let dir = temp_dir("run_list_mixed");
        let good_src = dir.join("good_src");
        let good_dst = dir.join("good_dst");
        write_file(&good_src.join("a.txt"), b"a");
        fs::create_dir_all(&good_dst).unwrap();

        let tasks = TasksList {
            tasks: vec![
                // This task's source directory is missing, so it is skipped.
                task("skipped", &dir.join("missing"), &dir.join("dst")),
                task("good", &good_src, &good_dst),
            ],
        };

        let mp = hidden_multi_progress();
        let (handles, errors) = run_tasks_list(&tasks, &mp);

        // The skipped task produces no error, and the remaining task still runs.
        assert!(errors.is_empty());
        assert_eq!(handles.len(), 1);
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        assert_eq!(fs::read(good_dst.join("a.txt")).unwrap(), b"a");

        fs::remove_dir_all(&dir).unwrap();
    }
}
