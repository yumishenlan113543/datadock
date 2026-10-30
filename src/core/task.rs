use std::fs;
use std::io::{self, Write};
use std::path::{Component, PathBuf};
use std::thread;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{debug, info, warn};
use serde::Deserialize;

use super::file_utils::{FailedFiles, collect_files, copy_file_list, files_differ};

type CopyThreadJoinHandle = thread::JoinHandle<io::Result<FailedFiles>>;

/// A single backup/copy task, deserialized from a `tasks.toml` entry.
///
/// Task behavior (scanning and copying files) lives here in `core::task`;
/// `config.rs` only handles loading the list from disk.
#[derive(Debug, Deserialize)]
pub struct Task {
    /// Human-readable name of the task.
    name: String,
    /// Source path to copy from.
    src: PathBuf,
    /// Destination path to copy to.
    dst: PathBuf,
    /// Whether to compare file contents when deciding what to copy.
    ///
    /// When `true`, files whose contents differ are copied over the destination.
    /// When `false`, only file names are considered, so differing files are left
    /// untouched.
    scan_contents: bool,
}

/// The parsed top-level contents of the `tasks.toml` config file.
///
/// This models the on-disk structure and schedules the tasks it contains;
/// creating, loading, and validating the file itself lives in `config.rs`.
#[derive(Debug, Default, Deserialize)]
pub struct TasksList {
    /// The tasks defined in the config file. Defaults to an empty list, so an
    /// empty config file deserializes cleanly.
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl Task {
    /// Validate this task's configuration, returning an error when it is
    /// invalid.
    ///
    /// A task is rejected when either path is relative or contains a `..`
    /// component, or when the source and destination are identical or nested
    /// (which would back a directory up into itself).
    pub fn validate(&self) -> io::Result<()> {
        // Reject relative paths, which are ambiguous about the intended root.
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

        // Reject `..` components before canonicalizing: a path such as
        // `/a/b/../c` is ambiguous about which directory is meant.
        if self.src.components().any(|c| c == Component::ParentDir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Task \"{}\": source path \"{}\" must not contain \"..\"",
                    self.name,
                    self.src.display()
                ),
            ));
        }
        if self.dst.components().any(|c| c == Component::ParentDir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Task \"{}\": destination path \"{}\" must not contain \"..\"",
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
    /// Return whether both the source and destination directories exist.
    ///
    /// [`TasksList::run`] skips a task whose directories are missing (e.g. an
    /// unmounted removable drive) instead of scheduling it.
    fn directories_exist(&self) -> bool {
        self.src.exists() && self.dst.exists()
    }
    /// Compute the relative paths and byte sizes of every source file that must
    /// be copied to make `dst` mirror `src`.
    ///
    /// A file is copied when it is missing under `dst`, or, when `scan_contents`
    /// is `true`, when its destination copy differs (compared by size, then
    /// content). Files that exist only under `dst` are left untouched. Each
    /// entry is a `(relative path, size in bytes)` pair.
    ///
    /// Files are discovered with [`collect_files`], so hidden and ignored files
    /// are not considered. A missing `src` or `dst` contributes an empty set
    /// rather than an error.
    fn compute_files_to_copy(&self) -> io::Result<Vec<(PathBuf, u64)>> {
        info!("Computing files in task \"{}\" to copy...", self.name);

        let src_files = collect_files(&self.src);
        let dst_files = collect_files(&self.dst);

        let mut to_copy = Vec::new();
        for src_path in src_files {
            // Every collected path is under `src`, so this cannot fail.
            let rel = src_path
                .strip_prefix(&self.src)
                .expect("collected path is not under the source root")
                .to_path_buf();
            let dst_path = self.dst.join(&rel);

            // Copy when the destination is missing the file, or (if content
            // scanning is enabled) when the existing destination copy differs
            // from the source (size, then content).
            let should_copy = if !dst_files.contains(&dst_path) {
                debug!(
                    "Task \"{}\": \"{}\" is missing from \"{}\"",
                    self.name,
                    rel.display(),
                    self.dst.display()
                );
                true
            } else if self.scan_contents && files_differ(&src_path, &dst_path)? {
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
    /// Compute the files to copy, then spawn a background thread that copies
    /// them while reporting progress to a bar registered under `mp`.
    ///
    /// Returns the thread's [`thread::JoinHandle`], which resolves to a
    /// `(path, error message)` pair for each failed file, or an error if a
    /// source file cannot be inspected while building the list.
    fn run(&self, mp: &MultiProgress) -> io::Result<CopyThreadJoinHandle> {
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
    /// Perform a dry run of this task, returning the absolute paths of every source.
    fn dry_run(&self) -> io::Result<Vec<PathBuf>> {
        // Determine which files to copy.
        let files_to_copy = self.compute_files_to_copy()?;
        let paths: Vec<PathBuf> = files_to_copy
            .into_iter()
            .map(|(path, _size)| self.src.join(path))
            .collect();

        Ok(paths)
    }
}

impl TasksList {
    /// Schedule every task on its own background thread and return the
    /// [`thread::JoinHandle`]s so the caller can wait for them to finish.
    ///
    /// Each handle resolves to a `(path, error message)` pair for each file that
    /// failed to copy. Each task registers its own [`ProgressBar`] under `mp`, which
    /// the caller uses to render all bars together.
    ///
    /// A task whose source or destination directory is missing is skipped with a
    /// message; any other scheduling failure is collected into the returned error
    /// list. The remaining tasks still run.
    pub fn run(&self, mp: &MultiProgress) -> (Vec<CopyThreadJoinHandle>, Vec<io::Error>) {
        let mut join_handles = Vec::new();
        let mut errors = Vec::new();

        for task in &self.tasks {
            if task.directories_exist() {
                info!("Starting task \"{}\"...", task.name);
                let _ = mp.println(format!("Starting task \"{}\"", task.name));
                match task.run(mp) {
                    Ok(join_handle) => {
                        info!("Task \"{}\" scheduled on a background thread", task.name);
                        join_handles.push(join_handle);
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
    /// Perform a dry run of every task in `self`, printing the results to `out`.
    pub fn dry_run(&self, out: &mut impl Write) {
        for task in &self.tasks {
            if task.directories_exist() {
                info!("Dry run for task \"{}\"...", task.name);
                writeln!(out, "Dry run for task \"{}\"...", task.name).unwrap();
                match task.dry_run() {
                    Ok(paths) => {
                        info!(
                            "Task \"{}\" dry run found {} file(s) to copy",
                            task.name,
                            paths.len()
                        );
                        writeln!(
                            out,
                            "Task \"{}\" dry run found {} file(s) to copy:",
                            task.name,
                            paths.len()
                        )
                        .unwrap();
                        for path in paths {
                            writeln!(out, "    \"{}\"", path.display()).unwrap();
                        }
                    }
                    Err(e) => {
                        warn!("Task \"{}\" dry run failed: {e}", task.name);
                        writeln!(out, "Task \"{}\" dry run failed: {e}", task.name).unwrap();
                    }
                }
            } else {
                info!(
                    "At least one folder for task \"{}\" doesn't exist, skipping dry run...",
                    task.name
                );
                writeln!(
                    out,
                    "At least one folder for task \"{}\" doesn't exist, skipping dry run...",
                    task.name
                )
                .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use indicatif::ProgressDrawTarget;

    use super::*;

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

    /// Build a [`Task`] pointing at the given source and destination paths,
    /// with content scanning enabled.
    fn task(name: &str, src: &Path, dst: &Path) -> Task {
        task_with_scan_contents(name, src, dst, true)
    }

    /// Build a [`Task`] pointing at the given source and destination paths,
    /// with content scanning set explicitly.
    fn task_with_scan_contents(name: &str, src: &Path, dst: &Path, scan_contents: bool) -> Task {
        Task {
            name: name.to_string(),
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            scan_contents,
        }
    }

    /// A [`MultiProgress`] whose bars never render, keeping test output clean.
    fn hidden_multi_progress() -> MultiProgress {
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
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
    fn compute_files_to_copy_name_only_skips_changed_files() {
        let dir = temp_dir("compute_name_only");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("changed.txt"), b"new content");
        write_file(&dst.join("changed.txt"), b"old");
        write_file(&src.join("missing.txt"), b"missing");
        write_file(&src.join("same.txt"), b"identical");
        write_file(&dst.join("same.txt"), b"identical");

        let to_copy = task_with_scan_contents("t", &src, &dst, false)
            .compute_files_to_copy()
            .unwrap();

        // With content scanning disabled, only the missing file is copied; the
        // existing file with different contents is left untouched.
        assert_eq!(to_copy, vec![(PathBuf::from("missing.txt"), 7)]);

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
    fn compute_files_to_copy_returns_empty_when_src_missing() {
        let dir = temp_dir("compute_src_missing");
        let dst = dir.join("dst");
        fs::create_dir_all(&dst).unwrap();

        let to_copy = task("t", &dir.join("src"), &dst)
            .compute_files_to_copy()
            .unwrap();
        assert!(to_copy.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compute_files_to_copy_treats_missing_dst_as_empty() {
        let dir = temp_dir("compute_dst_missing");
        let src = dir.join("src");
        write_file(&src.join("a.txt"), b"a");

        // A missing destination contributes no files, so every source file is
        // treated as missing (tasks are not run at all unless both directories
        // exist; see `directories_exist`).
        let to_copy = task("t", &src, &dir.join("dst"))
            .compute_files_to_copy()
            .unwrap();
        assert_eq!(to_copy, vec![(PathBuf::from("a.txt"), 1)]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compute_files_to_copy_skips_hidden_and_ignored_files() {
        let dir = temp_dir("compute_ignored");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join(".ignore"), b"ignored.txt\n");
        write_file(&src.join("visible.txt"), b"visible");
        write_file(&src.join("ignored.txt"), b"ignored");
        write_file(&src.join(".hidden.txt"), b"hidden");
        fs::create_dir_all(&dst).unwrap();

        let to_copy = task("t", &src, &dst).compute_files_to_copy().unwrap();

        assert_eq!(to_copy, vec![(PathBuf::from("visible.txt"), 7)]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validate_rejects_relative_src() {
        let root = temp_dir("validate_relative_src");
        let t = task(
            "t",
            Path::new("photos"),
            &root.join("backup").join("photos"),
        );
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_rejects_relative_dst() {
        let root = temp_dir("validate_relative_dst");
        let t = task(
            "t",
            &root.join("home").join("user").join("photos"),
            Path::new("backup"),
        );
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_rejects_src_with_parent_dir() {
        let root = temp_dir("validate_src_parent_dir");
        let t = task(
            "t",
            &root
                .join("home")
                .join("user")
                .join("photos")
                .join("..")
                .join("other"),
            &root.join("backup").join("photos"),
        );
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_rejects_dst_with_parent_dir() {
        let root = temp_dir("validate_dst_parent_dir");
        let t = task(
            "t",
            &root.join("home").join("user").join("photos"),
            &root.join("backup").join("..").join("photos"),
        );
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_rejects_identical_paths() {
        let root = temp_dir("validate_identical");
        let path = root.join("home").join("user").join("photos");
        let t = task("t", &path, &path);
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_rejects_dst_nested_in_src() {
        let root = temp_dir("validate_dst_nested");
        let src = root.join("home").join("user").join("photos");
        let t = task("t", &src, &src.join("backup"));
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_rejects_src_nested_in_dst() {
        let root = temp_dir("validate_src_nested");
        let dst = root.join("home").join("user").join("photos");
        let t = task("t", &dst.join("sub"), &dst);
        assert!(t.validate().is_err());
    }

    #[test]
    fn validate_accepts_sibling_paths() {
        let root = temp_dir("validate_siblings");
        let t = task(
            "t",
            &root.join("home").join("user").join("photos"),
            &root.join("backup").join("photos"),
        );
        assert!(t.validate().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn validate_rejects_nesting_through_symlink() {
        let dir = temp_dir("validate_symlink");
        let src = dir.join("src");
        let link = dir.join("link");
        fs::create_dir_all(src.join("backup")).unwrap();
        std::os::unix::fs::symlink(&src, &link).unwrap();

        // `link` points at `src`, so `link/backup` is nested inside `src` even
        // though the paths do not look related before canonicalization.
        let t = task("t", &src, &link.join("backup"));
        assert!(t.validate().is_err());

        fs::remove_dir_all(&dir).unwrap();
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
    fn tasks_list_run_runs_all_tasks() {
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
        let (handles, errors) = tasks.run(&mp);

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
    fn tasks_list_run_skips_tasks_with_missing_directories() {
        let dir = temp_dir("run_list_skip");
        fs::create_dir_all(&dir).unwrap();

        let tasks = TasksList {
            tasks: vec![task("t", &dir.join("missing"), &dir.join("dst"))],
        };

        let mp = hidden_multi_progress();
        let (handles, errors) = tasks.run(&mp);

        // A task whose source directory is missing is skipped, not reported as
        // an error, so neither a handle nor a startup error is produced.
        assert!(handles.is_empty());
        assert!(errors.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tasks_list_run_runs_remaining_tasks_when_one_is_skipped() {
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
        let (handles, errors) = tasks.run(&mp);

        // The skipped task produces no error, and the remaining task still runs.
        assert!(errors.is_empty());
        assert_eq!(handles.len(), 1);
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        assert_eq!(fs::read(good_dst.join("a.txt")).unwrap(), b"a");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dry_run_returns_absolute_paths_of_files_to_copy() {
        let dir = temp_dir("dry_run_files");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("missing.txt"), b"new");
        write_file(&src.join("same.txt"), b"same");
        write_file(&dst.join("same.txt"), b"same");
        write_file(&dst.join("extra.txt"), b"extra");

        let paths = task("t", &src, &dst).dry_run().unwrap();

        // Only files that would be copied are reported, as absolute source
        // paths. Files that are already up to date and destination-only files
        // are omitted.
        assert_eq!(paths, vec![src.join("missing.txt")]);
        // A dry run must not modify the destination.
        assert!(!dst.join("missing.txt").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dry_run_honors_scan_contents() {
        let dir = temp_dir("dry_run_scan_contents");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("changed.txt"), b"new content");
        write_file(&dst.join("changed.txt"), b"old");

        // With content scanning disabled, the differing file is left alone, so
        // there is nothing for the dry run to report.
        let paths = task_with_scan_contents("t", &src, &dst, false)
            .dry_run()
            .unwrap();
        assert!(paths.is_empty());

        // With content scanning enabled, the differing file is reported.
        let paths = task("t", &src, &dst).dry_run().unwrap();
        assert_eq!(paths, vec![src.join("changed.txt")]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tasks_list_dry_run_reports_files_to_copy() {
        let dir = temp_dir("dry_run_list");
        let src = dir.join("src");
        let dst = dir.join("dst");
        write_file(&src.join("a.txt"), b"a");
        fs::create_dir_all(&dst).unwrap();

        let tasks = TasksList {
            tasks: vec![task("t", &src, &dst)],
        };

        let mut out = Vec::new();
        tasks.dry_run(&mut out);
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("Dry run for task \"t\"..."));
        assert!(output.contains("Task \"t\" dry run found 1 file(s) to copy:"));
        assert!(output.contains(&src.join("a.txt").display().to_string()));
        // The dry run only prints; nothing is copied.
        assert!(!dst.join("a.txt").exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tasks_list_dry_run_skips_tasks_with_missing_directories() {
        let dir = temp_dir("dry_run_list_skip");
        fs::create_dir_all(&dir).unwrap();

        let tasks = TasksList {
            tasks: vec![task("t", &dir.join("missing"), &dir.join("dst"))],
        };

        let mut out = Vec::new();
        tasks.dry_run(&mut out);
        let output = String::from_utf8(out).unwrap();

        assert!(output.contains("skipping dry run"));
        assert!(!output.contains("found"));

        fs::remove_dir_all(&dir).unwrap();
    }
}
