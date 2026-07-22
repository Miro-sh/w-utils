<div align="center">

<img src="docs/logo.svg" alt="w-utils logo" width="128">

# w-utils

[![CI](https://github.com/Miro-sh/w-utils/actions/workflows/ci.yml/badge.svg)](https://github.com/Miro-sh/w-utils/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Miro-sh/w-utils)](https://github.com/Miro-sh/w-utils/releases)
[![dependency status](https://deps.rs/repo/github/Miro-sh/w-utils/status.svg)](https://deps.rs/repo/github/Miro-sh/w-utils)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey)

</div>

---

Unix command-line tools, rewritten in Rust with a modern UX. The first member of the suite is **wcp**, a drop-in replacement for `cp` that shows you what it's doing: a live progress bar with throughput and ETA, and copies that never leave a half-written file behind. Same flags you already know, same exit codes your scripts already check.

## Quick install

```console
$ curl -sSfL https://raw.githubusercontent.com/Miro-sh/w-utils/main/install.sh | sh
```

Packages and other install methods are [further down](#installation).

```console
$ wcp -r ~/Photos /mnt/backup
[████████████████████░░░░░░░░░░]  67%  2.3 GiB/3.4 GiB  45.2 MiB/s  ETA 25s

$ wcp document.txt /tmp/
✓ Copied document.txt → /tmp/document.txt (12 KB)

$ wcp -r huge_folder/ /dest/ --no-progress
# silence, the exit code tells you how it went
```

## Why

`cp` gives you no feedback. On a 200 GB backup you stare at a blinking cursor for twenty minutes, wondering if anything is happening at all. `wcp` answers that question and fixes a few other rough edges while it's at it: it checks free disk space before it starts, refuses to copy a directory into itself, and cleans up after itself when you hit Ctrl+C halfway through a file.

## Goals

- Drop-in replacements: same flags, same destination semantics, same exit codes as the originals. Behavior differences are bugs.
- Modern UX where it helps: live progress, clear colored errors, sensible defaults.
- Safe by default: atomic writes, pre-flight checks, nothing half-finished left behind after Ctrl+C.
- Cross-platform: Linux, macOS and Windows, with fully static Linux binaries that run on any distro.
- One package, many tools: install `w-utils` once and every utility comes with it.

## Features

- Recursive copies with a single aggregate progress bar, rsync `--info=progress2` style: percentage, copied vs total size, current speed, ETA.
- Atomic by default. Every file is written under a temporary name in the destination directory and renamed into place once complete. An interrupted copy leaves no partial files at the destination.
- Stays out of your way. The bar only appears after one second of copying, so quick copies don't flash it. Piped or scripted output disables the bar automatically, exactly how `cp` would behave.
- `-a` archive mode preserves permissions and timestamps, directory metadata included.
- `-v` lists every file as it is copied, like `cp -v`.
- Pre-flight disk space check with a clear error message, instead of dying at 97%.
- Symlinks are recreated as symlinks. Sockets, fifos and device files are skipped with a warning.
- Ships with a man page (`man wcp`), generated from the CLI definition so it never drifts from `--help`.

## Installation

Quick install script (Linux and macOS):

```console
$ curl -sSfL https://raw.githubusercontent.com/Miro-sh/w-utils/main/install.sh | sh
```

Native packages, from the [releases page](https://github.com/Miro-sh/w-utils/releases):

```console
# Debian / Ubuntu
$ sudo dpkg -i w-utils-x86_64-unknown-linux-musl.deb

# Fedora / RHEL / openSUSE
$ sudo rpm -i w-utils-x86_64-unknown-linux-musl.rpm
```

Raw binaries are there too (unpack, put `wcp` on your `PATH`). Every release ships a `SHA256SUMS.txt` covering all artifacts:

```console
$ curl -sSfLO https://github.com/Miro-sh/w-utils/releases/latest/download/SHA256SUMS.txt
$ sha256sum -c SHA256SUMS.txt --ignore-missing
```

And if you have a [Rust toolchain](https://rustup.rs/):

```console
$ cargo install --git https://github.com/Miro-sh/w-utils
```

Or from a clone:

```console
$ git clone https://github.com/Miro-sh/w-utils
$ cd w-utils
$ cargo install --path .
```

This puts a fully static `wcp` binary in `~/.cargo/bin`. Delete `.cargo/config.toml` if you'd rather build for your native target.

## Usage

```
wcp [OPTIONS] <SOURCE> <DESTINATION>
```

| Flag  | Long            | Effect                                        |
|-------|-----------------|-----------------------------------------------|
| `-r`  | `--recursive`   | Copy directories (required, like `cp -r`)     |
| `-a`  | `--archive`     | Preserve permissions and timestamps           |
| `-v`  | `--verbose`     | Print each file as it is copied               |
|       | `--progress`    | Force the progress bar on                     |
|       | `--no-progress` | Force the progress bar off (for scripts)      |

```console
$ wcp report.pdf ~/Documents/
$ wcp -ra ~/Photos /mnt/backup/photos
$ wcp -rv projects/ /external-drive/
```

Destination semantics match `cp`: an existing directory receives the source inside it under its original name, anything else is treated as the target file name. One deliberate extension: a trailing `/` on a destination that doesn't exist yet is taken as a directory to create, the way rsync reads it.

## What happens when things go wrong

- Ctrl+C during a copy deletes the temporary file in progress and exits with code 130.
- A file that can't be read or written is reported by name, the rest of the copy continues, and the process exits with code 1 at the end.
- Not enough free space at the destination is detected before a single byte gets copied.
- Copying a directory into one of its own subdirectories is refused up front. No infinite `a/b/b/b/b` recursion, ever.

## Performance notes

With the progress bar off, files go through `std::fs::copy`, which uses `copy_file_range(2)` on Linux and never leaves the kernel. With the bar on, `wcp` copies through a userspace buffer so it can count bytes as they pass: 256 KiB normally, 4 MiB for files above 1 GiB. In practice both paths saturate an NVMe drive. The buffered path costs a few percent on very fast storage and nothing you'd notice on anything slower.

## Uninstall

It depends on how you installed:

```console
# install script or manual copy (user install: look in ~/.local instead)
$ sudo rm /usr/local/bin/wcp /usr/local/share/man/man1/wcp.1.gz

# Debian / Ubuntu
$ sudo apt remove w-utils

# Fedora / RHEL / openSUSE
$ sudo dnf remove w-utils

# Rust toolchain
$ cargo uninstall w-utils
```

## Development

```console
$ cargo build --release   # compiles with zero warnings
$ cargo test              # 19 unit and integration tests
```

Four small modules: `main.rs` handles orchestration, `cli.rs` defines the CLI, `copy.rs` plans and executes the copy, `progress.rs` owns the bar, `utils.rs` does formatting and terminal detection. New tools join the suite as additional `[[bin]]` targets in `Cargo.toml`.

## Contributing

Issues and pull requests are welcome on [GitHub](https://github.com/Miro-sh/w-utils).

## License

MIT
