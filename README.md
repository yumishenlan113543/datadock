# datadock

`datadock` is a small command-line file-backup tool written in Rust. It copies files from one or more *source* directories to matching *destination* directories, keeping the destination up to date with the source according to each task's settings.

> **Positioning:** `datadock` is a tool for simple file backups (like photos and videos). If you need more advanced options, please look for another backup tool.

Each backup job is called a **task**. Tasks are declared in a single TOML config file; each task's copying phase runs on its own background thread, with progress shown in the terminal and detailed logging written to a file.

- **Incremental** — only files that are missing from the destination are copied, plus—when a task enables `scan_contents`—files that have changed (compared by size first, then by content).
- **Ignore-aware** — hidden files and directories, plus files excluded by `.ignore`/`.gitignore` rules, are skipped during the scan.
- **Mirrors, but never deletes** — files that exist only in the destination are left untouched; `datadock` never deletes destination-only files.
- **Safe copies** — each file is streamed to a temporary file alongside its destination and atomically renamed into place, so a failed copy never corrupts or truncates an existing file. The source file's permission bits are preserved.
- **Dry run** — pass `--dry-run` to list the files a run would copy without changing the destination.

---

## Table of contents

- [Building](#building)
- [Configuration file](#configuration-file)
  - [Location](#location)
  - [Format](#format)
  - [Fields](#fields)
  - [Validation rules](#validation-rules)
- [How a task works](#how-a-task-works)
  - [The destination must already exist](#the-destination-must-already-exist)
  - [What gets copied](#what-gets-copied)
  - [Ignored and hidden files](#ignored-and-hidden-files)
  - [Symlinks](#symlinks)
  - [Concurrency](#concurrency)
- [Running](#running)
  - [Dry run](#dry-run)
- [Logging](#logging)
- [License](#license)

---

## Building

`datadock` is built with Cargo (Rust's package manager) and requires a stable Rust toolchain (edition 2024).

1. Install Rust if you don't already have it, using [rustup](https://rustup.rs).

2. From the project root, build the release binary:

   ```sh
   cargo build --release
   ```

   The binary is written to `target/release/datadock`.

3. (Optional) Run the test suite to verify the build:

   ```sh
   cargo test
   ```

4. (Optional) Compile and run directly in one step, using a debug build:

   ```sh
   cargo run
   ```

   This is handy during development, but for regular use prefer `cargo build --release` and run the resulting `target/release/datadock` binary.

---

## Configuration file

`datadock` reads its tasks from a single TOML file named `tasks.toml`. The file is **created automatically (empty)** the first time you run `datadock` if it does not already exist.

### Location

The file is resolved through the platform's standard user-config directory (using the `directories` crate with the application name `datadock`):

| Platform | Config file path |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME/datadock/tasks.toml` (or `~/.config/datadock/tasks.toml`) |
| macOS | `~/Library/Application Support/datadock/tasks.toml` |
| Windows | `%APPDATA%\datadock\config\tasks.toml` (e.g. `C:\Users\<You>\AppData\Roaming\datadock\config\tasks.toml`) |

### Format

The file is TOML. At the top level it recognizes a single `tasks` key, whose value is an **array of tables** (one table per task). A task has four recognized fields, all required; unknown fields are ignored. For example:

```toml
[[tasks]]
name          = "photos"
src           = "/home/user/Pictures"
dst           = "/mnt/backup/Pictures"
scan_contents = false

[[tasks]]
name          = "documents"
src           = "/home/user/Documents"
dst           = "/mnt/backup/Documents"
scan_contents = true
```

### Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | string | yes | A human-readable label for the task. Used in progress output and logs. |
| `src` | string (path) | yes | The absolute source directory to copy **from**. Must not contain `..` components. |
| `dst` | string (path) | yes | The absolute destination directory to copy **to**. Must not contain `..` components. |
| `scan_contents` | boolean | yes | When `true`, files whose contents differ are copied over their destination copies. When `false`, only files missing from `dst` are copied; existing files are kept even if their contents differ. |

### Validation rules

Each task is validated when the config file is loaded. A task is rejected (and, on startup, the whole run exits with an error) when:

1. **`src` is not an absolute path.**
2. **`dst` is not an absolute path.**
3. **`src` or `dst` contains a `..` component.** Paths are used as written, so `..` is rejected rather than resolved.
4. **`src` and `dst` are identical**, or **one is nested inside the other** (in either direction). This would cause a directory to be backed up into itself, so it is always rejected — including when the nesting happens through a symlink that exists at load time.

A valid `src`/`dst` pair are therefore two *unrelated* absolute paths without `..` components, e.g. `/home/user/Pictures` → `/mnt/backup/Pictures`.

If the config file cannot be parsed as TOML, `datadock` also reports an error and exits. A completely empty `tasks.toml` (or one with no `tasks` key) is valid and simply means "nothing to do".

---

## How a task works

For each task, `datadock`:

1. Recursively scans `src` and `dst` to build a list of regular files. Hidden files and directories, as well as files excluded by ignore rules, are left out of the scan (see [Ignored and hidden files](#ignored-and-hidden-files)).
2. Computes the files that must be copied: those missing from `dst`, plus—when `scan_contents` is `true`—those whose `dst` copy differs from `src`.
3. Copies each such file to the matching relative location under `dst`, preserving the directory structure.

### The destination must already exist

**A task only starts when both `src` and `dst` already exist. This is by design.**

If either the source directory or the destination directory is missing, the task is **skipped** (a message is printed to the console and the log) rather than run. `datadock` intentionally does **not** create the destination root directory for you, and a skipped task is **not** counted as a failure.

> Why: `src` or `dst` is often a removable drive (an external disk, USB stick, or SD card) that is not always connected or mounted. `datadock` requires both directories to be present before starting so a backup is never silently missed — a missing drive is instead surfaced as a visible "skipping" message, prompting you to plug it in.

So, before running a task, make sure both directories exist (for example, mount the removable drive you pointed `src` or `dst` at). The destination can be empty — `datadock` will create any *sub*directories it needs while copying — but the destination root itself must be present:

```sh
mkdir -p /mnt/backup/Pictures   # destination root must exist
datadock                        # source /home/user/Pictures must also exist
```

Note that a missing `src` or `dst` does **not** stop the other tasks from running: each task is independent, and a task whose directories are missing is skipped (and not counted in the final summary) while the remaining tasks continue. A [dry run](#dry-run) applies the same rule and skips tasks whose directories are missing.

### What gets copied

`datadock` is incremental and one-directional (source → destination):

- A file is copied when it is **missing** under `dst`. When `scan_contents = true`, a file is also copied when its `dst` copy **differs** from `src`.
- When `scan_contents = false`, an existing file under `dst` is left alone even if its contents differ from `src`; only whether the file is present under `dst` is considered.
- When enabled, "differs" is decided cheaply: sizes are compared first, and only when sizes are equal are the contents compared in fixed-size chunks (so large files are never loaded fully into memory).
- Files that exist **only** in `dst` are left untouched — `datadock` never deletes destination-only files.
- Hidden files and files excluded by ignore rules are not scanned, so they are never copied (see [Ignored and hidden files](#ignored-and-hidden-files)).
- If the copy of a file's contents **fails**, it is reported to the console and skipped; the remaining files in the task are still copied. A failure to create a destination parent directory is not skipped, however: it stops that task and the task is counted as failed.

### Ignored and hidden files

File discovery uses the [`ignore`](https://crates.io/crates/ignore) crate with its default settings, so a scan is not a plain "every file" walk:

- Entries whose name starts with `.` are skipped (on Windows, entries with the hidden attribute are skipped too). This covers things such as `.git`, `.cache`, and `.env`.
- `.ignore` files are always honored, including those in parent directories.
- Inside a git repository (a directory containing `.git`), `.gitignore` files (also from parent directories), `.git/info/exclude`, and the user's global git ignore file are honored as well. A `.gitignore` outside a git repository has no effect.
- The rules apply to both `src` and `dst`, and each directory is evaluated independently. A file ignored under `src` is not backed up. A file ignored under `dst` is treated as missing from the destination, so it is copied again on every run if the same relative file exists under `src`.

Ignored entries are not errors; they are simply left out of the scan. If a file should be backed up, do not hide or ignore it. Entries that the scan cannot read are logged as warnings and skipped.

### Symlinks

Directory symlinks are **not followed**: a symlink to a directory is skipped and its contents are not scanned.

A symlink to a regular file is collected during the scan and copied as a regular file containing the target's contents; the result under `dst` is not a symlink. Broken symlinks, and symlinks to anything other than a regular file or directory, are skipped.

### Concurrency

Tasks are planned one at a time on the main thread: each task's source and destination are scanned and its copy list is computed before it is scheduled. The actual file copying for each task then runs on its own background thread, and progress bars for all copying tasks are rendered together. A failure in one task does not prevent the others from running.

---

## Running

```sh
datadock
```

The program loads `tasks.toml`, prints a short summary to the console (`Run finished: N succeeded, M failed.`), and writes detailed logs to a file (see [Logging](#logging)). The counts are **tasks**, not files. Tasks skipped because `src` or `dst` is missing are not counted. A task is counted as failed when planning its file list fails, when its copy thread returns an error, or when its thread panics; a task whose individual files fail to copy still counts as succeeded, and those files are reported separately.

Exit code `1` is returned when the config file fails to load, parse, or validate, or when logging cannot be initialized. Task and per-file failures are reported on the console but do not change the exit code.

Run `datadock --help` to see all options and `datadock --version` to print the version.

### Dry run

Pass `--dry-run` to preview a run without copying anything:

```sh
datadock --dry-run
```

For each task whose `src` and `dst` both exist, it prints the absolute source path of every file that would be copied, using the same rules as a real run (see [What gets copied](#what-gets-copied)). Files that are already up to date, and hidden or ignored files, are omitted. Tasks whose `src` or `dst` is missing are skipped with a message, just like a normal run.

A dry run does not create destination directories, copy or delete files, show progress bars, or print the `Run finished` summary. (As with any run, startup may still create the config and log locations if they do not exist.) `scan_contents` still controls whether changed files are reported, and a task whose scan fails is reported without stopping the remaining tasks.

---

## Logging

`datadock` writes its log to `app.log` in the platform's user **data** directory (note that on Linux and Windows this is a different directory from the config file's):

| Platform | Log file path |
| --- | --- |
| Linux | `$XDG_DATA_HOME/datadock/app.log` (or `~/.local/share/datadock/app.log`) |
| macOS | `~/Library/Application Support/datadock/app.log` |
| Windows | `%APPDATA%\datadock\data\app.log` (e.g. `C:\Users\<You>\AppData\Roaming\datadock\data\app.log`) |

The log is truncated at the start of each run. The default level is `Info`; set the `RUST_LOG` environment variable to change it, e.g. `RUST_LOG=debug datadock`.

---

## License

MIT — see [LICENSE](LICENSE).
