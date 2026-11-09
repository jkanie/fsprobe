# fsprobe

A command-line tool that compares 2 trees of files. Each side is a folder or a `.zip` file, in any mix (folder vs folder, zip vs zip, folder vs zip).

## What it does

- Walks each folder recursively.
- Reads each zip file from its central directory only (names, sizes, stored CRC-32 values), with no entry ever extracted. Folder entries inside a zip are skipped.
- Pairs files by relative path, written with forward slashes on every platform.
- Marks a pair as changed when the sizes differ, without reading either file.
- Compares CRC-32 values when the sizes match. A folder file has its CRC-32 computed from its bytes. A zip entry uses the CRC-32 stored in the zip.
- With `--hash blake3`, compares 2 folder files of equal size by their BLAKE3 digests instead. CRC-32 has only 32 bits. A file can also be crafted to match any given CRC-32. BLAKE3 is a collision-resistant hash.
- With `--verify`, compares 2 folder files of equal size byte for byte and names the offset of the 1st byte that differs. It wins over `--hash`, because a byte-for-byte compare needs no hash.
- Keeps CRC-32 for a pair with a zip entry on either side, even with `--hash blake3` or `--verify`, because the tool never extracts a zip entry.
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
- `blake3` 1.8.7 for `--hash blake3`.
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
  --hash NAME      crc32 (the default) or blake3, for 2 folder files.
  --verify         Compare 2 folder files byte for byte, wins over --hash.
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
- `--hash` takes `crc32` (the default) or `blake3`, as `--hash blake3` or `--hash=blake3`.
- `--verify` stops reading a pair at the 1st byte that differs. Only 2 equal files are read to the end.
- In the JSON report, a pair changed under `--hash blake3` has `"reason": "blake3"` with `left_blake3` and `right_blake3`. A pair changed under `--verify` has `"reason": "bytes"` with `offset`.

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

2 folders that each hold the same `notes.txt` and a 64-byte `sensor.bin`. The 2 `sensor.bin` files differ in their 1st byte and in their last 4 bytes, which were picked such that both files share the CRC-32 `0f3ef7a7`. The default compare cannot tell them apart:

```text
$ fsprobe disk-a disk-b
summary: 2 same, 0 changed, 0 only-left, 0 only-right, 0 skipped
$ echo $?
0
$ fsprobe --verify disk-a disk-b
changed:
  sensor.bin  (bytes differ at offset 0)
summary: 1 same, 1 changed, 0 only-left, 0 only-right, 0 skipped
$ echo $?
1
$ fsprobe --hash blake3 disk-a disk-b
changed:
  sensor.bin  (blake3 d4254e071e614acce9051d7922845ccd1d4290422f353f30741001f7a682bd71 -> d8787d8ac413be45d585be38b9cfe3c61c77a95d7e749a8a45f8ca24a9a64a49)
summary: 1 same, 1 changed, 0 only-left, 0 only-right, 0 skipped
$ echo $?
1
```

## Cloud-only files

Cloud services that use the Windows Cloud Files API, such as OneDrive and Dropbox, keep a placeholder on disk for a file whose data is still in the cloud. Opening or reading such a file fetches its data from the cloud.

On Windows, the tool reads the file attributes that come with the folder listing and checks 2 bits:

- `FILE_ATTRIBUTE_RECALL_ON_OPEN`, value `0x00040000`.
- `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, value `0x00400000`.

When either bit is set, the file data is never opened, with `--hash blake3` or `--verify` too:

- A size difference still counts as changed.
- Equal sizes put the pair in the skipped list, marked `cloud-only, not read`.
- Skipped pairs do not count as a difference for the exit code, because their sizes matched. The summary line always shows the skipped count.

On Linux and macOS the check compiles away, and every file is read when its size matches.

## Licence

MIT Licence. See [LICENSE](LICENSE).
