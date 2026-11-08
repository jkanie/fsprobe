//! Compare 2 trees by relative path, size and CRC-32.
//!
//! Each side is a folder or a `.zip` file. Folders are walked recursively.
//! Zip files are read from their central directory only: names, sizes and
//! stored CRC-32 values, with no entry ever extracted.

use std::collections::btree_map::{BTreeMap, Entry as MapEntry};
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

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

fn resolve_crc(checksum: &Checksum) -> Result<Option<u32>, Error> {
    match checksum {
        Checksum::Stored(crc) => Ok(Some(*crc)),
        Checksum::OnDisk(path) => crc32_of_file(path).map(Some),
        Checksum::CloudOnly => Ok(None),
    }
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
}

impl Report {
    /// True when no pair changed and no file is on 1 side only.
    ///
    /// Skipped pairs had equal sizes and do not count as a difference.
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
                }
                item
            })
            .collect();
        let skipped: Vec<Value> = self
            .skipped
            .iter()
            .map(|p| json!({ "left": p.left, "right": p.right, "reason": CLOUD_ONLY_NOTE }))
            .collect();
        json!({
            "left": left_root,
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

/// Compares 2 keyed trees.
///
/// Sizes are compared 1st. A CRC-32 is computed only for a pair of equal
/// size, and never for a cloud-only file.
pub fn compare_maps(
    left: &BTreeMap<String, Entry>,
    right: &BTreeMap<String, Entry>,
) -> Result<Report, Error> {
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
        let (Some(lc), Some(rc)) = (resolve_crc(&l.checksum)?, resolve_crc(&r.checksum)?) else {
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

/// Lists, filters, pairs and compares 2 trees.
pub fn compare_trees(left: &Path, right: &Path, options: &Options) -> Result<Report, Error> {
    let l = index(list_tree(left)?, options, Side::Left)?;
    let r = index(list_tree(right)?, options, Side::Right)?;
    compare_maps(&l, &r)
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
    pub options: Options,
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
  --json           Print a JSON object instead of text.
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
    let mut options = Options::default();
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
            options,
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
    let report = compare_trees(&args.left, &args.right, &args.options)?;
    if args.json {
        let value = report.to_json(
            &args.left.display().to_string(),
            &args.right.display().to_string(),
        );
        let text = serde_json::to_string_pretty(&value).expect("a JSON value always serializes");
        writeln!(out, "{text}").map_err(stdout_error)?;
    } else {
        out.write_all(report.to_text().as_bytes())
            .map_err(stdout_error)?;
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
        assert!(text.starts_with(r#"{"changed":[{"left":"b","left_crc32":"00000001","left_size":1,"reason":"crc32","right":"b","right_crc32":"00000002","right_size":1}],"left":"L","match":false,"only_left":[]"#));
    }
}
