# better-cp

[![CI](https://github.com/Miro-sh/better-cp/actions/workflows/ci.yml/badge.svg)](https://github.com/Miro-sh/better-cp/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Miro-sh/better-cp)](https://github.com/Miro-sh/better-cp/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)
![Binary](https://img.shields.io/badge/binary-fully%20static-blue)

`bcp` is a drop-in replacement for `cp` that shows you what it's doing. It prints a live progress bar with throughput and ETA while it copies, and it never leaves a half-written file behind when something goes wrong. Same flags you already know, same exit codes your scripts already check.

```console
$ bcp -r ~/Photos /mnt/backup
[████████████████████░░░░░░░░░░]  67%  2.3 GiB/3.4 GiB  45.2 MiB/s  ETA 25s

$ bcp document.txt /tmp/
✓ Copied document.txt → /tmp/document.txt (12 KB)

$ bcp -r huge_folder/ /dest/ --no-progress
# silence, the exit code tells you how it went
```

## Why

`cp` gives you no feedback. On a 200 GB backup you stare at a blinking cursor for twenty minutes, wondering if anything is happening at all. `bcp` answers that question and fixes a few other rough edges while it's at it: it checks free disk space before it starts, refuses to copy a directory into itself, and cleans up after itself when you hit Ctrl+C halfway through a file.

## Features

- Recursive copies with a single aggregate progress bar, rsync `--info=progress2` style: percentage, copied vs total size, current speed, ETA.
- Atomic by default. Every file is written under a temporary name in the destination directory and renamed into place once complete. An interrupted copy leaves no partial files at the destination.
- Stays out of your way. The bar only appears after one second of copying, so quick copies don't flash it. Piped or scripted output disables the bar automatically, exactly how `cp` would behave.
- `-a` archive mode preserves permissions and timestamps, directory metadata included.
- `-v` lists every file as it is copied, like `cp -v`.
- Pre-flight disk space check with a clear error message, instead of dying at 97%.
- Symlinks are recreated as symlinks. Sockets, fifos and device files are skipped with a warning.

## Installation

Grab a prebuilt binary for your platform from the [releases page](https://github.com/Miro-sh/better-cp/releases), unpack it, and put `bcp` somewhere on your `PATH`. Linux builds are fully static and run on any distro.

Or build from source with a [Rust toolchain](https://rustup.rs/):

```console
$ git clone https://github.com/<your-username>/better-cp
$ cd better-cp
$ cargo install --path .
```

This puts a fully static `bcp` binary in `~/.cargo/bin`. Delete `.cargo/config.toml` if you'd rather build for your native target.

## Usage

```
bcp [OPTIONS] <SOURCE> <DESTINATION>
```

| Flag  | Long            | Effect                                        |
|-------|-----------------|-----------------------------------------------|
| `-r`  | `--recursive`   | Copy directories (required, like `cp -r`)     |
| `-a`  | `--archive`     | Preserve permissions and timestamps           |
| `-v`  | `--verbose`     | Print each file as it is copied               |
|       | `--progress`    | Force the progress bar on                     |
|       | `--no-progress` | Force the progress bar off (for scripts)      |

```console
$ bcp report.pdf ~/Documents/
$ bcp -ra ~/Photos /mnt/backup/photos
$ bcp -rv projects/ /external-drive/
```

Destination semantics match `cp`: an existing directory receives the source inside it under its original name, anything else is treated as the target file name. One deliberate extension: a trailing `/` on a destination that doesn't exist yet is taken as a directory to create, the way rsync reads it.

## What happens when things go wrong

- Ctrl+C during a copy deletes the temporary file in progress and exits with code 130.
- A file that can't be read or written is reported by name, the rest of the copy continues, and the process exits with code 1 at the end.
- Not enough free space at the destination is detected before a single byte gets copied.
- Copying a directory into one of its own subdirectories is refused up front. No infinite `a/b/b/b/b` recursion, ever.

## Performance notes

With the progress bar off, files go through `std::fs::copy`, which uses `copy_file_range(2)` on Linux and never leaves the kernel. With the bar on, `bcp` copies through a userspace buffer so it can count bytes as they pass: 256 KiB normally, 4 MiB for files above 1 GiB. In practice both paths saturate an NVMe drive. The buffered path costs a few percent on very fast storage and nothing you'd notice on anything slower.

## Development

```console
$ cargo build --release   # compiles with zero warnings
$ cargo test              # 19 unit and integration tests
```

Four small modules: `main.rs` handles the CLI and orchestration, `copy.rs` plans and executes the copy, `progress.rs` owns the bar, `utils.rs` does formatting and terminal detection.

## License

MIT
