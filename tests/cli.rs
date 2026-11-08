//! End-to-end tests: build folders and zips in a temp dir, run the binary.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const BIN: &str = env!("CARGO_BIN_EXE_fsprobe");
const STAMP: &str = r"_\d{4}_\d{2}_\d{2}_\d{2}_\d{2}_(AM|PM)_BC";

/// The 3 files both sides start with.
const BASE: &[(&str, &[u8])] = &[
    ("readme.txt", b"hello\n"),
    ("docs/notes.txt", b"alpha beta\n"),
    ("docs/deep/data.bin", &[0, 1, 2, 3, 4, 5, 6, 7]),
];

fn write_folder(root: &Path, files: &[(&str, &[u8])]) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    for (name, bytes) in files {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    root.to_path_buf()
}

/// Writes a zip with a directory entry for every folder, the way archivers do.
fn write_zip(path: &Path, files: &[(&str, &[u8])]) -> PathBuf {
    let mut zip = ZipWriter::new(fs::File::create(path).unwrap());
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let mut dirs: Vec<&str> = files
        .iter()
        .filter_map(|(name, _)| name.rfind('/').map(|i| &name[..=i]))
        .collect();
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        zip.add_directory(dir, opts).unwrap();
    }
    for (name, bytes) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
    path.to_path_buf()
}

fn with(files: &[(&'static str, &'static [u8])]) -> Vec<(&'static str, &'static [u8])> {
    let mut all: Vec<_> = BASE.to_vec();
    for (name, bytes) in files {
        all.retain(|(n, _)| n != name);
        all.push((name, bytes));
    }
    all
}

fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn run2(left: &Path, right: &Path, extra: &[&str]) -> Output {
    let mut args: Vec<&std::ffi::OsStr> = extra.iter().map(|s| s.as_ref()).collect();
    args.push(left.as_os_str());
    args.push(right.as_os_str());
    run(&args)
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

#[test]
fn identical_folders_exit_0() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(&t.path().join("r"), BASE);
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "summary: 3 same, 0 changed, 0 only-left, 0 only-right, 0 skipped\n"
    );
}

#[test]
fn changed_file_same_size_is_found_by_crc() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(&t.path().join("r"), &with(&[("readme.txt", b"HELLO\n")]));
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1);
    let text = stdout(&out);
    let expected = format!(
        "changed:\n  readme.txt  (crc32 {:08x} -> {:08x})\n",
        crc32fast::hash(b"hello\n"),
        crc32fast::hash(b"HELLO\n")
    );
    assert!(text.starts_with(&expected), "{text}");
    assert!(text.ends_with("summary: 2 same, 1 changed, 0 only-left, 0 only-right, 0 skipped\n"));
}

#[test]
fn changed_size_is_reported_with_both_sizes() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(
        &t.path().join("r"),
        &with(&[("docs/notes.txt", b"alpha\n")]),
    );
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("  docs/notes.txt  (size 11 -> 6 bytes)\n"));
}

#[test]
fn only_left_is_listed() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), &with(&[("extra/old.log", b"x")]));
    let r = write_folder(&t.path().join("r"), BASE);
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1);
    let text = stdout(&out);
    assert!(text.contains("only in left:\n  extra/old.log\n"), "{text}");
    assert!(!text.contains("only in right:"));
    assert!(text.contains("1 only-left, 0 only-right"));
}

#[test]
fn only_right_is_listed() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(&t.path().join("r"), &with(&[("new.txt", b"x")]));
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1);
    let text = stdout(&out);
    assert!(text.contains("only in right:\n  new.txt\n"), "{text}");
    assert!(text.contains("0 only-left, 1 only-right"));
}

#[test]
fn folder_vs_zip_match() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_zip(&t.path().join("r.zip"), BASE);
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("3 same, 0 changed"));

    // And the other way round.
    let out = run2(&r, &l, &[]);
    assert_eq!(code(&out), 0);
}

#[test]
fn zip_vs_zip_with_changed_crc() {
    let t = TempDir::new().unwrap();
    let l = write_zip(&t.path().join("l.zip"), BASE);
    let r = write_zip(
        &t.path().join("r.zip"),
        &with(&[("docs/deep/data.bin", &[0, 1, 2, 3, 4, 5, 6, 8])]),
    );
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1);
    let expected = format!(
        "  docs/deep/data.bin  (crc32 {:08x} -> {:08x})\n",
        crc32fast::hash(&[0, 1, 2, 3, 4, 5, 6, 7]),
        crc32fast::hash(&[0, 1, 2, 3, 4, 5, 6, 8])
    );
    assert!(stdout(&out).contains(&expected), "{}", stdout(&out));
}

#[test]
fn exclude_hides_a_difference() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), &with(&[("cache/a.tmp", b"1")]));
    let r = write_folder(&t.path().join("r"), &with(&[("cache/a.tmp", b"22")]));
    assert_eq!(code(&run2(&l, &r, &[])), 1);
    let out = run2(&l, &r, &["--exclude", r"^cache/"]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("3 same"));
}

#[test]
fn include_keeps_only_matches_and_exclude_wins() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(&t.path().join("r"), &with(&[("readme.txt", b"HELLO\n")]));
    // Only docs/ is compared, so the changed readme is out of scope.
    let out = run2(&l, &r, &["--include", "^docs/"]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("2 same"));
    // Exclude wins over include for the same path.
    let out = run2(
        &l,
        &r,
        &[
            "--include",
            "^docs/",
            "--include=readme",
            "--exclude",
            "deep",
        ],
    );
    assert_eq!(code(&out), 1);
    assert!(
        stdout(&out).contains("1 same, 1 changed"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn pair_matches_time_stamped_names() {
    let t = TempDir::new().unwrap();
    let l = write_folder(
        &t.path().join("l"),
        &[("a_2026_11_07_04_28_PM_BC.zip", b"same bytes")],
    );
    let r = write_folder(
        &t.path().join("r"),
        &[("a_2026_11_07_05_34_PM_BC.zip", b"same bytes")],
    );
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("1 only-left, 1 only-right"));

    let out = run2(&l, &r, &["--pair", STAMP]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(stdout(&out).contains("1 same"));
}

#[test]
fn pair_collision_is_an_error_naming_both() {
    let t = TempDir::new().unwrap();
    let l = write_folder(
        &t.path().join("l"),
        &[
            ("a_2026_11_07_04_28_PM_BC.zip", b"1"),
            ("a_2026_11_07_05_34_PM_BC.zip", b"2"),
        ],
    );
    let r = write_folder(&t.path().join("r"), &[("a.zip", b"1")]);
    let out = run2(&l, &r, &["--pair", STAMP]);
    assert_eq!(code(&out), 2);
    let err = stderr(&out);
    assert!(err.contains("a_2026_11_07_04_28_PM_BC.zip"), "{err}");
    assert!(err.contains("a_2026_11_07_05_34_PM_BC.zip"), "{err}");
    assert!(err.contains("left side"), "{err}");
    assert!(stdout(&out).is_empty());
}

#[test]
fn json_shape() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), &with(&[("gone.txt", b"g")]));
    let r = write_zip(
        &t.path().join("r.zip"),
        &with(&[("readme.txt", b"HELLO\n"), ("added.txt", b"a")]),
    );
    let out = run2(&l, &r, &["--json"]);
    assert_eq!(code(&out), 1);
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "changed",
            "left",
            "match",
            "only_left",
            "only_right",
            "right",
            "skipped",
            "summary"
        ]
    );
    assert_eq!(v["match"], false);
    assert_eq!(v["left"], l.display().to_string());
    assert_eq!(v["only_left"], serde_json::json!(["gone.txt"]));
    assert_eq!(v["only_right"], serde_json::json!(["added.txt"]));
    assert_eq!(v["skipped"], serde_json::json!([]));
    let changed = &v["changed"][0];
    assert_eq!(changed["left"], "readme.txt");
    assert_eq!(changed["reason"], "crc32");
    assert_eq!(changed["left_size"], 6);
    assert_eq!(
        changed["left_crc32"],
        format!("{:08x}", crc32fast::hash(b"hello\n"))
    );
    assert_eq!(
        v["summary"],
        serde_json::json!({
            "changed": 1,
            "only_left": 1,
            "only_right": 1,
            "same": 2,
            "skipped": 0
        })
    );
}

#[test]
fn missing_path_exits_2() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let missing = t.path().join("nope");
    let out = run2(&l, &missing, &[]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());
}

#[test]
fn plain_file_that_is_not_a_zip_exits_2() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let f = t.path().join("plain.txt");
    fs::write(&f, b"x").unwrap();
    let out = run2(&l, &f, &[]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("not a folder or a .zip file"));
}

#[test]
fn broken_zip_exits_2() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let z = t.path().join("broken.zip");
    fs::write(&z, b"this is not a zip").unwrap();
    let out = run2(&l, &z, &[]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("cannot read zip"));
}

#[test]
fn usage_errors_exit_2() {
    let out = run(&["only-one".as_ref()]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("expected 2 paths"));

    let out = run(&["--pair".as_ref(), "(".as_ref(), "a".as_ref(), "b".as_ref()]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("invalid --pair pattern"));
}

#[test]
fn help_exits_0() {
    let out = run(&["--help".as_ref()]);
    assert_eq!(code(&out), 0);
    let expected = format!("Usage: {} [OPTIONS] LEFT RIGHT", env!("CARGO_PKG_NAME"));
    assert!(stdout(&out).starts_with(&expected));
}
