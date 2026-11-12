//! Compare 2 trees by relative path, size and CRC-32.
//!
//! Each side is a folder or a `.zip` file. Folders are walked recursively.
//! Zip files are read from their central directory only: names, sizes and
//! stored CRC-32 values, with no entry ever extracted.
//!
//! For 2 folder files of equal size, `--hash blake3` compares BLAKE3 digests
//! and `--verify` compares the bytes themselves.
//!
//! Every size is compared before any file is read. The reads for the pairs of
//! equal size then run on as many threads as
//! `std::thread::available_parallelism` reports.
//!
//! With `--cache FILE`, a folder file whose size and modified time match the
//! cache takes its CRC-32 or BLAKE3 digest from there instead of being read.

use std::collections::btree_map::{BTreeMap, Entry as MapEntry};
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, BufReader, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};
use walkdir::WalkDir;

/// Exit code when the 2 trees match.
pub const EXIT_MATCH: i32 = 0;
/// Exit code when the 2 trees differ.
pub const EXIT_DIFFER: i32 = 1;
/// Exit code on an error.
pub const EXIT_ERROR: i32 = 2;

const BIN: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Text shown for a pair whose data was never read.
pub const CLOUD_ONLY_NOTE: &str = "cloud-only, not read";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Which side of the comparison a value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Left => "left",
            Side::Right => "right",
        })
    }
}

/// Everything that ends a run with exit code 2.
#[derive(Debug)]
pub enum Error {
    /// The command line is not valid.
    Usage(String),
    /// A `--include`, `--exclude` or `--pair` pattern does not compile.
    BadRegex {
        flag: &'static str,
        pattern: String,
        message: String,
    },
    /// A LEFT or RIGHT path does not exist.
    NotFound(PathBuf),
    /// A LEFT or RIGHT path is neither a folder nor a `.zip` file.
    NotATree(PathBuf),
    /// Reading a file or folder failed.
    Io { path: PathBuf, source: io::Error },
    /// A zip file could not be read.
    Zip { path: PathBuf, message: String },
    /// 2 files on 1 side map to the same pairing key.
    Collision {
        side: Side,
        key: String,
        first: String,
        second: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(msg) => write!(f, "{msg}"),
            Error::BadRegex {
                flag,
                pattern,
                message,
            } => write!(f, "invalid {flag} pattern '{pattern}': {message}"),
            Error::NotFound(path) => write!(f, "{}: not found", path.display()),
            Error::NotATree(path) => {
                write!(f, "{}: not a folder or a .zip file", path.display())
            }
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Zip { path, message } => {
                write!(f, "{}: cannot read zip: {message}", path.display())
            }
            Error::Collision {
                side,
                key,
                first,
                second,
            } => write!(
                f,
                "{side} side: '{first}' and '{second}' both pair as '{key}'"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

// ---------------------------------------------------------------------------
// Tree listing
// ---------------------------------------------------------------------------

/// Where the CRC-32 of an entry comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checksum {
    /// The CRC-32 stored in a zip central directory.
    Stored(u32),
    /// A file on disk. Its CRC-32 is computed only when sizes match.
    OnDisk(PathBuf),
    /// A cloud-only placeholder. Its data is never read.
    CloudOnly,
}

/// 1 file in a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Relative path with forward slashes.
    pub path: String,
    /// Size in bytes (uncompressed size for a zip entry).
    pub size: u64,
    /// Where the CRC-32 comes from.
    pub checksum: Checksum,
}

/// Lists every file in a folder or a `.zip` file.
pub fn list_tree(root: &Path) -> Result<Vec<Entry>, Error> {
    let meta = match std::fs::metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Error::NotFound(root.to_path_buf()))
        }
        Err(e) => return Err(io_error(root, e)),
    };
    if meta.is_dir() {
        list_folder(root)
    } else if meta.is_file() && has_zip_extension(root) {
        list_zip(root)
    } else {
        Err(Error::NotATree(root.to_path_buf()))
    }
}

/// A symbolic link inside a folder. It is listed, never followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Relative path with forward slashes.
    pub path: String,
    /// The path that the link holds, as written in the link.
    pub target: String,
}

/// Lists the symbolic links in a folder, sorted by path, without following
/// them. A zip file holds no links that the tool reads, so it gives none.
pub fn list_links(root: &Path) -> Result<Vec<Link>, Error> {
    let meta = std::fs::metadata(root).map_err(|e| io_error(root, e))?;
    if !meta.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for item in WalkDir::new(root).follow_links(false).min_depth(1) {
        let item = item.map_err(|e| {
            let path = e.path().unwrap_or(root).to_path_buf();
            Error::Io {
                path,
                source: e.into(),
            }
        })?;
        if !item.path_is_symlink() {
            continue;
        }
        let target = std::fs::read_link(item.path()).map_err(|e| io_error(item.path(), e))?;
        out.push(Link {
            path: relative_path(root, item.path()),
            target: target.to_string_lossy().into_owned(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn has_zip_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

fn list_folder(root: &Path) -> Result<Vec<Entry>, Error> {
    let mut out = Vec::new();
    // follow_links(false) matters on Windows: walkdir then keeps the metadata
    // that came from the directory listing itself, so the attribute check
    // below never opens a placeholder file.
    for item in WalkDir::new(root).follow_links(false) {
        let item = item.map_err(|e| {
            let path = e.path().unwrap_or(root).to_path_buf();
            Error::Io {
                path,
                source: e.into(),
            }
        })?;
        if !item.file_type().is_file() {
            continue;
        }
        let meta = item
            .metadata()
            .map_err(|e| io_error(item.path(), e.into()))?;
        let checksum = if is_cloud_only(&meta) {
            Checksum::CloudOnly
        } else {
            Checksum::OnDisk(item.path().to_path_buf())
        };
        out.push(Entry {
            path: relative_path(root, item.path()),
            size: meta.len(),
            checksum,
        });
    }
    Ok(out)
}

/// Joins the components of `path` below `root` with forward slashes.
fn relative_path(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn list_zip(path: &Path) -> Result<Vec<Entry>, Error> {
    let zip_error = |e: zip::result::ZipError| Error::Zip {
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    let file = File::open(path).map_err(|e| io_error(path, e))?;
    // ZipArchive::new reads the central directory only.
    let archive = zip::ZipArchive::new(BufReader::new(file)).map_err(zip_error)?;
    let mut out = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        // by_index_data returns central directory data and never touches
        // the entry's compressed bytes.
        let entry = archive.by_index_data(index).map_err(zip_error)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().map_err(zip_error)?;
        out.push(Entry {
            path: normalize_zip_name(&name),
            size: entry.size(),
            checksum: Checksum::Stored(entry.crc32()),
        });
    }
    Ok(out)
}

/// Turns a zip entry name into the same relative-path form as a folder file.
fn normalize_zip_name(name: &str) -> String {
    let name = name.replace('\\', "/");
    let mut rest = name.as_str();
    loop {
        if let Some(r) = rest.strip_prefix("./") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix('/') {
            rest = r;
        } else {
            return rest.to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// Cloud-only placeholders (Windows Cloud Files API)
// ---------------------------------------------------------------------------

// Values from Microsoft's "File Attribute Constants" page:
// https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants
#[cfg(any(windows, test))]
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
#[cfg(any(windows, test))]
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// True when the attribute bits mark a file whose data lives in the cloud.
#[cfg(any(windows, test))]
fn attributes_are_cloud_only(attributes: u32) -> bool {
    attributes & (FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(windows)]
fn is_cloud_only(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    attributes_are_cloud_only(meta.file_attributes())
}

#[cfg(not(windows))]
fn is_cloud_only(_meta: &Metadata) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Filtering and pairing
// ---------------------------------------------------------------------------

/// Filters and pairing rules from the command line.
#[derive(Debug, Default)]
pub struct Options {
    /// Keep only paths that match at least 1 of these (when not empty).
    pub include: Vec<Regex>,
    /// Drop paths that match any of these. Exclude wins over include.
    pub exclude: Vec<Regex>,
    /// Remove every match of each of these from the file name before pairing.
    pub pair: Vec<Regex>,
}

impl Options {
    /// True when `path` passes the include and exclude filters.
    pub fn keeps(&self, path: &str) -> bool {
        if self.exclude.iter().any(|re| re.is_match(path)) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|re| re.is_match(path))
    }

    /// The key that pairs `path` with its partner on the other side.
    ///
    /// Only the last path component changes. Each `--pair` regex runs in turn
    /// and every match is removed.
    pub fn pair_key(&self, path: &str) -> String {
        if self.pair.is_empty() {
            return path.to_string();
        }
        let (dir, name) = match path.rfind('/') {
            Some(i) => path.split_at(i + 1),
            None => ("", path),
        };
        let mut name = name.to_string();
        for re in &self.pair {
            name = re.replace_all(&name, "").into_owned();
        }
        format!("{dir}{name}")
    }
}

/// Filters `entries` and keys them for pairing.
///
/// Fails when 2 entries on the same side end up with the same key.
pub fn index(
    entries: Vec<Entry>,
    options: &Options,
    side: Side,
) -> Result<BTreeMap<String, Entry>, Error> {
    let mut map = BTreeMap::new();
    for entry in entries {
        if !options.keeps(&entry.path) {
            continue;
        }
        match map.entry(options.pair_key(&entry.path)) {
            MapEntry::Vacant(slot) => {
                slot.insert(entry);
            }
            MapEntry::Occupied(slot) => {
                let mut names = [slot.get().path.clone(), entry.path];
                names.sort();
                let [first, second] = names;
                return Err(Error::Collision {
                    side,
                    key: slot.key().clone(),
                    first,
                    second,
                });
            }
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Comparing
// ---------------------------------------------------------------------------

/// Computes the CRC-32 of a file's bytes.
pub fn crc32_of_file(path: &Path) -> Result<u32, Error> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(path, e)),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Computes the BLAKE3 digest of a file's bytes.
pub fn blake3_of_file(path: &Path) -> Result<[u8; 32], Error> {
    let mut file = File::open(path).map_err(|e| io_error(path, e))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(path, e)),
        };
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Reads until `buf` is full or the file ends, and returns the count read.
fn fill(file: &mut File, path: &Path, buf: &mut [u8]) -> Result<usize, Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(path, e)),
        }
    }
    Ok(filled)
}

/// Compares 2 files byte for byte.
///
/// Returns `None` when every byte matches, or the offset of the 1st byte that
/// differs. When 1 file ends before the other, the offset is where it ends.
pub fn first_difference(left: &Path, right: &Path) -> Result<Option<u64>, Error> {
    let mut lf = File::open(left).map_err(|e| io_error(left, e))?;
    let mut rf = File::open(right).map_err(|e| io_error(right, e))?;
    let mut lbuf = vec![0u8; 64 * 1024];
    let mut rbuf = vec![0u8; 64 * 1024];
    let mut offset = 0u64;
    loop {
        let ln = fill(&mut lf, left, &mut lbuf)?;
        let rn = fill(&mut rf, right, &mut rbuf)?;
        let common = ln.min(rn);
        if let Some(i) = (0..common).find(|&i| lbuf[i] != rbuf[i]) {
            return Ok(Some(offset + i as u64));
        }
        if ln != rn {
            return Ok(Some(offset + common as u64));
        }
        if ln == 0 {
            return Ok(None);
        }
        offset += ln as u64;
    }
}

/// Which digest compares 2 folder files of equal size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HashKind {
    /// CRC-32, the default.
    #[default]
    Crc32,
    /// BLAKE3, a collision-resistant hash.
    Blake3,
}

/// How a pair of equal size is checked.
///
/// `hash` and `verify` apply only when both files are folder files on disk.
/// A pair with a zip entry on either side always compares CRC-32 values,
/// because a zip entry is never extracted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    /// The digest for 2 folder files.
    pub hash: HashKind,
    /// Compare 2 folder files byte for byte. This wins over `hash`.
    pub verify: bool,
}

/// 2 paired files, 1 from each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub left: String,
    pub right: String,
}

impl Pair {
    fn label(&self) -> String {
        if self.left == self.right {
            self.left.clone()
        } else {
            format!("{} -> {}", self.left, self.right)
        }
    }
}

/// Why a pair counts as changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The sizes differ, so no data was read.
    Size,
    /// The sizes match and the CRC-32 values differ.
    Crc32 { left: u32, right: u32 },
    /// The sizes match and the BLAKE3 digests differ (`--hash blake3`).
    Blake3 { left: [u8; 32], right: [u8; 32] },
    /// The sizes match and the bytes differ, 1st at this offset (`--verify`).
    Bytes { offset: u64 },
}

/// Lower-case hex of a byte string.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A pair whose contents differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub pair: Pair,
    pub left_size: u64,
    pub right_size: u64,
    pub reason: Reason,
}

/// The result of comparing 2 trees. Every list is in pairing-key order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub same: Vec<Pair>,
    pub changed: Vec<Changed>,
    pub only_left: Vec<String>,
    pub only_right: Vec<String>,
    /// Pairs with equal sizes where 1 side is cloud-only, so no data was read.
    pub skipped: Vec<Pair>,
    /// Symbolic links in the left folder. They are listed, not compared.
    pub links_left: Vec<Link>,
    /// Symbolic links in the right folder. They are listed, not compared.
    pub links_right: Vec<Link>,
}

impl Report {
    /// True when no pair changed and no file is on 1 side only.
    ///
    /// Skipped pairs had equal sizes and do not count as a difference.
    /// Symbolic links are listed only, so they do not count either.
    pub fn matches(&self) -> bool {
        self.changed.is_empty() && self.only_left.is_empty() && self.only_right.is_empty()
    }

    /// The summary line, as printed at the end of the text report.
    pub fn summary_line(&self) -> String {
        format!(
            "summary: {} same, {} changed, {} only-left, {} only-right, {} skipped",
            self.same.len(),
            self.changed.len(),
            self.only_left.len(),
            self.only_right.len(),
            self.skipped.len()
        )
    }

    /// The plain-text report. Empty sections are left out.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        if !self.changed.is_empty() {
            out.push_str("changed:\n");
            for c in &self.changed {
                let detail = match c.reason {
                    Reason::Size => format!("size {} -> {} bytes", c.left_size, c.right_size),
                    Reason::Crc32 { left, right } => format!("crc32 {left:08x} -> {right:08x}"),
                    Reason::Blake3 { left, right } => {
                        format!("blake3 {} -> {}", hex(&left), hex(&right))
                    }
                    Reason::Bytes { offset } => format!("bytes differ at offset {offset}"),
                };
                out.push_str(&format!("  {}  ({detail})\n", c.pair.label()));
            }
        }
        for (title, list) in [
            ("only in left:", &self.only_left),
            ("only in right:", &self.only_right),
        ] {
            if !list.is_empty() {
                out.push_str(title);
                out.push('\n');
                for path in list {
                    out.push_str(&format!("  {path}\n"));
                }
            }
        }
        if !self.skipped.is_empty() {
            out.push_str("skipped:\n");
            for p in &self.skipped {
                out.push_str(&format!("  {}  ({CLOUD_ONLY_NOTE})\n", p.label()));
            }
        }
        if !self.links_left.is_empty() || !self.links_right.is_empty() {
            out.push_str("symbolic links (not compared):\n");
            for (side, links) in [("left", &self.links_left), ("right", &self.links_right)] {
                for link in links {
                    out.push_str(&format!("  {side:<5}  {} -> {}\n", link.path, link.target));
                }
            }
        }
        out.push_str(&self.summary_line());
        out.push('\n');
        out
    }

    /// The JSON report. Keys come out in sorted order.
    pub fn to_json(&self, left_root: &str, right_root: &str) -> Value {
        let changed: Vec<Value> = self
            .changed
            .iter()
            .map(|c| {
                let mut item = json!({
                    "left": c.pair.left,
                    "right": c.pair.right,
                    "left_size": c.left_size,
                    "right_size": c.right_size,
                });
                match c.reason {
                    Reason::Size => {
                        item["reason"] = json!("size");
                    }
                    Reason::Crc32 { left, right } => {
                        item["reason"] = json!("crc32");
                        item["left_crc32"] = json!(format!("{left:08x}"));
                        item["right_crc32"] = json!(format!("{right:08x}"));
                    }
                    Reason::Blake3 { left, right } => {
                        item["reason"] = json!("blake3");
                        item["left_blake3"] = json!(hex(&left));
                        item["right_blake3"] = json!(hex(&right));
                    }
                    Reason::Bytes { offset } => {
                        item["reason"] = json!("bytes");
                        item["offset"] = json!(offset);
                    }
                }
                item
            })
            .collect();
        let skipped: Vec<Value> = self
            .skipped
            .iter()
            .map(|p| json!({ "left": p.left, "right": p.right, "reason": CLOUD_ONLY_NOTE }))
            .collect();
        let links = |list: &[Link]| -> Vec<Value> {
            list.iter()
                .map(|l| json!({ "path": l.path, "target": l.target }))
                .collect()
        };
        json!({
            "left": left_root,
            "links_left": links(&self.links_left),
            "links_right": links(&self.links_right),
            "right": right_root,
            "match": self.matches(),
            "changed": changed,
            "only_left": self.only_left,
            "only_right": self.only_right,
            "skipped": skipped,
            "summary": {
                "same": self.same.len(),
                "changed": self.changed.len(),
                "only_left": self.only_left.len(),
                "only_right": self.only_right.len(),
                "skipped": self.skipped.len(),
            },
        })
    }
}

/// Compares 2 keyed trees with the default check (CRC-32).
///
/// Sizes are compared 1st. A CRC-32 is computed only for a pair of equal
/// size, and never for a cloud-only file.
pub fn compare_maps(
    left: &BTreeMap<String, Entry>,
    right: &BTreeMap<String, Entry>,
) -> Result<Report, Error> {
    compare_maps_with(left, right, Check::default())
}

/// Compares 2 keyed trees, checking each pair of equal size as `check` asks.
///
/// Every size is compared before any file is read. The reads then run on
/// [`default_threads`] threads. A cloud-only file is never read, whatever the
/// check.
pub fn compare_maps_with(
    left: &BTreeMap<String, Entry>,
    right: &BTreeMap<String, Entry>,
    check: Check,
) -> Result<Report, Error> {
    compare_maps_threads(left, right, check, default_threads())
}

/// The thread count for the reads: the value that
/// `std::thread::available_parallelism` reports, or 1 when that call fails.
pub fn default_threads() -> NonZeroUsize {
    thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
}

/// Compares 2 keyed trees like [`compare_maps_with`], reading files on at
/// most `threads` threads.
///
/// The report is the same for every thread count. On a read error, the error
/// is the one a single thread meets 1st, because tasks are numbered in
/// pairing-key order (left side before right side) and the lowest-numbered
/// error wins.
pub fn compare_maps_threads(
    left: &BTreeMap<String, Entry>,
    right: &BTreeMap<String, Entry>,
    check: Check,
    threads: NonZeroUsize,
) -> Result<Report, Error> {
    compare_maps_cached(left, right, check, threads, None)
}

/// Compares 2 keyed trees like [`compare_maps_threads`]. With a cache, each
/// CRC-32 or BLAKE3 of a folder file comes from `cache` when the file's size
/// and modified time match, and new digests go into it. The report is the
/// same with or without a cache.
pub fn compare_maps_cached(
    left: &BTreeMap<String, Entry>,
    right: &BTreeMap<String, Entry>,
    check: Check,
    threads: NonZeroUsize,
    cache: Option<&HashCache>,
) -> Result<Report, Error> {
    let plan = plan(left, right, check);
    let done = run_tasks(&plan.tasks, threads.get(), cache)?;
    Ok(plan.finish(&done))
}

/// 1 read that a pair of equal size needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task<'a> {
    Crc32(&'a Path),
    Blake3(&'a Path),
    Verify(&'a Path, &'a Path),
}

/// The result of 1 [`Task`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Done {
    Crc32(u32),
    Blake3([u8; 32]),
    Verify(Option<u64>),
}

impl Task<'_> {
    fn run(self, cache: Option<&HashCache>) -> Result<Done, Error> {
        match (self, cache) {
            (Task::Crc32(path), Some(cache)) => cache.crc32(path).map(Done::Crc32),
            (Task::Blake3(path), Some(cache)) => cache.blake3(path).map(Done::Blake3),
            (Task::Crc32(path), None) => crc32_of_file(path).map(Done::Crc32),
            (Task::Blake3(path), None) => blake3_of_file(path).map(Done::Blake3),
            // A byte-for-byte compare reads both files every time.
            (Task::Verify(l, r), _) => first_difference(l, r).map(Done::Verify),
        }
    }
}

/// 1 side's CRC-32, known from the listing or still to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crc {
    /// Stored in a zip, or `None` for a cloud-only file.
    Known(Option<u32>),
    /// Computed by the task with this number.
    Task(usize),
}

/// What a paired key needs before it can go into the report.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// The sizes differ. Settled without a read.
    Changed(Changed),
    /// A cloud-only side under `--hash blake3` or `--verify`. Settled without
    /// a read.
    Skipped(Pair),
    /// Waits for the CRC-32 task of each side on disk.
    Crc32 {
        pair: Pair,
        size: u64,
        left: Crc,
        right: Crc,
    },
    /// Waits for the BLAKE3 task of each side.
    Blake3 {
        pair: Pair,
        size: u64,
        left: usize,
        right: usize,
    },
    /// Waits for 1 byte-for-byte task.
    Verify { pair: Pair, size: u64, task: usize },
}

/// Every pair in pairing-key order, plus the reads they need.
#[derive(Debug)]
struct Plan<'a> {
    steps: Vec<Step>,
    tasks: Vec<Task<'a>>,
    only_left: Vec<String>,
    only_right: Vec<String>,
}

/// Compares every size and lists the reads that the pairs of equal size need.
///
/// Nothing is read here. A pair whose sizes differ gets no task.
fn plan<'a>(
    left: &'a BTreeMap<String, Entry>,
    right: &'a BTreeMap<String, Entry>,
    check: Check,
) -> Plan<'a> {
    let mut plan = Plan {
        steps: Vec::new(),
        tasks: Vec::new(),
        only_left: Vec::new(),
        only_right: Vec::new(),
    };
    let strong = check.verify || check.hash == HashKind::Blake3;
    for (key, l) in left {
        let Some(r) = right.get(key) else {
            plan.only_left.push(l.path.clone());
            continue;
        };
        let pair = Pair {
            left: l.path.clone(),
            right: r.path.clone(),
        };
        if l.size != r.size {
            plan.steps.push(Step::Changed(Changed {
                pair,
                left_size: l.size,
                right_size: r.size,
                reason: Reason::Size,
            }));
            continue;
        }
        let size = l.size;
        // Under the new checks, a pair with a cloud-only side is skipped before
        // anything is read. The default check keeps its old order, which reads
        // the other side's CRC-32 1st.
        if strong && (l.checksum == Checksum::CloudOnly || r.checksum == Checksum::CloudOnly) {
            plan.steps.push(Step::Skipped(pair));
            continue;
        }
        let mut add = |task| {
            plan.tasks.push(task);
            plan.tasks.len() - 1
        };
        let step = match (&l.checksum, &r.checksum) {
            (Checksum::OnDisk(lp), Checksum::OnDisk(rp)) if check.verify => Step::Verify {
                pair,
                size,
                task: add(Task::Verify(lp, rp)),
            },
            (Checksum::OnDisk(lp), Checksum::OnDisk(rp)) if strong => Step::Blake3 {
                pair,
                size,
                left: add(Task::Blake3(lp)),
                right: add(Task::Blake3(rp)),
            },
            (lc, rc) => {
                let mut crc = |checksum: &'a Checksum| match checksum {
                    Checksum::Stored(crc) => Crc::Known(Some(*crc)),
                    Checksum::OnDisk(path) => Crc::Task(add(Task::Crc32(path))),
                    Checksum::CloudOnly => Crc::Known(None),
                };
                let left = crc(lc);
                let right = crc(rc);
                Step::Crc32 {
                    pair,
                    size,
                    left,
                    right,
                }
            }
        };
        plan.steps.push(step);
    }
    for (key, r) in right {
        if !left.contains_key(key) {
            plan.only_right.push(r.path.clone());
        }
    }
    plan
}

impl Plan<'_> {
    /// Builds the report from the task results, in pairing-key order.
    fn finish(self, done: &[Done]) -> Report {
        let crc = |c: Crc| match c {
            Crc::Known(value) => value,
            Crc::Task(i) => match done[i] {
                Done::Crc32(value) => Some(value),
                other => unreachable!("task {i} is a CRC-32, got {other:?}"),
            },
        };
        let blake3 = |i: usize| match done[i] {
            Done::Blake3(digest) => digest,
            other => unreachable!("task {i} is a BLAKE3, got {other:?}"),
        };
        let mut report = Report {
            only_left: self.only_left,
            only_right: self.only_right,
            ..Report::default()
        };
        let changed = |pair, size, reason| Changed {
            pair,
            left_size: size,
            right_size: size,
            reason,
        };
        for step in self.steps {
            let (pair, size, reason) = match step {
                Step::Changed(c) => {
                    report.changed.push(c);
                    continue;
                }
                Step::Skipped(pair) => {
                    report.skipped.push(pair);
                    continue;
                }
                Step::Crc32 {
                    pair,
                    size,
                    left,
                    right,
                } => {
                    let (Some(lc), Some(rc)) = (crc(left), crc(right)) else {
                        report.skipped.push(pair);
                        continue;
                    };
                    let reason = (lc != rc).then_some(Reason::Crc32 {
                        left: lc,
                        right: rc,
                    });
                    (pair, size, reason)
                }
                Step::Blake3 {
                    pair,
                    size,
                    left,
                    right,
                } => {
                    let (lh, rh) = (blake3(left), blake3(right));
                    let reason = (lh != rh).then_some(Reason::Blake3 {
                        left: lh,
                        right: rh,
                    });
                    (pair, size, reason)
                }
                Step::Verify { pair, size, task } => {
                    let reason = match done[task] {
                        Done::Verify(offset) => offset.map(|offset| Reason::Bytes { offset }),
                        other => unreachable!("task {task} is a verify, got {other:?}"),
                    };
                    (pair, size, reason)
                }
            };
            match reason {
                None => report.same.push(pair),
                Some(reason) => report.changed.push(changed(pair, size, reason)),
            }
        }
        report
    }
}

/// Runs `tasks` on at most `threads` threads and returns the results in task
/// order.
///
/// Each thread takes the next task number from a shared counter, so a slow
/// file holds up only its own thread. After an error, no thread starts a task
/// numbered above it, and the lowest-numbered error is returned.
fn run_tasks(
    tasks: &[Task<'_>],
    threads: usize,
    cache: Option<&HashCache>,
) -> Result<Vec<Done>, Error> {
    let threads = threads.min(tasks.len()).max(1);
    let next = AtomicUsize::new(0);
    let stop = AtomicUsize::new(usize::MAX);
    let work = || {
        let mut out = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= tasks.len() || i > stop.load(Ordering::Relaxed) {
                return out;
            }
            let result = tasks[i].run(cache);
            if result.is_err() {
                stop.fetch_min(i, Ordering::Relaxed);
            }
            out.push((i, result));
        }
    };
    let batches = if threads == 1 {
        vec![work()]
    } else {
        thread::scope(|s| {
            let handles: Vec<_> = (0..threads).map(|_| s.spawn(work)).collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                })
                .collect()
        })
    };
    let mut slots: Vec<Option<Result<Done, Error>>> = (0..tasks.len()).map(|_| None).collect();
    for (i, result) in batches.into_iter().flatten() {
        slots[i] = Some(result);
    }
    // Every task numbered below the lowest error ran, so the slots fill up in
    // order until that error.
    slots
        .into_iter()
        .map(|slot| slot.expect("every task before the 1st error runs"))
        .collect()
}

/// Lists, filters, pairs and compares 2 trees with the default check.
pub fn compare_trees(left: &Path, right: &Path, options: &Options) -> Result<Report, Error> {
    compare_trees_with(left, right, options, Check::default())
}

/// Lists, filters, pairs and compares 2 trees, checking pairs as `check` asks.
pub fn compare_trees_with(
    left: &Path,
    right: &Path,
    options: &Options,
    check: Check,
) -> Result<Report, Error> {
    compare_trees_cached(left, right, options, check, None)
}

/// Lists, filters, pairs and compares 2 trees like [`compare_trees_with`],
/// with digests taken from and added to `cache`.
pub fn compare_trees_cached(
    left: &Path,
    right: &Path,
    options: &Options,
    check: Check,
    cache: Option<&HashCache>,
) -> Result<Report, Error> {
    compare_trees_threads(left, right, options, check, default_threads(), cache)
}

/// Lists, filters, pairs, compares 2 trees like [`compare_trees_cached`],
/// reading files on at most `threads` threads. The report is the same for
/// every thread count.
pub fn compare_trees_threads(
    left: &Path,
    right: &Path,
    options: &Options,
    check: Check,
    threads: NonZeroUsize,
    cache: Option<&HashCache>,
) -> Result<Report, Error> {
    let l = index(list_tree(left)?, options, Side::Left)?;
    let r = index(list_tree(right)?, options, Side::Right)?;
    let mut report = compare_maps_cached(&l, &r, check, threads, cache)?;
    let kept = |links: Vec<Link>| -> Vec<Link> {
        links
            .into_iter()
            .filter(|l| options.keeps(&l.path))
            .collect()
    };
    report.links_left = kept(list_links(left)?);
    report.links_right = kept(list_links(right)?);
    Ok(report)
}

// ---------------------------------------------------------------------------
// Digest cache
// ---------------------------------------------------------------------------

/// The cache file format. A file with another version is refused.
const CACHE_VERSION: u64 = 1;

/// A digest is stored only when the file was last modified this long before
/// the read began. A write in the same clock tick as the read could otherwise
/// leave the same size and time-stamp with new bytes (the "racy clean" case).
pub const CACHE_SETTLE: Duration = Duration::from_secs(2);

/// What the cache knows about 1 file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Cached {
    size: u64,
    mtime_ns: u64,
    crc32: Option<u32>,
    blake3: Option<[u8; 32]>,
}

/// A digest cache, keyed by absolute path, checked by size and modified time.
///
/// Load it with [`HashCache::load`], pass it to [`compare_trees_cached`], then
/// [`HashCache::save`] it. Threads share it through a mutex that is held only
/// for a lookup or an insert, never during a read.
#[derive(Debug)]
pub struct HashCache {
    file: PathBuf,
    entries: Mutex<BTreeMap<String, Cached>>,
    changed: AtomicUsize,
    hits: AtomicUsize,
    reads: AtomicUsize,
}

/// The cache key and the size and time-stamp of a file, or `None` when the
/// file cannot be cached: a path that is not UTF-8, or a time-stamp before
/// 1970 or past the year 2554.
fn cache_stamp(path: &Path) -> Result<Option<(String, u64, u64, SystemTime)>, Error> {
    let meta = std::fs::metadata(path).map_err(|e| io_error(path, e))?;
    let Ok(modified) = meta.modified() else {
        return Ok(None);
    };
    let Some(ns) = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
    else {
        return Ok(None);
    };
    let absolute = std::path::absolute(path).map_err(|e| io_error(path, e))?;
    Ok(absolute
        .to_str()
        .map(|key| (key.to_string(), meta.len(), ns, modified)))
}

fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl HashCache {
    /// An empty cache that [`HashCache::save`] writes to `file`.
    pub fn new(file: &Path) -> HashCache {
        HashCache {
            file: file.to_path_buf(),
            entries: Mutex::new(BTreeMap::new()),
            changed: AtomicUsize::new(0),
            hits: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        }
    }

    /// Reads `file`, or starts empty when it does not exist yet.
    pub fn load(file: &Path) -> Result<HashCache, Error> {
        let cache = HashCache::new(file);
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cache),
            Err(e) => return Err(io_error(file, e)),
        };
        let bad = |why: &str| {
            Error::Usage(format!(
                "{} is not a {BIN} cache file ({why}). Delete it or name another file",
                file.display()
            ))
        };
        let value: Value = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
        if value.get("version").and_then(Value::as_u64) != Some(CACHE_VERSION) {
            return Err(bad("unknown version"));
        }
        let files = value
            .get("files")
            .and_then(Value::as_object)
            .ok_or_else(|| bad("no files object"))?;
        let mut entries = BTreeMap::new();
        for (path, item) in files {
            let number = |key| item.get(key).and_then(Value::as_u64);
            let (Some(size), Some(mtime_ns)) = (number("size"), number("mtime_ns")) else {
                return Err(bad(&format!("entry {path} lacks size or mtime_ns")));
            };
            let text = |key| item.get(key).and_then(Value::as_str);
            let crc32 = match text("crc32") {
                None => None,
                Some(t) => Some(u32::from_be_bytes(
                    parse_hex::<4>(t).ok_or_else(|| bad(&format!("bad crc32 for {path}")))?,
                )),
            };
            let blake3 = match text("blake3") {
                None => None,
                Some(t) => {
                    Some(parse_hex::<32>(t).ok_or_else(|| bad(&format!("bad blake3 for {path}")))?)
                }
            };
            entries.insert(
                path.clone(),
                Cached {
                    size,
                    mtime_ns,
                    crc32,
                    blake3,
                },
            );
        }
        *cache.entries.lock().expect("cache lock") = entries;
        Ok(cache)
    }

    /// Writes the cache when this run added or changed an entry. The data goes
    /// to a temporary file next to the cache, which then replaces it, so a run
    /// that stops half-way never leaves a broken cache.
    pub fn save(&self) -> Result<(), Error> {
        if self.changed.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        let entries = self.entries.lock().expect("cache lock");
        let mut files = serde_json::Map::new();
        for (path, c) in entries.iter() {
            let mut item = json!({ "size": c.size, "mtime_ns": c.mtime_ns });
            if let Some(crc) = c.crc32 {
                item["crc32"] = json!(format!("{crc:08x}"));
            }
            if let Some(digest) = c.blake3 {
                item["blake3"] = json!(hex(&digest));
            }
            files.insert(path.clone(), item);
        }
        let text = serde_json::to_string(&json!({ "version": CACHE_VERSION, "files": files }))
            .expect("a JSON value always serializes");
        let mut tmp = self.file.clone().into_os_string();
        tmp.push(format!(".tmp{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text).map_err(|e| io_error(&tmp, e))?;
        std::fs::rename(&tmp, &self.file).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io_error(&self.file, e)
        })
    }

    /// How many digests came from the cache in this run.
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    /// How many files this run read to compute a digest.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    /// How many entries the cache holds.
    pub fn len(&self) -> usize {
        self.entries.lock().expect("cache lock").len()
    }

    /// True when the cache holds no entry.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The CRC-32 of a file, from the cache when its size and time-stamp match.
    pub fn crc32(&self, path: &Path) -> Result<u32, Error> {
        self.digest(path, |c| c.crc32, |c, v| c.crc32 = Some(v), crc32_of_file)
    }

    /// The BLAKE3 digest of a file, from the cache when its size and time-stamp
    /// match.
    pub fn blake3(&self, path: &Path) -> Result<[u8; 32], Error> {
        self.digest(
            path,
            |c| c.blake3,
            |c, v| c.blake3 = Some(v),
            blake3_of_file,
        )
    }

    fn digest<T: Copy>(
        &self,
        path: &Path,
        get: impl Fn(&Cached) -> Option<T>,
        set: impl Fn(&mut Cached, T),
        compute: impl Fn(&Path) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let Some((key, size, mtime_ns, modified)) = cache_stamp(path)? else {
            self.reads.fetch_add(1, Ordering::Relaxed);
            return compute(path);
        };
        {
            let entries = self.entries.lock().expect("cache lock");
            if let Some(c) = entries.get(&key) {
                if c.size == size && c.mtime_ns == mtime_ns {
                    if let Some(value) = get(c) {
                        self.hits.fetch_add(1, Ordering::Relaxed);
                        return Ok(value);
                    }
                }
            }
        }
        let started = SystemTime::now();
        self.reads.fetch_add(1, Ordering::Relaxed);
        let value = compute(path)?;
        // Store only a digest of bytes that had settled before the read began,
        // and only when the file still has the size and time-stamp of before.
        let settled = started
            .duration_since(modified)
            .is_ok_and(|age| age >= CACHE_SETTLE);
        let unchanged = cache_stamp(path)?.is_some_and(|(_, s, m, _)| s == size && m == mtime_ns);
        if settled && unchanged {
            let mut entries = self.entries.lock().expect("cache lock");
            let c = entries.entry(key).or_default();
            if c.size != size || c.mtime_ns != mtime_ns {
                *c = Cached {
                    size,
                    mtime_ns,
                    ..Cached::default()
                };
            }
            set(c, value);
            self.changed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(value)
    }
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

/// A parsed command line that asks for a comparison.
#[derive(Debug)]
pub struct Args {
    pub left: PathBuf,
    pub right: PathBuf,
    pub json: bool,
    /// `--quiet`: print nothing, the exit code alone gives the result.
    pub quiet: bool,
    pub options: Options,
    pub check: Check,
    /// The digest cache file of `--cache`.
    pub cache: Option<PathBuf>,
    /// The thread count of `--threads`, else `None` for [`default_threads`].
    pub threads: Option<NonZeroUsize>,
}

/// What the command line asks for.
#[derive(Debug)]
pub enum Command {
    Compare(Args),
    Help,
    Version,
}

/// The `--help` text.
pub fn usage() -> String {
    format!(
        "Usage: {BIN} [OPTIONS] LEFT RIGHT

Compare 2 trees. Each side is a folder or a .zip file.
Files pair up by relative path and compare by size, then CRC-32.

Options:
  --include REGEX  Keep only paths that match (repeatable).
  --exclude REGEX  Drop paths that match, wins over --include (repeatable).
  --pair REGEX     Remove matches from file-names before pairing (repeatable).
  --hash NAME      crc32 (the default) or blake3, for 2 folder files.
  --verify         Compare 2 folder files byte for byte, wins over --hash.
  --cache FILE     Take digests of unchanged folder files from FILE, and add
                   new ones to it (created when missing).
  --threads N      Read files on at most N threads (default: 1 per CPU).
  --json           Print a JSON object instead of text.
  -q, --quiet      Print nothing (wins over --json). Errors still print.
  -h, --help       Print this help.
  -V, --version    Print the version.

Exit status: 0 when the trees match, 1 when they differ, 2 on an error.
"
    )
}

fn compile(flag: &'static str, pattern: &str) -> Result<Regex, Error> {
    Regex::new(pattern).map_err(|e| Error::BadRegex {
        flag,
        pattern: pattern.to_string(),
        message: e.to_string(),
    })
}

/// Parses the arguments that follow the program name.
pub fn parse_args<I>(args: I) -> Result<Command, Error>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let mut positional: Vec<OsString> = Vec::new();
    let mut json = false;
    let mut quiet = false;
    let mut check = Check::default();
    let mut options = Options::default();
    let mut cache: Option<PathBuf> = None;
    let mut threads: Option<NonZeroUsize> = None;
    let mut options_done = false;

    while let Some(arg) = args.next() {
        if options_done {
            positional.push(arg);
            continue;
        }
        let Some(text) = arg.to_str() else {
            positional.push(arg);
            continue;
        };
        if text == "--" {
            options_done = true;
            continue;
        }
        if !text.starts_with('-') || text == "-" {
            positional.push(arg);
            continue;
        }
        let (flag, inline) = match text.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (text.to_string(), None),
        };
        let slot: &'static str = match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--json" if inline.is_none() => {
                json = true;
                continue;
            }
            "-q" | "--quiet" if inline.is_none() => {
                quiet = true;
                continue;
            }
            "--verify" if inline.is_none() => {
                check.verify = true;
                continue;
            }
            "--hash" => {
                let value = match inline {
                    Some(v) => v,
                    None => match args.next() {
                        Some(v) => v.to_string_lossy().into_owned(),
                        None => return Err(Error::Usage("--hash needs a NAME value".into())),
                    },
                };
                check.hash = match value.as_str() {
                    "crc32" => HashKind::Crc32,
                    "blake3" => HashKind::Blake3,
                    _ => {
                        return Err(Error::Usage(format!(
                            "unknown --hash '{value}', expected crc32 or blake3"
                        )))
                    }
                };
                continue;
            }
            "--cache" => {
                let value = match inline {
                    Some(v) => OsString::from(v),
                    None => match args.next() {
                        Some(v) => v,
                        None => return Err(Error::Usage("--cache needs a FILE value".into())),
                    },
                };
                if value.is_empty() {
                    return Err(Error::Usage("--cache needs a FILE value".into()));
                }
                cache = Some(PathBuf::from(value));
                continue;
            }
            "--threads" => {
                let value = match inline {
                    Some(v) => v,
                    None => match args.next() {
                        Some(v) => v.to_string_lossy().into_owned(),
                        None => return Err(Error::Usage("--threads needs an N value".into())),
                    },
                };
                threads = Some(value.parse::<NonZeroUsize>().map_err(|_| {
                    Error::Usage(format!(
                        "--threads needs a whole number of 1 or more, got '{value}'"
                    ))
                })?);
                continue;
            }
            "--include" => "--include",
            "--exclude" => "--exclude",
            "--pair" => "--pair",
            _ => return Err(Error::Usage(format!("unknown option '{text}'"))),
        };
        let value = match inline {
            Some(v) => v,
            None => match args.next() {
                Some(v) => v
                    .into_string()
                    .map_err(|_| Error::Usage(format!("{slot} needs a UTF-8 pattern")))?,
                None => return Err(Error::Usage(format!("{slot} needs a REGEX value"))),
            },
        };
        let re = compile(slot, &value)?;
        match slot {
            "--include" => options.include.push(re),
            "--exclude" => options.exclude.push(re),
            _ => options.pair.push(re),
        }
    }

    match <[OsString; 2]>::try_from(positional) {
        Ok([left, right]) => Ok(Command::Compare(Args {
            left: PathBuf::from(left),
            right: PathBuf::from(right),
            json,
            quiet,
            options,
            check,
            cache,
            threads,
        })),
        Err(got) => Err(Error::Usage(format!(
            "expected 2 paths (LEFT RIGHT), got {}",
            got.len()
        ))),
    }
}

/// Runs the tool and returns the exit code.
pub fn run<I>(args: I, out: &mut dyn Write, err: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    match try_run(args, out) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(err, "{BIN}: {e}");
            if matches!(e, Error::Usage(_)) {
                let _ = writeln!(err, "Try '{BIN} --help' for more information.");
            }
            EXIT_ERROR
        }
    }
}

fn try_run<I>(args: I, out: &mut dyn Write) -> Result<i32, Error>
where
    I: IntoIterator<Item = OsString>,
{
    let stdout_error = |e| io_error(Path::new("<stdout>"), e);
    let args = match parse_args(args)? {
        Command::Help => {
            out.write_all(usage().as_bytes()).map_err(stdout_error)?;
            return Ok(EXIT_MATCH);
        }
        Command::Version => {
            writeln!(out, "{BIN} {VERSION}").map_err(stdout_error)?;
            return Ok(EXIT_MATCH);
        }
        Command::Compare(args) => args,
    };
    let cache = args.cache.as_deref().map(HashCache::load).transpose()?;
    let report = compare_trees_threads(
        &args.left,
        &args.right,
        &args.options,
        args.check,
        args.threads.unwrap_or_else(default_threads),
        cache.as_ref(),
    )?;
    if let Some(cache) = &cache {
        cache.save()?;
    }
    if args.quiet {
        // Nothing to print: the exit code (0 match, 1 differ) is the answer.
    } else if args.json {
        let mut value = report.to_json(
            &args.left.display().to_string(),
            &args.right.display().to_string(),
        );
        if let Some(cache) = &cache {
            value["cache"] = json!({
                "file": args.cache.as_ref().map(|p| p.display().to_string()),
                "from_cache": cache.hits(),
                "read": cache.reads(),
            });
        }
        let text = serde_json::to_string_pretty(&value).expect("a JSON value always serializes");
        writeln!(out, "{text}").map_err(stdout_error)?;
    } else {
        out.write_all(report.to_text().as_bytes())
            .map_err(stdout_error)?;
        if let Some(cache) = &cache {
            writeln!(
                out,
                "cache: {} from the cache, {} read",
                cache.hits(),
                cache.reads()
            )
            .map_err(stdout_error)?;
        }
    }
    out.flush().map_err(stdout_error)?;
    Ok(if report.matches() {
        EXIT_MATCH
    } else {
        EXIT_DIFFER
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: &str = r"_\d{4}_\d{2}_\d{2}_\d{2}_\d{2}_(AM|PM)_BC";

    fn re(p: &str) -> Regex {
        Regex::new(p).unwrap()
    }

    fn stored(path: &str, size: u64, crc: u32) -> Entry {
        Entry {
            path: path.to_string(),
            size,
            checksum: Checksum::Stored(crc),
        }
    }

    fn cloud(path: &str, size: u64) -> Entry {
        Entry {
            path: path.to_string(),
            size,
            checksum: Checksum::CloudOnly,
        }
    }

    fn keyed(entries: Vec<Entry>) -> BTreeMap<String, Entry> {
        index(entries, &Options::default(), Side::Left).unwrap()
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn keeps_everything_without_filters() {
        assert!(Options::default().keeps("a/b.txt"));
    }

    #[test]
    fn include_keeps_only_matches() {
        let o = Options {
            include: vec![re(r"\.txt$")],
            ..Options::default()
        };
        assert!(o.keeps("a/b.txt"));
        assert!(!o.keeps("a/b.jpg"));
    }

    #[test]
    fn exclude_wins_over_include() {
        let o = Options {
            include: vec![re(r"\.txt$")],
            exclude: vec![re(r"^tmp/")],
            ..Options::default()
        };
        assert!(o.keeps("docs/b.txt"));
        assert!(!o.keeps("tmp/b.txt"));
    }

    #[test]
    fn pair_key_strips_stamp_from_file_name() {
        let o = Options {
            pair: vec![re(STAMP)],
            ..Options::default()
        };
        assert_eq!(o.pair_key("a_2026_11_07_04_28_PM_BC.zip"), "a.zip");
        assert_eq!(o.pair_key("x/a_2026_11_07_05_34_PM_BC.zip"), "x/a.zip");
    }

    #[test]
    fn pair_key_leaves_folder_names_alone() {
        let o = Options {
            pair: vec![re(STAMP)],
            ..Options::default()
        };
        assert_eq!(
            o.pair_key("b_2026_11_07_04_28_PM_BC/c.txt"),
            "b_2026_11_07_04_28_PM_BC/c.txt"
        );
    }

    #[test]
    fn pair_key_applies_every_regex_in_turn() {
        let o = Options {
            pair: vec![re(r"-v\d+"), re(r"\(copy\)")],
            ..Options::default()
        };
        assert_eq!(o.pair_key("d/report-v3(copy).pdf"), "d/report.pdf");
    }

    #[test]
    fn index_pairs_stamped_names() {
        let o = Options {
            pair: vec![re(STAMP)],
            ..Options::default()
        };
        let left = index(
            vec![stored("a_2026_11_07_04_28_PM_BC.zip", 3, 1)],
            &o,
            Side::Left,
        )
        .unwrap();
        let right = index(
            vec![stored("a_2026_11_07_05_34_PM_BC.zip", 3, 1)],
            &o,
            Side::Right,
        )
        .unwrap();
        let report = compare_maps(&left, &right).unwrap();
        assert!(report.matches());
        assert_eq!(
            report.same,
            vec![Pair {
                left: "a_2026_11_07_04_28_PM_BC.zip".into(),
                right: "a_2026_11_07_05_34_PM_BC.zip".into(),
            }]
        );
    }

    #[test]
    fn index_reports_collision_naming_both_files() {
        let o = Options {
            pair: vec![re(STAMP)],
            ..Options::default()
        };
        let err = index(
            vec![
                stored("a_2026_11_07_05_34_PM_BC.zip", 3, 1),
                stored("a_2026_11_07_04_28_PM_BC.zip", 3, 1),
            ],
            &o,
            Side::Right,
        )
        .unwrap_err();
        match &err {
            Error::Collision {
                side,
                key,
                first,
                second,
            } => {
                assert_eq!(*side, Side::Right);
                assert_eq!(key, "a.zip");
                assert_eq!(first, "a_2026_11_07_04_28_PM_BC.zip");
                assert_eq!(second, "a_2026_11_07_05_34_PM_BC.zip");
            }
            other => panic!("unexpected error {other:?}"),
        }
        let text = err.to_string();
        assert!(text.contains("a_2026_11_07_04_28_PM_BC.zip"));
        assert!(text.contains("a_2026_11_07_05_34_PM_BC.zip"));
    }

    #[test]
    fn index_drops_filtered_entries_before_pairing() {
        let o = Options {
            exclude: vec![re(r"\.bak$")],
            pair: vec![re(r"\.bak$")],
            ..Options::default()
        };
        // Without the filter these 2 would collide as "a".
        let map = index(
            vec![stored("a", 1, 1), stored("a.bak", 1, 1)],
            &o,
            Side::Left,
        )
        .unwrap();
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn crc_equal_is_same_and_crc_different_is_changed() {
        let left = keyed(vec![stored("a", 4, 0x1111), stored("b", 4, 0x2222)]);
        let right = keyed(vec![stored("a", 4, 0x1111), stored("b", 4, 0x3333)]);
        let report = compare_maps(&left, &right).unwrap();
        assert_eq!(report.same.len(), 1);
        assert_eq!(report.changed.len(), 1);
        assert_eq!(
            report.changed[0].reason,
            Reason::Crc32 {
                left: 0x2222,
                right: 0x3333
            }
        );
        assert!(!report.matches());
    }

    #[test]
    fn size_difference_is_changed_without_reading() {
        // The on-disk path does not exist: a read would fail the test.
        let left = keyed(vec![Entry {
            path: "a".into(),
            size: 4,
            checksum: Checksum::OnDisk(PathBuf::from("/no/such/file")),
        }]);
        let right = keyed(vec![stored("a", 5, 0)]);
        let report = compare_maps(&left, &right).unwrap();
        assert_eq!(report.changed[0].reason, Reason::Size);
    }

    #[test]
    fn only_left_and_only_right() {
        let left = keyed(vec![stored("a", 1, 1), stored("l", 1, 1)]);
        let right = keyed(vec![stored("a", 1, 1), stored("r", 1, 1)]);
        let report = compare_maps(&left, &right).unwrap();
        assert_eq!(report.only_left, vec!["l".to_string()]);
        assert_eq!(report.only_right, vec!["r".to_string()]);
    }

    #[test]
    fn cloud_only_same_size_is_skipped_and_still_matches() {
        let left = keyed(vec![cloud("big.iso", 10)]);
        let right = keyed(vec![stored("big.iso", 10, 7)]);
        let report = compare_maps(&left, &right).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert!(report.matches());
        assert!(report.to_text().contains(CLOUD_ONLY_NOTE));
    }

    #[test]
    fn cloud_only_size_difference_is_changed() {
        let left = keyed(vec![cloud("big.iso", 10)]);
        let right = keyed(vec![stored("big.iso", 11, 7)]);
        let report = compare_maps(&left, &right).unwrap();
        assert_eq!(report.changed[0].reason, Reason::Size);
    }

    #[test]
    fn crc32_of_file_matches_crc32fast() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(crc32_of_file(&path).unwrap(), crc32fast::hash(&bytes));
    }

    #[test]
    fn cloud_attribute_bits() {
        assert_eq!(FILE_ATTRIBUTE_RECALL_ON_OPEN, 262_144);
        assert_eq!(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, 4_194_304);
        assert!(attributes_are_cloud_only(FILE_ATTRIBUTE_RECALL_ON_OPEN));
        assert!(attributes_are_cloud_only(
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | 0x20
        ));
        // FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_PINNED is a hydrated file.
        assert!(!attributes_are_cloud_only(0x20 | 0x0008_0000));
    }

    #[test]
    fn zip_names_are_normalized() {
        assert_eq!(normalize_zip_name("a\\b\\c.txt"), "a/b/c.txt");
        assert_eq!(normalize_zip_name("./a/b.txt"), "a/b.txt");
        assert_eq!(normalize_zip_name("/a.txt"), "a.txt");
    }

    #[test]
    fn relative_path_uses_forward_slashes() {
        let root = Path::new("root");
        let path = root.join("a").join("b.txt");
        assert_eq!(relative_path(root, &path), "a/b.txt");
    }

    #[test]
    fn parse_args_reads_repeated_and_inline_values() {
        let cmd = parse_args(os(&[
            "--include",
            r"\.txt$",
            "--include=^docs/",
            "--exclude",
            "tmp",
            "--pair",
            STAMP,
            "--json",
            "L",
            "R",
        ]))
        .unwrap();
        let Command::Compare(args) = cmd else {
            panic!("expected a compare command");
        };
        assert_eq!(args.options.include.len(), 2);
        assert_eq!(args.options.exclude.len(), 1);
        assert_eq!(args.options.pair.len(), 1);
        assert!(args.json);
        assert_eq!(args.left, PathBuf::from("L"));
        assert_eq!(args.right, PathBuf::from("R"));
    }

    #[test]
    fn parse_args_double_dash_ends_options() {
        let Command::Compare(args) = parse_args(os(&["--", "-L", "R"])).unwrap() else {
            panic!("expected a compare command");
        };
        assert_eq!(args.left, PathBuf::from("-L"));
    }

    #[test]
    fn parse_args_errors() {
        assert!(matches!(
            parse_args(os(&["--bogus", "L", "R"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            parse_args(os(&["L", "R", "--include"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(parse_args(os(&["L"])), Err(Error::Usage(_))));
        assert!(matches!(
            parse_args(os(&["--pair", "(", "L", "R"])),
            Err(Error::BadRegex { flag: "--pair", .. })
        ));
    }

    #[test]
    fn parse_args_help_and_version() {
        assert!(matches!(parse_args(os(&["-h"])), Ok(Command::Help)));
        assert!(matches!(
            parse_args(os(&["--version"])),
            Ok(Command::Version)
        ));
    }

    /// Returns 2 different 64-byte strings with the same CRC-32.
    ///
    /// CRC-32 is affine over GF(2), so for strings of equal length, flipping
    /// a set of bits changes the CRC-32 by the XOR of each bit's own change.
    /// The last 32 bits are picked to undo the change from flipping bit 0 of
    /// byte 0. Those 32 bits always suffice, because they map 1 to 1 onto the
    /// 32 bits of the CRC-32.
    fn crc32_twins() -> (Vec<u8>, Vec<u8>) {
        let a: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(37)).collect();
        let mut b = a.clone();
        b[0] ^= 1;
        let base = crc32fast::hash(&b);
        let flip = |v: &mut Vec<u8>, j: usize| v[60 + j / 8] ^= 1 << (j % 8);
        let mut basis: Vec<(u32, u32)> = Vec::new();
        for j in 0..32 {
            let mut c = b.clone();
            flip(&mut c, j);
            let (mut effect, mut mask) = (crc32fast::hash(&c) ^ base, 1u32 << j);
            for &(be, bm) in &basis {
                if effect ^ be < effect {
                    effect ^= be;
                    mask ^= bm;
                }
            }
            if effect != 0 {
                basis.push((effect, mask));
                basis.sort_by_key(|x| std::cmp::Reverse(x.0));
            }
        }
        let mut want = crc32fast::hash(&a) ^ base;
        let mut pick = 0u32;
        for &(be, bm) in &basis {
            if want ^ be < want {
                want ^= be;
                pick ^= bm;
            }
        }
        assert_eq!(want, 0, "the last 32 bits must reach every CRC-32");
        for j in 0..32 {
            if pick >> j & 1 == 1 {
                flip(&mut b, j);
            }
        }
        assert_eq!(crc32fast::hash(&a), crc32fast::hash(&b));
        assert_ne!(a, b);
        (a, b)
    }

    fn on_disk(path: &str, file: &Path) -> Entry {
        Entry {
            path: path.to_string(),
            size: std::fs::metadata(file).unwrap().len(),
            checksum: Checksum::OnDisk(file.to_path_buf()),
        }
    }

    #[test]
    fn first_difference_finds_the_offset() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &bytes).unwrap();
        std::fs::write(&b, &bytes).unwrap();
        assert_eq!(first_difference(&a, &b).unwrap(), None);
        // Past the 1st 64 KiB buffer, so the offset must carry across reads.
        let mut changed = bytes.clone();
        changed[70_000] ^= 0xff;
        std::fs::write(&b, &changed).unwrap();
        assert_eq!(first_difference(&a, &b).unwrap(), Some(70_000));
        // A shorter file differs where it ends.
        std::fs::write(&b, &bytes[..1_000]).unwrap();
        assert_eq!(first_difference(&a, &b).unwrap(), Some(1_000));
        std::fs::write(&a, b"").unwrap();
        std::fs::write(&b, b"").unwrap();
        assert_eq!(first_difference(&a, &b).unwrap(), None);
    }

    #[test]
    fn blake3_of_file_matches_blake3() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            blake3_of_file(&path).unwrap(),
            *blake3::hash(&bytes).as_bytes()
        );
    }

    #[test]
    fn crc32_twins_pass_crc32_but_not_blake3_or_verify() {
        let (a, b) = crc32_twins();
        let dir = tempfile::tempdir().unwrap();
        let (pa, pb) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&pa, &a).unwrap();
        std::fs::write(&pb, &b).unwrap();
        let left = keyed(vec![on_disk("f", &pa)]);
        let right = keyed(vec![on_disk("f", &pb)]);

        let crc = compare_maps(&left, &right).unwrap();
        assert_eq!(crc.same.len(), 1, "CRC-32 cannot tell the twins apart");

        let blake = Check {
            hash: HashKind::Blake3,
            verify: false,
        };
        let report = compare_maps_with(&left, &right, blake).unwrap();
        assert_eq!(
            report.changed[0].reason,
            Reason::Blake3 {
                left: *blake3::hash(&a).as_bytes(),
                right: *blake3::hash(&b).as_bytes(),
            }
        );

        let verify = Check {
            verify: true,
            ..Check::default()
        };
        let report = compare_maps_with(&left, &right, verify).unwrap();
        assert_eq!(report.changed[0].reason, Reason::Bytes { offset: 0 });
        assert!(!report.matches());
    }

    #[test]
    fn equal_folder_files_are_same_under_every_check() {
        let dir = tempfile::tempdir().unwrap();
        let (pa, pb) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&pa, b"same bytes").unwrap();
        std::fs::write(&pb, b"same bytes").unwrap();
        let left = keyed(vec![on_disk("f", &pa)]);
        let right = keyed(vec![on_disk("f", &pb)]);
        for check in [
            Check::default(),
            Check {
                hash: HashKind::Blake3,
                verify: false,
            },
            Check {
                hash: HashKind::Crc32,
                verify: true,
            },
            Check {
                hash: HashKind::Blake3,
                verify: true,
            },
        ] {
            let report = compare_maps_with(&left, &right, check).unwrap();
            assert_eq!(report.same.len(), 1, "{check:?}");
        }
    }

    #[test]
    fn verify_wins_over_hash() {
        let (a, b) = crc32_twins();
        let dir = tempfile::tempdir().unwrap();
        let (pa, pb) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&pa, &a).unwrap();
        std::fs::write(&pb, &b).unwrap();
        let both = Check {
            hash: HashKind::Blake3,
            verify: true,
        };
        let report = compare_maps_with(
            &keyed(vec![on_disk("f", &pa)]),
            &keyed(vec![on_disk("f", &pb)]),
            both,
        )
        .unwrap();
        assert_eq!(report.changed[0].reason, Reason::Bytes { offset: 0 });
    }

    #[test]
    fn zip_entry_pairs_fall_back_to_crc32() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"abcd").unwrap();
        let left = keyed(vec![on_disk("a", &path)]);
        let right = keyed(vec![stored("a", 4, 0x1234)]);
        let check = Check {
            hash: HashKind::Blake3,
            verify: true,
        };
        let report = compare_maps_with(&left, &right, check).unwrap();
        assert_eq!(
            report.changed[0].reason,
            Reason::Crc32 {
                left: crc32fast::hash(b"abcd"),
                right: 0x1234
            }
        );
    }

    #[test]
    fn cloud_only_is_never_read_under_verify() {
        // The on-disk path does not exist: a read would fail the test.
        let left = keyed(vec![Entry {
            path: "big.iso".into(),
            size: 10,
            checksum: Checksum::OnDisk(PathBuf::from("/no/such/file")),
        }]);
        let right = keyed(vec![cloud("big.iso", 10)]);
        let check = Check {
            hash: HashKind::Blake3,
            verify: true,
        };
        let report = compare_maps_with(&left, &right, check).unwrap();
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn parse_args_reads_hash_and_verify() {
        let Command::Compare(args) = parse_args(os(&["L", "R"])).unwrap() else {
            panic!("expected a compare command");
        };
        assert_eq!(args.check, Check::default());
        for (argv, hash, verify) in [
            (vec!["--hash", "blake3", "L", "R"], HashKind::Blake3, false),
            (vec!["--hash=crc32", "L", "R"], HashKind::Crc32, false),
            (vec!["--verify", "L", "R"], HashKind::Crc32, true),
            (
                vec!["--verify", "--hash=blake3", "L", "R"],
                HashKind::Blake3,
                true,
            ),
        ] {
            let Command::Compare(args) = parse_args(os(&argv)).unwrap() else {
                panic!("expected a compare command");
            };
            assert_eq!(args.check, Check { hash, verify }, "{argv:?}");
        }
        assert!(matches!(
            parse_args(os(&["--hash", "md5", "L", "R"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            parse_args(os(&["L", "R", "--hash"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            parse_args(os(&["--verify=yes", "L", "R"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn text_and_json_for_blake3_and_bytes() {
        let pair = Pair {
            left: "f".into(),
            right: "f".into(),
        };
        let report = Report {
            changed: vec![
                Changed {
                    pair: pair.clone(),
                    left_size: 2,
                    right_size: 2,
                    reason: Reason::Blake3 {
                        left: [0xab; 32],
                        right: [0x01; 32],
                    },
                },
                Changed {
                    pair,
                    left_size: 2,
                    right_size: 2,
                    reason: Reason::Bytes { offset: 7 },
                },
            ],
            ..Report::default()
        };
        let text = report.to_text();
        assert!(text.contains(&format!(
            "  f  (blake3 {} -> {})\n",
            "ab".repeat(32),
            "01".repeat(32)
        )));
        assert!(text.contains("  f  (bytes differ at offset 7)\n"));
        let v = report.to_json("L", "R");
        assert_eq!(v["changed"][0]["reason"], "blake3");
        assert_eq!(v["changed"][0]["left_blake3"], "ab".repeat(32));
        assert_eq!(v["changed"][0]["right_blake3"], "01".repeat(32));
        assert_eq!(v["changed"][1]["reason"], "bytes");
        assert_eq!(v["changed"][1]["offset"], 7);
    }

    /// The compare loop as it was before the reads were staged and spread
    /// across threads, kept here to check the new code against.
    fn serial_reference(
        left: &BTreeMap<String, Entry>,
        right: &BTreeMap<String, Entry>,
        check: Check,
    ) -> Result<Report, Error> {
        fn resolve_crc(checksum: &Checksum) -> Result<Option<u32>, Error> {
            match checksum {
                Checksum::Stored(crc) => Ok(Some(*crc)),
                Checksum::OnDisk(path) => crc32_of_file(path).map(Some),
                Checksum::CloudOnly => Ok(None),
            }
        }
        let mut report = Report::default();
        for (key, l) in left {
            let Some(r) = right.get(key) else {
                report.only_left.push(l.path.clone());
                continue;
            };
            let pair = Pair {
                left: l.path.clone(),
                right: r.path.clone(),
            };
            if l.size != r.size {
                report.changed.push(Changed {
                    pair,
                    left_size: l.size,
                    right_size: r.size,
                    reason: Reason::Size,
                });
                continue;
            }
            let strong = check.verify || check.hash == HashKind::Blake3;
            if strong && (l.checksum == Checksum::CloudOnly || r.checksum == Checksum::CloudOnly) {
                report.skipped.push(pair);
                continue;
            }
            if let (Checksum::OnDisk(lp), Checksum::OnDisk(rp)) = (&l.checksum, &r.checksum) {
                if strong {
                    let reason = if check.verify {
                        first_difference(lp, rp)?.map(|offset| Reason::Bytes { offset })
                    } else {
                        let (lh, rh) = (blake3_of_file(lp)?, blake3_of_file(rp)?);
                        (lh != rh).then_some(Reason::Blake3 {
                            left: lh,
                            right: rh,
                        })
                    };
                    match reason {
                        None => report.same.push(pair),
                        Some(reason) => report.changed.push(Changed {
                            pair,
                            left_size: l.size,
                            right_size: r.size,
                            reason,
                        }),
                    }
                    continue;
                }
            }
            let (Some(lc), Some(rc)) = (resolve_crc(&l.checksum)?, resolve_crc(&r.checksum)?)
            else {
                report.skipped.push(pair);
                continue;
            };
            if lc == rc {
                report.same.push(pair);
            } else {
                report.changed.push(Changed {
                    pair,
                    left_size: l.size,
                    right_size: r.size,
                    reason: Reason::Crc32 {
                        left: lc,
                        right: rc,
                    },
                });
            }
        }
        for (key, r) in right {
            if !left.contains_key(key) {
                report.only_right.push(r.path.clone());
            }
        }
        Ok(report)
    }

    const ALL_CHECKS: [Check; 4] = [
        Check {
            hash: HashKind::Crc32,
            verify: false,
        },
        Check {
            hash: HashKind::Blake3,
            verify: false,
        },
        Check {
            hash: HashKind::Crc32,
            verify: true,
        },
        Check {
            hash: HashKind::Blake3,
            verify: true,
        },
    ];

    fn threads(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    /// A path that does not exist. Opening it fails, so a pair that holds it
    /// proves by passing that it was never read.
    fn missing(dir: &Path, name: &str) -> Entry {
        Entry {
            path: name.to_string(),
            size: 0,
            checksum: Checksum::OnDisk(dir.join("missing").join(name)),
        }
    }

    /// 2 keyed trees with 300 keys that cover every kind of pair: equal files,
    /// files of equal size with other bytes, CRC-32 twins, sizes that differ
    /// (with files that do not exist), zip entries, cloud-only files, files on
    /// 1 side only.
    fn mixed_trees(dir: &Path) -> (BTreeMap<String, Entry>, BTreeMap<String, Entry>) {
        let (twin_a, twin_b) = crc32_twins();
        let (mut l, mut r) = (Vec::new(), Vec::new());
        let write = |side: &str, name: &str, bytes: &[u8]| {
            let path = dir.join(side).join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            on_disk(name, &path)
        };
        for i in 0..300u32 {
            // Names whose byte order differs from creation order.
            let name = format!("d{}/f{:03}.bin", i % 7, (i * 37) % 1000);
            let body: Vec<u8> = (0..(i * 97) % 3_000).map(|j| (j * i % 251) as u8).collect();
            match i % 10 {
                0..=2 => {
                    l.push(write("l", &name, &body));
                    r.push(write("r", &name, &body));
                }
                3 => {
                    let mut other = body.clone();
                    other.push(1);
                    other[0] ^= 0x55;
                    let mut same_len = body.clone();
                    same_len.push(2);
                    l.push(write("l", &name, &same_len));
                    r.push(write("r", &name, &other));
                }
                4 => {
                    let (mut a, mut b) = (missing(dir, &name), missing(dir, &name));
                    a.size = 10 + u64::from(i);
                    b.size = 11 + u64::from(i);
                    l.push(a);
                    r.push(b);
                }
                5 => {
                    l.push(write("l", &name, &twin_a));
                    r.push(write("r", &name, &twin_b));
                }
                6 => {
                    l.push(write("l", &name, &body));
                    let crc = crc32fast::hash(&body) ^ ((i / 10) & 1);
                    r.push(stored(&name, body.len() as u64, crc));
                }
                7 => {
                    l.push(write("l", &name, &body));
                    r.push(cloud(&name, body.len() as u64));
                }
                8 => {
                    l.push(cloud(&name, 5));
                    r.push(stored(&name, 5, i));
                }
                _ => {
                    if i % 20 == 9 {
                        l.push(write("l", &name, &body));
                    } else {
                        r.push(write("r", &name, &body));
                    }
                }
            }
        }
        (keyed(l), keyed(r))
    }

    #[test]
    fn plan_reads_only_pairs_of_equal_size() {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = mixed_trees(dir.path());
        for check in ALL_CHECKS {
            let plan = plan(&left, &right, check);
            let mut read: Vec<&Path> = Vec::new();
            for task in &plan.tasks {
                match *task {
                    Task::Crc32(p) | Task::Blake3(p) => read.push(p),
                    Task::Verify(a, b) => read.extend([a, b]),
                }
            }
            let mut expected: Vec<&Path> = Vec::new();
            for (key, l) in &left {
                let Some(r) = right.get(key) else { continue };
                if l.size != r.size {
                    continue;
                }
                let sides = [&l.checksum, &r.checksum];
                let strong = check.verify || check.hash == HashKind::Blake3;
                if strong && sides.contains(&&Checksum::CloudOnly) {
                    continue;
                }
                for checksum in sides {
                    if let Checksum::OnDisk(p) = checksum {
                        expected.push(p);
                    }
                }
            }
            assert_eq!(read, expected, "{check:?}");
            assert!(read
                .iter()
                .all(|p| !p.starts_with(dir.path().join("missing"))));
            assert!(plan.tasks.len() > 50, "{check:?}");
        }
    }

    #[test]
    fn every_thread_count_gives_the_serial_report() {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = mixed_trees(dir.path());
        for check in ALL_CHECKS {
            let serial = serial_reference(&left, &right, check).unwrap();
            assert!(!serial.same.is_empty(), "{check:?}");
            assert!(!serial.changed.is_empty(), "{check:?}");
            assert!(!serial.skipped.is_empty(), "{check:?}");
            for n in [1, 2, 3, 4, 8, 64] {
                let report = compare_maps_threads(&left, &right, check, threads(n)).unwrap();
                assert_eq!(report, serial, "{check:?} on {n} threads");
                assert_eq!(report.to_text(), serial.to_text());
                assert_eq!(report.to_json("L", "R"), serial.to_json("L", "R"));
            }
            assert_eq!(compare_maps_with(&left, &right, check).unwrap(), serial);
        }
    }

    #[test]
    fn output_order_is_stable_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (left, right) = mixed_trees(dir.path());
        for check in ALL_CHECKS {
            let first = compare_maps_threads(&left, &right, check, threads(8)).unwrap();
            let keys = |pairs: Vec<&Pair>| pairs.iter().map(|p| p.left.clone()).collect::<Vec<_>>();
            for list in [
                keys(first.same.iter().collect()),
                keys(first.changed.iter().map(|c| &c.pair).collect()),
                keys(first.skipped.iter().collect()),
                first.only_left.clone(),
                first.only_right.clone(),
            ] {
                let mut sorted = list.clone();
                sorted.sort();
                assert_eq!(list, sorted, "{check:?}");
            }
            for _ in 0..20 {
                let again = compare_maps_threads(&left, &right, check, threads(8)).unwrap();
                assert_eq!(again.to_text(), first.to_text(), "{check:?}");
            }
        }
    }

    #[test]
    fn read_error_is_the_first_in_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ok");
        std::fs::write(&file, b"abcd").unwrap();
        let good = |name: &str| Entry {
            path: name.to_string(),
            ..on_disk(name, &file)
        };
        let gone = |name: &str| Entry {
            size: 4,
            ..missing(dir.path(), name)
        };
        let mut l: Vec<Entry> = (0..100).map(|i| good(&format!("a{i:03}"))).collect();
        let mut r = l.clone();
        // Key "b" fails on its right side, key "c" on its left side.
        l.push(good("b"));
        r.push(gone("b"));
        l.push(gone("c"));
        r.push(gone("c"));
        l.extend((0..100).map(|i| good(&format!("d{i:03}"))));
        r.extend((0..100).map(|i| good(&format!("d{i:03}"))));
        let (left, right) = (keyed(l), keyed(r));
        let want = dir.path().join("missing").join("b");
        for check in ALL_CHECKS {
            let serial = serial_reference(&left, &right, check).unwrap_err();
            assert!(matches!(&serial, Error::Io { path, .. } if *path == want));
            for n in [1, 2, 3, 4, 8, 64] {
                let err = compare_maps_threads(&left, &right, check, threads(n)).unwrap_err();
                assert_eq!(
                    err.to_string(),
                    serial.to_string(),
                    "{check:?} on {n} threads"
                );
            }
        }
    }

    #[test]
    fn run_tasks_with_no_tasks() {
        assert_eq!(run_tasks(&[], 8, None).unwrap(), Vec::new());
    }

    #[test]
    fn json_keys_are_sorted() {
        let report = Report {
            changed: vec![Changed {
                pair: Pair {
                    left: "b".into(),
                    right: "b".into(),
                },
                left_size: 1,
                right_size: 1,
                reason: Reason::Crc32 { left: 1, right: 2 },
            }],
            ..Report::default()
        };
        let text = serde_json::to_string(&report.to_json("L", "R")).unwrap();
        assert!(text.starts_with(r#"{"changed":[{"left":"b","left_crc32":"00000001","left_size":1,"reason":"crc32","right":"b","right_crc32":"00000002","right_size":1}],"left":"L","links_left":[],"links_right":[],"match":false,"only_left":[]"#));
    }

    // -----------------------------------------------------------------------
    // Digest cache
    // -----------------------------------------------------------------------

    /// Writes `bytes` to `path` and sets its modified time `age` in the past.
    fn aged(path: &Path, bytes: &[u8], age: Duration) {
        std::fs::write(path, bytes).unwrap();
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn cache_reads_once_then_answers_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        aged(&file, b"some bytes", HOUR);
        let cache = HashCache::new(&dir.path().join("cache.json"));

        assert_eq!(cache.crc32(&file).unwrap(), crc32_of_file(&file).unwrap());
        assert_eq!((cache.hits(), cache.reads(), cache.len()), (0, 1, 1));
        assert_eq!(cache.crc32(&file).unwrap(), crc32_of_file(&file).unwrap());
        assert_eq!((cache.hits(), cache.reads()), (1, 1));

        // BLAKE3 is a 2nd digest in the same entry, read once on its own.
        assert_eq!(cache.blake3(&file).unwrap(), blake3_of_file(&file).unwrap());
        assert_eq!(cache.blake3(&file).unwrap(), blake3_of_file(&file).unwrap());
        assert_eq!((cache.hits(), cache.reads(), cache.len()), (2, 2, 1));
    }

    #[test]
    fn cache_survives_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        aged(&a, b"alpha", HOUR);
        aged(&b, b"beta", HOUR);
        let file = dir.path().join("cache.json");
        let cache = HashCache::load(&file).unwrap();
        assert!(cache.is_empty());
        let crc = cache.crc32(&a).unwrap();
        let digest = cache.blake3(&b).unwrap();
        cache.save().unwrap();

        let again = HashCache::load(&file).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again.crc32(&a).unwrap(), crc);
        assert_eq!(again.blake3(&b).unwrap(), digest);
        assert_eq!((again.hits(), again.reads()), (2, 0));
        // No temporary file is left next to the cache.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.iter().filter(|n| n.contains(".tmp")).count(), 0);
    }

    #[test]
    fn cache_reads_again_after_a_change_of_size_or_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        aged(&file, b"version 1", HOUR);
        let cache = HashCache::new(&dir.path().join("cache.json"));
        let first = cache.crc32(&file).unwrap();

        aged(&file, b"version 2", HOUR / 2); // same size, newer time-stamp
        let second = cache.crc32(&file).unwrap();
        assert_ne!(first, second);
        assert_eq!(second, crc32_of_file(&file).unwrap());

        aged(&file, b"version 30", HOUR / 2); // other size
        assert_eq!(cache.crc32(&file).unwrap(), crc32_of_file(&file).unwrap());
        assert_eq!((cache.hits(), cache.reads()), (0, 3));
    }

    #[test]
    fn a_file_written_just_now_is_read_but_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fresh.bin");
        std::fs::write(&file, b"just written").unwrap();
        let cache = HashCache::new(&dir.path().join("cache.json"));
        assert_eq!(cache.crc32(&file).unwrap(), crc32_of_file(&file).unwrap());
        assert_eq!(cache.crc32(&file).unwrap(), crc32_of_file(&file).unwrap());
        assert_eq!((cache.hits(), cache.reads(), cache.len()), (0, 2, 0));
        // Nothing changed, so nothing is written.
        cache.save().unwrap();
        assert!(!dir.path().join("cache.json").exists());
    }

    #[test]
    fn cache_trusts_size_and_time_stamp() {
        // The documented limit: new bytes of the same size under the same
        // time-stamp keep the old digest. --verify always reads.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        aged(&file, b"aaaa", HOUR);
        let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
        let cache = HashCache::new(&dir.path().join("cache.json"));
        let old = cache.crc32(&file).unwrap();
        std::fs::write(&file, b"bbbb").unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_eq!(cache.crc32(&file).unwrap(), old);
    }

    #[test]
    fn bad_cache_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.json");
        for (text, why) in [
            ("not json", "expected"),
            (r#"{"version": 2, "files": {}}"#, "unknown version"),
            (r#"{"version": 1}"#, "no files object"),
            (
                r#"{"version": 1, "files": {"/a": {"size": 1}}}"#,
                "lacks size or mtime_ns",
            ),
            (
                r#"{"version": 1, "files": {"/a": {"size": 1, "mtime_ns": 5, "crc32": "xyz"}}}"#,
                "bad crc32",
            ),
            (
                r#"{"version": 1, "files": {"/a": {"size": 1, "mtime_ns": 5, "blake3": "00"}}}"#,
                "bad blake3",
            ),
        ] {
            std::fs::write(&file, text).unwrap();
            let err = HashCache::load(&file).unwrap_err().to_string();
            assert!(err.contains("is not a fsprobe cache file"), "{err}");
            assert!(err.contains(why), "{text}: {err}");
        }
    }

    #[test]
    fn a_cached_run_gives_the_same_report_and_reads_nothing_the_2nd_time() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        for i in 0..40u32 {
            for (side, root) in [(0u32, &l), (1, &r)] {
                std::fs::create_dir_all(root.join("d")).unwrap();
                // Every 3rd file differs in its bytes, every 7th in its size.
                let mut bytes = format!("file {i:03}").into_bytes();
                if side == 1 && i % 3 == 0 {
                    bytes[0] = b'F';
                }
                if side == 1 && i % 7 == 0 {
                    bytes.push(b'!');
                }
                aged(&root.join(format!("d/{i}.txt")), &bytes, HOUR);
            }
        }
        let options = Options::default();
        for check in [
            Check::default(),
            Check {
                hash: HashKind::Blake3,
                verify: false,
            },
        ] {
            let plain = compare_trees_with(&l, &r, &options, check).unwrap();
            let file = dir.path().join(format!("cache-{:?}.json", check.hash));
            let first = HashCache::load(&file).unwrap();
            assert_eq!(
                compare_trees_cached(&l, &r, &options, check, Some(&first)).unwrap(),
                plain
            );
            first.save().unwrap();
            assert_eq!(first.hits(), 0);
            let second = HashCache::load(&file).unwrap();
            assert_eq!(
                compare_trees_cached(&l, &r, &options, check, Some(&second)).unwrap(),
                plain
            );
            assert_eq!(second.reads(), 0);
            assert_eq!(second.hits(), first.reads());
            // 40 keys, of which 6 differ in size: 34 pairs read on both sides.
            assert_eq!(first.reads(), 68);
        }
    }

    #[test]
    fn verify_never_uses_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        std::fs::create_dir_all(&l).unwrap();
        std::fs::create_dir_all(&r).unwrap();
        aged(&l.join("a"), b"same", HOUR);
        aged(&r.join("a"), b"same", HOUR);
        let cache = HashCache::new(&dir.path().join("cache.json"));
        let check = Check {
            hash: HashKind::Crc32,
            verify: true,
        };
        let report =
            compare_trees_cached(&l, &r, &Options::default(), check, Some(&cache)).unwrap();
        assert_eq!(report.same.len(), 1);
        assert_eq!((cache.hits(), cache.reads(), cache.len()), (0, 0, 0));
    }

    #[test]
    fn cache_option_is_parsed() {
        for args in [
            vec!["--cache", "c.json", "L", "R"],
            vec!["--cache=c.json", "L", "R"],
        ] {
            let Command::Compare(a) = parse_args(os(&args)).unwrap() else {
                panic!("expected a compare");
            };
            assert_eq!(a.cache, Some(PathBuf::from("c.json")));
        }
        let Command::Compare(a) = parse_args(os(&["L", "R"])).unwrap() else {
            panic!("expected a compare");
        };
        assert_eq!(a.cache, None);
        for args in [vec!["L", "R", "--cache"], vec!["--cache=", "L", "R"]] {
            assert!(
                matches!(parse_args(os(&args)), Err(Error::Usage(m)) if m == "--cache needs a FILE value")
            );
        }
    }

    #[test]
    fn quiet_option_is_parsed() {
        for args in [vec!["-q", "L", "R"], vec!["L", "R", "--quiet"]] {
            let Command::Compare(a) = parse_args(os(&args)).unwrap() else {
                panic!("expected a compare");
            };
            assert!(a.quiet, "{args:?}");
        }
        let Command::Compare(a) = parse_args(os(&["L", "R"])).unwrap() else {
            panic!("expected a compare");
        };
        assert!(!a.quiet);
        assert!(matches!(
            parse_args(os(&["--quiet=1", "L", "R"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn threads_option_is_parsed() {
        for (args, want) in [
            (vec!["--threads", "1", "L", "R"], 1),
            (vec!["--threads=4", "L", "R"], 4),
            (vec!["L", "R", "--threads", "2"], 2),
        ] {
            let Command::Compare(a) = parse_args(os(&args)).unwrap() else {
                panic!("expected a compare");
            };
            assert_eq!(a.threads, NonZeroUsize::new(want), "{args:?}");
        }
        let Command::Compare(a) = parse_args(os(&["L", "R"])).unwrap() else {
            panic!("expected a compare");
        };
        assert_eq!(a.threads, None);
        assert!(
            matches!(parse_args(os(&["L", "R", "--threads"])), Err(Error::Usage(m)) if m == "--threads needs an N value")
        );
        for bad in ["0", "-1", "two", "", "1.5"] {
            let flag = format!("--threads={bad}");
            assert!(
                matches!(parse_args(os(&[flag.as_str(), "L", "R"])), Err(Error::Usage(m)) if m == format!("--threads needs a whole number of 1 or more, got '{bad}'")),
                "{bad}"
            );
        }
    }
}
