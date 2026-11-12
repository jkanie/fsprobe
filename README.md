# fsprobe

A command-line tool that compares 2 trees of files. Each side is a folder or a `.zip` file, in any mix (folder vs folder, zip vs zip, folder vs zip).

## What it does

- Walks each folder recursively.
- Reads each zip file from its central directory only (names, sizes, stored CRC-32 values), with no entry ever extracted. Folder entries inside a zip are skipped.
- Pairs files by relative path, written with forward slashes on every platform.
- Marks a pair as changed when the sizes differ, without reading either file.
- Compares every size before it reads any file. It then reads only the files of the pairs whose sizes match, on as many threads as `std::thread::available_parallelism` reports for the process (or `--threads N`), never more threads than reads.
- Prints the same report for any thread count. Each list keeps the same order, whichever thread ends 1st. On a read error, the run names the same file as a run on 1 thread.
- Compares CRC-32 values when the sizes match. A folder file has its CRC-32 computed from its bytes. A zip entry uses the CRC-32 stored in the zip.
- With `--hash blake3`, compares 2 folder files of equal size by their BLAKE3 digests instead. CRC-32 has only 32 bits. A file can also be crafted to match any given CRC-32. BLAKE3 is a collision-resistant hash.
- With `--verify`, compares 2 folder files of equal size byte for byte and names the offset of the 1st byte that differs. It wins over `--hash`, because a byte-for-byte compare needs no hash.
- Keeps CRC-32 for a pair with a zip entry on either side, even with `--hash blake3` or `--verify`, because the tool never extracts a zip entry.
- Lists changed files, files only on the left, files only on the right, then a summary line with counts of same, changed, only-left, only-right, skipped files.
- Lists each symbolic link inside a folder in its own section, with the path that the link holds. Without `--follow-links`, a link is never followed and never compared, so links alone do not make 2 trees differ. With it, a link to a file compares that file under the link's path, and a link to a folder compares the folder's files below the link's path. A link to nothing, or a link to a folder that is already on its own path (a loop), is still listed instead. `--include` and `--exclude` apply to links too.
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

The argument parser is hand-written, with no parser crate. The reads run on `std::thread::scope` threads from the standard library, with no thread-pool crate.

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
  --ignore-case    Pair paths that differ only in letter case, such as a
                   Windows copy of a Linux tree.
  --hash NAME      crc32 (the default) or blake3, for 2 folder files.
  --verify         Compare 2 folder files byte for byte, wins over --hash.
  --cache FILE     Take digests of unchanged folder files from FILE, and add
                   new ones to it (created when missing).
  --threads N      Read files on at most N threads (default: 1 per CPU).
  --follow-links   Compare what each symbolic link points to, instead of
                   listing the link. A link to nothing, or 1 that loops,
                   is still listed.
  --json           Print a JSON object instead of text.
  -q, --quiet      Print nothing (wins over --json). Errors still print.
  -h, --help       Print this help.
  -V, --version    Print the version.

Exit status: 0 when the trees match, 1 when they differ, 2 on an error.
```

- `--include` and `--exclude` match against the relative path, such as `docs/notes.txt`.
- With no `--include`, every path is kept.
- A path that matches an `--exclude` is dropped even when it also matches an `--include`.
- `--pair` removes every match from the file-name (the last path component) before pairing so that time-stamped versions of a file pair up. Folder names stay as they are.
- `--ignore-case` pairs paths (folders and file-names, after `--pair`) that differ only in letter case. The report still shows each side's own spelling. 2 paths in 1 tree that differ only in case stop the run with exit status 2, since both would pair with the same file.
- When 2 files on 1 side end up with the same pairing key, the run stops with exit code 2 and names both files.
- `--hash` takes `crc32` (the default) or `blake3`, as `--hash blake3` or `--hash=blake3`.
- `--verify` stops reading a pair at the 1st byte that differs. Only 2 equal files are read to the end.
- In the JSON report, a pair changed under `--hash blake3` has `"reason": "blake3"` with `left_blake3` and `right_blake3`. A pair changed under `--verify` has `"reason": "bytes"` with `offset`.
- `-q` or `--quiet` prints nothing, for a script that needs only the exit status (0 when the trees match, 1 when they differ). It wins over `--json`. An error still prints its message to stderr and exits with 2. A `--cache` file is still saved.
- `--threads N` caps the reads at N threads, such as `--threads 1` to read 1 file at a time from a spinning back-up drive or a network share. The default is the count that `std::thread::available_parallelism` reports. The report is the same for every N.
- `--cache FILE` keeps the CRC-32 and BLAKE3 digests of folder files in a JSON file, keyed by absolute path with the size and the modified time-stamp in nanoseconds. A re-run takes the digest of each file whose size and time-stamp did not change from that file, so a 2nd run over the same backup drive reads only the files that changed. See [Digest cache](#digest-cache).

## Digest cache

`fsprobe --cache digests.json LEFT RIGHT` prints 1 more line after the summary, such as `cache: 5210 from the cache, 14 read`. With `--json`, the report gains a `cache` object with `file`, `from_cache`, `read`. The rest of the report is the same with or without the cache.

- A file whose size or time-stamp changed is read again, and its new digest replaces the old one.
- A digest goes into the cache only when the file was last modified at least 2 seconds before the read began, and still has the same size and time-stamp after the read. A file written during the run, or in the same clock tick as the read, is read every time until it settles.
- `--verify` reads both files every time and never uses the cache. Zip entries need no cache, because their CRC-32 comes from the zip's central directory.
- The cache is written to a temporary file next to it, which then replaces it, so a run that stops half-way leaves the old cache whole. A run that read no new digest leaves the file untouched.
- A file that is not a cache (bad JSON, another version, a bad digest) stops the run with exit code 2, and the file is left as it was.
- Entries are never removed. A cache shared by many folders keeps 1 entry per file ever read.
- The cache trusts the size and the time-stamp. A tool that writes new bytes of the same size and then sets the old time-stamp back hides the change from the cache. Use `--verify` for a check that reads every byte.
- Paths that are not valid UTF-8, and time-stamps before 1970, are read every time.

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
  "links_left": [],
  "links_right": [],
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
