# fsprobe

A command-line tool that compares 2 trees of files. Each side is a folder or a `.zip` file, in any mix (folder vs folder, zip vs zip, folder vs zip).

## What it does

- Walks each folder recursively.
- Reads each zip file from its central directory only (names, sizes, stored CRC-32 values), with no entry ever extracted. Folder entries inside a zip are skipped.
- Pairs files by relative path, written with forward slashes on every platform.
- Marks a pair as changed when the sizes differ, without reading either file.
- Compares CRC-32 values when the sizes match. A folder file has its CRC-32 computed from its bytes. A zip entry uses the CRC-32 stored in the zip.
- Lists changed files, files only on the left, files only on the right, then a summary line with counts of same, changed, only-left, only-right, skipped files.
- Prints a JSON object instead of text with `--json`. Keys come out in sorted order so that 2 runs on the same trees print the same bytes.
- Exits with 0 when the trees match, 1 when they differ, 2 on an error, like `diff`.
- On Windows, never opens the data of a cloud-only placeholder file. Such a pair is compared by size only and listed as skipped.

## Who it is for

Anyone who compares folders, drives, zip files (for example before and after a copy or a backup). It answers 1 question fast -- did every file arrive intact, and what is missing on each side?

## Built with

- Rust, edition 2021, built and tested with Rust 1.97.0.
- `regex` 1.13.1 for `--include`, `--exclude`, `--pair`.
- `zip` 9.4.2 with default features off, used only to read the central directory.
- `crc32fast` 1.5.4 for the CRC-32 of folder files.
- `walkdir` 2.5.0 for the recursive folder walk.
- `serde_json` 1.0.154 for `--json`.
- `tempfile` 3.27.0 for the tests only.

The argument parser is hand-written, with no parser crate.

## Build and run

```sh
cargo build --release
./target/release/fsprobe LEFT RIGHT
```

Run the tests:

```sh
cargo test
```

On every push and pull request, GitHub Actions runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` on Linux, Windows, macOS with Rust 1.97.0 (see [.github/workflows/ci.yml](.github/workflows/ci.yml)).

## Options

```text
$ fsprobe --help
Usage: fsprobe [OPTIONS] LEFT RIGHT

Compare 2 trees. Each side is a folder or a .zip file.
Files pair up by relative path and compare by size, then CRC-32.

Options:
  --include REGEX  Keep only paths that match (repeatable).
  --exclude REGEX  Drop paths that match, wins over --include (repeatable).
  --pair REGEX     Remove matches from file-names before pairing (repeatable).
  --json           Print a JSON object instead of text.
  -h, --help       Print this help.
  -V, --version    Print the version.

Exit status: 0 when the trees match, 1 when they differ, 2 on an error.
```

- `--include` and `--exclude` match against the relative path, such as `docs/notes.txt`.
- With no `--include`, every path is kept.
- A path that matches an `--exclude` is dropped even when it also matches an `--include`.
- `--pair` removes every match from the file-name (the last path component) before pairing so that time-stamped versions of a file pair up. Folder names stay as they are.
- When 2 files on 1 side end up with the same pairing key, the run stops with exit code 2 and names both files.

## Sample output

These runs are pasted from real runs on made-up files.

- The folder `photos-2025` holds 6 files.
- The zip `backup.zip` holds 6 files plus 3 folder entries, written with deflate compression by a separate archiver.
- Against the folder, the zip has a different `budget.txt` of the same size, a longer `print-list.txt`, a new `trips/tofino/map.png`, no `old-draft.txt`.

```text
$ fsprobe photos-2025 backup.zip
changed:
  budget.txt  (crc32 486848d5 -> f0d42fb0)
  print-list.txt  (size 11 -> 19 bytes)
only in left:
  old-draft.txt
only in right:
  trips/tofino/map.png
summary: 3 same, 2 changed, 1 only-left, 1 only-right, 0 skipped
$ echo $?
1
```

The same 2 trees, limited to `budget.txt`, as JSON:

```text
$ fsprobe --json --include '^budget' photos-2025 backup.zip
{
  "changed": [
    {
      "left": "budget.txt",
      "left_crc32": "486848d5",
      "left_size": 18,
      "reason": "crc32",
      "right": "budget.txt",
      "right_crc32": "f0d42fb0",
      "right_size": 18
    }
  ],
  "left": "photos-2025",
  "match": false,
  "only_left": [],
  "only_right": [],
  "right": "backup.zip",
  "skipped": [],
  "summary": {
    "changed": 1,
    "only_left": 0,
    "only_right": 0,
    "same": 0,
    "skipped": 0
  }
}
$ echo $?
1
```

2 folders that each hold 1 copy of a report, saved with a different time-stamp in the file-name:

```text
$ ls mon tue
mon:
report_2026_11_07_04_28_PM_BC.zip

tue:
report_2026_11_07_05_34_PM_BC.zip
$ fsprobe --pair '_\d{4}_\d{2}_\d{2}_\d{2}_\d{2}_(AM|PM)_BC' mon tue
summary: 1 same, 0 changed, 0 only-left, 0 only-right, 0 skipped
$ echo $?
0
```

## Cloud-only files

Cloud services that use the Windows Cloud Files API, such as OneDrive and Dropbox, keep a placeholder on disk for a file whose data is still in the cloud. Opening or reading such a file fetches its data from the cloud.

On Windows, the tool reads the file attributes that come with the folder listing and checks 2 bits:

- `FILE_ATTRIBUTE_RECALL_ON_OPEN`, value `0x00040000`.
- `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, value `0x00400000`.

When either bit is set, the file data is never opened:

- A size difference still counts as changed.
- Equal sizes put the pair in the skipped list, marked `cloud-only, not read`.
- Skipped pairs do not count as a difference for the exit code, because their sizes matched. The summary line always shows the skipped count.

On Linux and macOS the check compiles away, and every file is read when its size matches.

## Licence

MIT Licence. See [LICENSE](LICENSE).
