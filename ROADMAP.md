# Roadmap

Planned work after the seed, with an hour estimate per item. The total is 23.5 hours.

## 1. Nested zips, 2 hours

- What: an option to treat a `.zip` inside a tree as a folder and compare its central directory too.
- Why: back-ups and down-loads hold zips inside folders and zips inside zips. Today a changed entry deep inside shows up only as 1 changed zip, with no hint of which entry changed.

## 2. Moved and re-named files by hash, 2 hours

- What: after pairing by path, match the only-left and only-right files by size plus hash, then list them as moved instead of as 1 deletion plus 1 addition.
- Why: re-organising a folder moves files without changing their bytes. A list of 200 deletions and 200 additions hides the fact that nothing was lost.

## 3. Text diffs of changed text files, 2.5 hours

- What: with `--diff`, print a unified diff for each changed pair that reads as UTF-8 text on both sides, with a size limit.
- Why: for notes, configuration files, source code, the next question after "changed" is "what changed". Answering it in the same run saves a 2nd tool.

## 4. Ignore-lines regex for text diffs, 1 hour

- What: `--ignore-lines REGEX` drops matching lines from both sides before the text diff and before deciding that a text file changed.
- Why: generated files carry a time-stamp or a build number on 1 line. Without this option every such file shows as changed on every run.

## 5. Snap-shots, 2.5 hours

- What: `snapshot TREE FILE` saves each file's path, size, hash to a file. Either side of a compare can then be a snapshot file.
- Why: it answers "what changed since last week" without a 2nd copy of the data, and it lets a tree be checked after the original drive is gone.

## 6. WSL and Windows paths, plus how WSL sees cloud placeholders, 2.5 hours

- What: accept Windows paths such as `C:\Users\me\OneDrive` when the tool runs inside WSL, map them to `/mnt/c/...`, then find out how a cloud-only placeholder looks when read through `/mnt/c`.
- Why: the Linux build has no access to Windows file attributes, so under WSL the cloud check does not run. A compare there could fetch every cloud-only file from the cloud. The goal is to detect that case or at least warn about it.

## 7. Google Drive streaming folders listed as cloud-backed by a setting, 1.5 hours

- What: a setting, such as `--cloud-root G:\`, that marks every file below the given root as cloud-backed. Those files get the same size-only compare as Windows placeholders.
- Why: Google Drive for desktop streams files from a virtual drive, and it is not yet checked whether its files carry the 2 recall attributes that the tool checks for. A setting gives the owner of the drive a safe compare without depending on how that drive reports its files.

## 8. HTML report, 2.5 hours

- What: `--html FILE` writes a single self-contained page with the summary, the 4 lists, a show or hide filter per list.
- Why: a long text report is hard to read and hard to share. A page opens in any browser and can go to someone who does not use a terminal.

## 9. Benchmarks on large trees, 2 hours

- What: a repeatable benchmark that builds a tree of 100,000 files plus a 10 GB zip, then times a cold run, a warm run, a run with the cache.
- Why: speed claims need numbers. The benchmark also catches a slow-down before a release instead of after.

## 10. BLAKE3 and verify for zip entries, 3 hours

- What: an option that streams each zip entry's data through the decompressor in memory (never to disk), such that `--hash blake3` and `--verify` cover zip entries too.
- Why: today, a pair with a zip entry on either side falls back to CRC-32, because the tool never reads a zip entry's data. 2 different entries of equal size with the same CRC-32 therefore still count as the same.

## 11. Release v0.1.0, 2 hours

- What: write a change log, tag v0.1.0, publish binaries for the 3 platforms, publish the crate on crates.io.
- Why: a tagged release with binaries lets people use the tool without a Rust set-up, and it gives later releases a fixed point to compare against.

## Total

23.5 hours.
