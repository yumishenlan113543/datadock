# datadock

[![crates.io](https://img.shields.io/crates/v/datadock.svg)](https://crates.io/crates/datadock)

`datadock` is a small command-line file-backup tool written in Rust. It copies files from one or more *source* directories to matching *destination* directories, keeping the destination up to date with the source according to each task's settings.

> **Positioning:** `datadock` is a tool for simple file backups (like photos and videos). If you need more advanced options, please look for another backup tool.

Each backup job is called a **task**. Tasks are declared in a single TOML config file; each task's copying phase runs on its own background thread, with progress shown in the terminal and detailed logging written to a file.

- **Incremental** — only files that are missing from the destination are copied, plus—when a task enables `scan_contents`—files that have changed (compared by size first, then by content).
- **Ignore-aware** — hidden files and directories, plus files excluded by `.ignore`/`.gitignore` rules, are skipped during the scan.
- **Mirrors, but never deletes** — files that exist only in the destination are left untouched; `datadock` never deletes destination-only files.
- **Safe copies** — each file is streamed to a temporary file alongside its destination and atomically renamed into place, so a failed copy never corrupts or truncates an existing file. The source file's permission bits are preserved.
- **Dry run** — pass `--dry-run` to list the files a run would copy without changing the destination.
- **Guided setup** — run `datadock config add` to add a task, or `datadock config remove` to remove one, interactively.

---

## Table of contents

- [Installation](#installation)
  - [Installing with Cargo](#installing-with-cargo)
  - [Prebuilt binaries](#prebuilt-binaries)
  - [Building from source](#building-from-source)
- [Running](#running)
  - [Adding a task](#adding-a-task)
  - [Removing a task](#removing-a-task)
  - [Dry run](#dry-run)
- [How a task works](#how-a-task-works)
  - [The destination must already exist](#the-destination-must-already-exist)
  - [What gets copied](#what-gets-copied)
  - [Ignored and hidden files](#ignored-and-hidden-files)
  - [Symlinks](#symlinks)
  - [Concurrency](#concurrency)
- [Configuration and logging](#configuration-and-logging)
  - [Config file](#config-file)
  - [Logging](#logging)
- [License](#license)

---

## Installation

The methods below are listed from most to least recommended.

### Installing with Cargo (Recommended)

If you have a Rust toolchain (see [Building from source](#building-from-source) for how to get one), the recommended way to install `datadock` is from [crates.io](https://crates.io/crates/datadock):

```sh
cargo install datadock
```

This downloads, compiles, and installs the latest published release into Cargo's binary directory (`~/.cargo/bin` by default), which should already be on your `PATH`. To upgrade later, run the same command again.

### Prebuilt binaries

If you don't have a Rust toolchain, download the binary for your platform from the [Releases](https://github.com/yumishenlan113543/datadock/releases) page. No Rust toolchain is required.

| Platform | Binary |
| --- | --- |
| Linux x86_64 | `datadock-linux-x86_64` |
| macOS (Apple Silicon) | `datadock-macos-aarch64` |
| macOS (Intel) | `datadock-macos-x86_64` |
| Windows x86_64 | `datadock-windows-x86_64.exe` |

Each release provides one executable per platform. Download the file for your system, make it executable if needed (`chmod +x datadock-*` on Linux/macOS), and run it directly.

### Building from source

Use this if you want to build from a checkout, for example to try an unreleased change or to run the test suite.

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

   This is handy during development, but for regular use prefer `cargo install datadock` or the prebuilt binary.

---

## Running

```sh
datadock
```

The program loads `tasks.toml` (see [Config file](#config-file)), prints a short summary to the console (`Run finished: N task(s) succeeded, M task(s) failed, K file(s) failed.`), and writes detailed logs to a file (see [Logging](#logging)). The first two counts are **tasks** and the last is **individual files**. Tasks skipped because `src` or `dst` is missing are not counted. A task is counted as failed when planning its file list fails, when its copy thread returns an error, or when its thread panics; a task whose individual files fail to copy still counts as succeeded, but those files are counted separately in the summary.

Exit code `1` is returned when the config file fails to load, parse, or validate; when logging cannot be initialized; when adding or removing a task fails; when any task fails; when any file fails to copy; or when a task's scan fails during a dry run. Tasks skipped because `src` or `dst` is missing do not change the exit code.

Run `datadock --help` to see all options and `datadock --version` to print the version.

### Adding a task

Run `datadock config add` to add a task interactively:

```sh
datadock config add
```

It prompts for the task `name`, `src`, `dst`, and whether to `scan_contents`, shows a summary of the new task, and asks for confirmation before saving. The task is validated with the same rules used when loading the file, then appended to `tasks.toml`: `src` and `dst` must be absolute paths without `..` components, must be distinct (neither nested inside the other, directly or through a symlink), and any path that already exists must be a directory. Missing directories are allowed — such a task is simply skipped at run time until they exist.

Adding a task rewrites the whole config file: tasks loaded at startup are preserved, but any comments or custom formatting in `tasks.toml` are not. The file is replaced atomically, so a failed save leaves the previous config intact. Canceling the confirmation leaves the file unchanged. Do not edit `tasks.toml` in another program while `datadock config add` is running; the rewritten file is based on the tasks loaded at startup, so such edits are overwritten.

### Removing a task

Run `datadock config remove` to remove an existing task interactively:

```sh
datadock config remove
```

It shows a list of the configured tasks (each labeled with its source and destination paths) and lets you select the one to remove. After you confirm, the task is deleted from `tasks.toml`. When no tasks are configured, it prints a message and exits without removing any task. (Startup may still create the config file and log file if they do not exist.)

Removing a task rewrites the whole config file: the remaining tasks loaded at startup are preserved, but any comments or custom formatting in `tasks.toml` are not. The file is replaced atomically, so a failed save leaves the previous config intact. Canceling the confirmation leaves the file unchanged. Do not edit `tasks.toml` in another program while `datadock config remove` is running; the rewritten file is based on the tasks loaded at startup, so such edits are overwritten.

### Dry run

Pass `--dry-run` to preview a run without copying anything:

```sh
datadock --dry-run
```

For each task whose `src` and `dst` both exist and are directories, it prints the absolute source path of every file that would be copied, using the same rules as a real run (see [What gets copied](#what-gets-copied)). Files that are already up to date, and hidden or ignored files, are omitted. Tasks whose `src` or `dst` is missing are skipped with a message, just like a normal run.

A dry run does not create destination directories, copy or delete files, show progress bars, or print the `Run finished` summary. (As with any run, startup may still create the config and log locations if they do not exist.) `scan_contents` still controls whether changed files are reported, and a task whose scan fails is reported without stopping the remaining tasks.

---

## How a task works

For each task, `datadock`:

1. Recursively scans `src` and `dst` to build a list of regular files. Hidden files and directories, as well as files excluded by ignore rules, are left out of the scan (see [Ignored and hidden files](#ignored-and-hidden-files)).
2. Computes the files that must be copied: those missing from `dst`, plus—when `scan_contents` is `true`—those whose `dst` copy differs from `src`.
3. Copies each such file to the matching relative location under `dst`, preserving the directory structure.

### The destination must already exist

**A task only starts when both `src` and `dst` already exist and are directories. This is by design.**

If either the source directory or the destination directory is missing, the task is **skipped** (a message is printed to the console and the log) rather than run. An existing path that is not a directory is instead rejected when the config is loaded (see the validation rules under [Adding a task](#adding-a-task)). `datadock` intentionally does **not** create the destination root directory for you, and a skipped task is **not** counted as a failure.

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

## Configuration and logging files

`datadock` stores its task list in `tasks.toml` and its log in `app.log`, both under the platform's standard user directories. The config file is created empty on first run if it does not already exist.

| Platform | Config (`tasks.toml`) | Log (`app.log`) |
| --- | --- | --- |
| Linux | `$XDG_CONFIG_HOME/datadock/tasks.toml` (or `~/.config/datadock/tasks.toml`) | `$XDG_DATA_HOME/datadock/app.log` (or `~/.local/share/datadock/app.log`) |
| macOS | `~/Library/Application Support/datadock/tasks.toml` | `~/Library/Application Support/datadock/app.log` |
| Windows | `%APPDATA%\datadock\config\tasks.toml` | `%APPDATA%\datadock\data\app.log` |

### Config file

The config file is TOML. At the top level it recognizes a single `tasks` key, whose value is an **array of tables** (one table per task). All four fields are required; unknown fields are ignored.

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

| Field | Type | Description |
| --- | --- | --- |
| `name` | string | A human-readable label for the task. Used in progress output and logs. |
| `src` | string (path) | The absolute source directory to copy **from**. Must not contain `..` components. |
| `dst` | string (path) | The absolute destination directory to copy **to**. Must not contain `..` components. |
| `scan_contents` | boolean | When `true`, files whose contents differ are copied over their destination copies. When `false`, only files missing from `dst` are copied. |

A file that cannot be parsed as TOML is reported as an error; a completely empty file (or one with no `tasks` key) simply means "nothing to do".

### Logging

The log is truncated at the start of each run. The default level is `Info`; set the `RUST_LOG` environment variable to change it, e.g. `RUST_LOG=debug datadock`.

---

## License

MIT — see [LICENSE](LICENSE).
