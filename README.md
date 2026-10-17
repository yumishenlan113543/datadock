# datadock

`datadock` is a small command-line file-backup tool written in Rust. It copies files from one or more *source* directories to matching *destination* directories, keeping the destination up to date with the source.

Each backup job is called a **task**. Tasks are declared in a single TOML config file, and each task is run on its own background thread, with progress shown in the terminal and detailed logging written to a file.

- **Incremental by default** — only files that are missing from the destination or that have changed (compared by size first, then by content) are copied.
- **Mirrors, but never deletes** — files that exist only in the destination are left untouched. `datadock` never removes data from the destination.
- **Safe copies** — each file is streamed to a temporary file in the destination and atomically renamed into place, so a failed copy never corrupts or truncates an existing file.

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
  - [Symlinks](#symlinks)
  - [Concurrency](#concurrency)
- [Running](#running)
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
| Windows | `%APPDATA%\datadock\tasks.toml` (e.g. `C:\Users\<You>\AppData\Roaming\datadock\tasks.toml`) |

### Format

The file is TOML. At the top level it has a single `tasks` key, whose value is an **array of tables** (one table per task). A task has exactly three fields:

```toml
[[tasks]]
name = "photos"
src  = "/home/user/Pictures"
dst  = "/mnt/backup/Pictures"

[[tasks]]
name = "documents"
src  = "/home/user/Documents"
dst  = "/mnt/backup/Documents"
```

### Fields

| Field | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | string | yes | A human-readable label for the task. Used in progress output and logs. |
| `src` | string (path) | yes | The absolute source directory to copy **from**. |
| `dst` | string (path) | yes | The absolute destination directory to copy **to**. |

### Validation rules

Each task is validated when the config file is loaded. A task is rejected (and, on startup, the whole run exits with an error) when:

1. **`src` is not an absolute path.**
2. **`dst` is not an absolute path.**
3. **`src` and `dst` are identical**, or **one is nested inside the other** (in either direction). This would cause a directory to be backed up into itself, so it is always rejected — including when the nesting happens through a symlink.

A valid `src`/`dst` pair are therefore two *unrelated* absolute paths, e.g. `/home/user/Pictures` → `/mnt/backup/Pictures`.

If the config file cannot be parsed as TOML, `datadock` also reports an error and exits. A completely empty `tasks.toml` (or one with no `tasks` key) is valid and simply means "nothing to do".

---

## How a task works

For each task, `datadock`:

1. Recursively scans `src` and `dst` to build a list of regular files.
2. Computes the files that must be copied: those missing from `dst`, plus those whose `dst` copy differs from `src`.
3. Copies each such file to the matching relative location under `dst`, preserving the directory structure.

### The destination must already exist

**A task only starts when both `src` and `dst` already exist. This is by design.**

If either the source directory or the destination directory is missing, the task **fails to start** (it is reported as a startup error) rather than running. `datadock` intentionally does **not** create the destination root directory for you.

> Why: `src` or `dst` is often a removable drive (an external disk, USB stick, or SD card) that is not always connected or mounted. `datadock` requires both directories to be present before starting so a backup is never silently skipped just because a drive happened to be unplugged — the missing drive is instead surfaced as a visible startup error, prompting you to plug it in.

So, before running a task, make sure both directories exist (for example, mount the removable drive you pointed `src` or `dst` at). The destination can be empty — `datadock` will create any *sub*directories it needs while copying — but the destination root itself must be present:

```sh
mkdir -p /mnt/backup/Pictures   # destination root must exist
datadock                        # source /home/user/Pictures must also exist
```

Note that a missing `src` or `dst` does **not** stop the other tasks from running: each task is independent, and a task that cannot start is skipped while the remaining tasks continue.

### What gets copied

`datadock` is incremental and one-directional (source → destination):

- A file is copied when it is **missing** under `dst`, or when its `dst` copy **differs** from `src`.
- "Differs" is decided cheaply: sizes are compared first, and only when sizes are equal are the contents compared in fixed-size chunks (so large files are never loaded fully into memory).
- Files that exist **only** in `dst` are left untouched — `datadock` never deletes anything from the destination.

### Symlinks

Symlinks are **not followed**. They are skipped during the scan and never copied.

### Concurrency

Each task runs on its own background thread, and progress bars for all tasks are rendered together. A failure in one task does not prevent the others from running.

---

## Running

```sh
datadock
```

The program loads `tasks.toml`, prints a short summary to the console (`Run finished: N succeeded, M failed.`), and writes detailed logs to a file (see [Logging](#logging)). Exit code `1` is returned when the config file fails to load or parse.

---

## Logging

`datadock` writes its log to `app.log` in the platform's user **data** directory, alongside the config file's parent directory:

| Platform | Log file path |
| --- | --- |
| Linux | `~/.local/share/datadock/app.log` |
| macOS | `~/Library/Application Support/datadock/app.log` |
| Windows | `%APPDATA%\datadock\app.log` |

The log is truncated at the start of each run. The default level is `Info`; set the `RUST_LOG` environment variable to change it, e.g. `RUST_LOG=debug datadock`.

---

## License

MIT — see [LICENSE](LICENSE).
