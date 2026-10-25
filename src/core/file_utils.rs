use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use ignore::Walk;
use indicatif::ProgressBar;
use log::{debug, info, warn};
use tempfile::NamedTempFile;

/// The files that failed to copy, each paired with its error message.
pub(super) type FailedFiles = Vec<(PathBuf, String)>;

/// Recursively collect every regular file under `root`.
///
/// Discovery uses the `ignore` crate with default settings: hidden entries and
/// ignored files (`.ignore`, `.gitignore`, `.git/info/exclude`, and the global
/// git ignore) are skipped, with git rules only applying inside a repository.
/// Directory symlinks are not followed, but a symlink to a regular file is
/// collected. Other entry types are skipped.
///
/// Unreadable entries are logged and skipped, and a missing `root` yields an
/// empty set rather than an error.
pub(super) fn collect_files(root: &Path) -> HashSet<PathBuf> {
    let mut files = HashSet::new();

    debug!("Collecting files under \"{}\"", root.display());

    for entry in Walk::new(root) {
        match entry {
            // Insert the path of every regular file into the set.
            Ok(entry) => {
                let path = entry.path();
                if path.is_file() {
                    files.insert(path.to_path_buf());
                }
            }
            // Log a warning for any entry that cannot be read.
            Err(error) => warn!("Failed to read entry under \"{}\": {error}", root.display()),
        }
    }

    debug!(
        "Collected {} file(s) under \"{}\"",
        files.len(),
        root.display()
    );

    files
}

/// Compare `file_a` and `file_b`, returning `Ok(true)` when they differ.
///
/// Sizes are compared first, then contents in fixed-size chunks only when the
/// sizes match, so large files are not fully loaded into memory.
pub(super) fn files_differ(file_a: &Path, file_b: &Path) -> io::Result<bool> {
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

/// Copy every file in `files` from under `src` to the matching path under
/// `dst`, preserving the relative directory structure.
///
/// Each entry is a `(path, size)` pair, where `path` is relative to `src`.
/// Parent directories are created as needed, and progress is reported to `bar`
/// in bytes.
///
/// A file that fails to copy is skipped (any existing destination is left
/// intact) and its absolute source path and error message are collected; the
/// remaining files are still copied.
pub(super) fn copy_file_list(
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
/// preserves the source file's permission bits.
///
/// The copy is written to a temporary file and atomically renamed into place on
/// success, so a failed copy never truncates an existing destination.
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

#[cfg(test)]
mod tests {
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

    /// A progress bar that never renders, keeping test output clean.
    fn hidden_bar() -> ProgressBar {
        ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden())
    }

    #[test]
    fn collect_files_returns_empty_for_empty_dir() {
        let dir = temp_dir("collect_empty");
        fs::create_dir_all(&dir).unwrap();

        let files = collect_files(&dir);
        assert!(files.is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn collect_files_collects_files_recursively() {
        let dir = temp_dir("collect_recursive");
        write_file(&dir.join("a.txt"), b"a");
        write_file(&dir.join("sub").join("b.txt"), b"b");
        write_file(&dir.join("sub").join("deep").join("c.txt"), b"c");

        let mut files: Vec<PathBuf> = collect_files(&dir).into_iter().collect();
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
    fn collect_files_returns_empty_for_missing_root() {
        let dir = temp_dir("collect_missing");
        assert!(collect_files(&dir).is_empty());
    }

    #[test]
    fn collect_files_skips_hidden_files_and_directories() {
        let dir = temp_dir("collect_hidden");
        write_file(&dir.join("visible.txt"), b"visible");
        write_file(&dir.join(".hidden.txt"), b"hidden");
        write_file(&dir.join(".hidden_dir").join("nested.txt"), b"nested");

        let files = collect_files(&dir);
        assert_eq!(files.len(), 1);
        assert!(files.contains(&dir.join("visible.txt")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn collect_files_respects_ignore_file() {
        let dir = temp_dir("collect_ignore");
        write_file(&dir.join(".ignore"), b"ignored.txt\nignored_dir/\n");
        write_file(&dir.join("visible.txt"), b"visible");
        write_file(&dir.join("ignored.txt"), b"ignored");
        write_file(&dir.join("ignored_dir").join("nested.txt"), b"nested");

        let files = collect_files(&dir);
        assert_eq!(files.len(), 1);
        assert!(files.contains(&dir.join("visible.txt")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn collect_files_respects_gitignore_inside_git_repository() {
        let dir = temp_dir("collect_gitignore");
        fs::create_dir_all(dir.join(".git")).unwrap();
        write_file(&dir.join(".gitignore"), b"ignored.txt\n");
        write_file(&dir.join("visible.txt"), b"visible");
        write_file(&dir.join("ignored.txt"), b"ignored");

        let files = collect_files(&dir);
        assert_eq!(files.len(), 1);
        assert!(files.contains(&dir.join("visible.txt")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn collect_files_includes_symlinked_files_without_following_directory_symlinks() {
        let dir = temp_dir("collect_symlinks");
        write_file(&dir.join("real.txt"), b"real");
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();
        write_file(&dir.join("real_dir").join("nested.txt"), b"nested");
        std::os::unix::fs::symlink(dir.join("real_dir"), dir.join("link_dir")).unwrap();

        let files = collect_files(&dir);
        assert_eq!(files.len(), 3);
        assert!(files.contains(&dir.join("real.txt")));
        // A symlink to a regular file is collected as a file...
        assert!(files.contains(&dir.join("link.txt")));
        assert!(files.contains(&dir.join("real_dir").join("nested.txt")));
        // ...but a symlink to a directory is neither collected nor descended
        // into.
        assert!(!files.contains(&dir.join("link_dir")));
        assert!(!files.contains(&dir.join("link_dir").join("nested.txt")));

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
}
