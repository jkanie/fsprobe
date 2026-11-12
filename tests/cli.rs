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
            "fsprobe_version",
            "left",
            "links_left",
            "links_right",
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
    assert_eq!(v["links_left"], serde_json::json!([]));
    assert_eq!(v["links_right"], serde_json::json!([]));
    // The version that wrote the report, the same that --version prints.
    let version = run(&[std::ffi::OsStr::new("--version")]);
    assert_eq!(
        stdout(&version),
        format!("fsprobe {}\n", v["fsprobe_version"].as_str().unwrap())
    );
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

#[cfg(unix)]
#[test]
fn symbolic_links_are_listed_not_followed() {
    use std::os::unix::fs::symlink;
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(&t.path().join("r"), &with(&[("a.txt", b"a")]));
    // A link to a file, a link to a folder, a link to nothing, on the left.
    symlink("readme.txt", l.join("a.txt")).unwrap();
    symlink("deep", l.join("docs/latest")).unwrap();
    symlink("../missing", l.join("docs/gone")).unwrap();
    symlink("../readme.txt", r.join("docs/readme-link")).unwrap();

    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "only in right:\n  a.txt\n\
         symbolic links (not compared):\n\
         \x20 left   a.txt -> readme.txt\n\
         \x20 left   docs/gone -> ../missing\n\
         \x20 left   docs/latest -> deep\n\
         \x20 right  docs/readme-link -> ../readme.txt\n\
         summary: 3 same, 0 changed, 0 only-left, 1 only-right, 0 skipped\n"
    );

    // --include and --exclude filter links like files.
    let out = run2(&l, &r, &["--json", "--exclude", "^docs/gone$"]);
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        v["links_left"],
        serde_json::json!([
            { "path": "a.txt", "target": "readme.txt" },
            { "path": "docs/latest", "target": "deep" }
        ])
    );
    assert_eq!(
        v["links_right"],
        serde_json::json!([{ "path": "docs/readme-link", "target": "../readme.txt" }])
    );

    // Links alone do not make 2 trees differ, and --quiet prints nothing.
    let r2 = write_folder(&t.path().join("r2"), BASE);
    symlink("readme.txt", r2.join("link")).unwrap();
    let out = run2(&write_folder(&t.path().join("l2"), BASE), &r2, &["-q"]);
    assert_eq!(code(&out), 0);
    assert_eq!(stdout(&out), "");
}

#[cfg(unix)]
#[test]
fn follow_links_compares_what_links_point_to_and_stops_loops() {
    use std::os::unix::fs::symlink;
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(
        &t.path().join("r"),
        &with(&[
            ("a.txt", b"hello\n"),
            ("docs/latest/data.bin", &[0, 1, 2, 3, 4, 5, 6, 7]),
        ]),
    );
    symlink("readme.txt", l.join("a.txt")).unwrap();
    symlink("deep", l.join("docs/latest")).unwrap();
    symlink("../missing", l.join("docs/gone")).unwrap();
    // A link back to the folder that holds it, and 1 back to the root.
    symlink(".", l.join("docs/self")).unwrap();
    symlink("../..", l.join("docs/deep/up")).unwrap();

    let out = run2(&l, &r, &["--follow-links"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert_eq!(
        stdout(&out),
        "symbolic links (not compared):\n\
         \x20 left   docs/deep/up -> ../..\n\
         \x20 left   docs/gone -> ../missing\n\
         \x20 left   docs/latest/up -> ../..\n\
         \x20 left   docs/self -> .\n\
         summary: 5 same, 0 changed, 0 only-left, 0 only-right, 0 skipped\n"
    );

    // A followed file is read through the link, so a change shows under both
    // the file's own path and the link's path.
    fs::write(l.join("readme.txt"), b"changed\n").unwrap();
    let out = run2(&l, &r, &["--follow-links", "--json"]);
    assert_eq!(code(&out), 1);
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let changed: Vec<_> = v["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["left"].clone())
        .collect();
    assert_eq!(
        changed,
        vec![serde_json::json!("a.txt"), serde_json::json!("readme.txt")]
    );

    // A link loop through 2 folders ends too.
    let m = write_folder(&t.path().join("m"), &[("x/1.txt", b"1"), ("y/2.txt", b"2")]);
    symlink("../y", m.join("x/to-y")).unwrap();
    symlink("../x", m.join("y/to-x")).unwrap();
    let out = run2(&m, &m, &["--follow-links", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["summary"]["same"], 4, "{v}");
    assert_eq!(
        v["links_left"],
        serde_json::json!([
            { "path": "x/to-y/to-x", "target": "../x" },
            { "path": "y/to-x/to-y", "target": "../y" }
        ])
    );
}

#[test]
fn ignore_case_pairs_paths_that_differ_only_in_case() {
    let t = TempDir::new().unwrap();
    let l = write_folder(
        &t.path().join("l"),
        &[("Docs/ReadMe.TXT", b"a"), ("Data.bin", b"12")],
    );
    let r = write_folder(
        &t.path().join("r"),
        &[("docs/readme.txt", b"a"), ("data.bin", b"13")],
    );

    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("summary: 0 same, 0 changed, 2 only-left, 2 only-right"));

    let out = run2(&l, &r, &["--ignore-case", "--json"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["summary"]["same"], 1);
    assert_eq!(v["changed"][0]["left"], "Data.bin");
    assert_eq!(v["changed"][0]["right"], "data.bin");

    // 2 names in 1 tree that differ only in case cannot both pair. Only a
    // file system that tells case apart (as on Linux) can hold both.
    let both = write_folder(&t.path().join("both"), &[("a.txt", b"1"), ("A.TXT", b"2")]);
    if fs::read_dir(&both).unwrap().count() == 2 {
        let out = run2(&both, &r, &["--ignore-case"]);
        assert_eq!(code(&out), 2);
        assert!(stderr(&out).contains("A.TXT"), "{}", stderr(&out));
    }
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

/// Returns 2 different 64-byte strings with the same CRC-32.
///
/// CRC-32 is affine over GF(2), so for strings of equal length, flipping a set
/// of bits changes the CRC-32 by the XOR of each bit's own change. The last 32
/// bits are picked to undo the change from flipping bit 0 of byte 0.
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
    assert_eq!(want, 0);
    for j in 0..32 {
        if pick >> j & 1 == 1 {
            flip(&mut b, j);
        }
    }
    assert_eq!(crc32fast::hash(&a), crc32fast::hash(&b));
    assert_ne!(a, b);
    (a, b)
}

fn twin_folders(t: &TempDir) -> (PathBuf, PathBuf, Vec<u8>, Vec<u8>) {
    let (a, b) = crc32_twins();
    let l = t.path().join("l");
    let r = t.path().join("r");
    fs::create_dir_all(&l).unwrap();
    fs::create_dir_all(&r).unwrap();
    fs::write(l.join("f.bin"), &a).unwrap();
    fs::write(r.join("f.bin"), &b).unwrap();
    (l, r, a, b)
}

#[test]
fn crc32_twins_pass_by_default() {
    let t = TempDir::new().unwrap();
    let (l, r, _, _) = twin_folders(&t);
    let out = run2(&l, &r, &[]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(stdout(&out).contains("1 same, 0 changed"));
}

#[test]
fn verify_catches_crc32_twins() {
    let t = TempDir::new().unwrap();
    let (l, r, _, _) = twin_folders(&t);
    let out = run2(&l, &r, &["--verify"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "changed:\n  f.bin  (bytes differ at offset 0)\n\
         summary: 0 same, 1 changed, 0 only-left, 0 only-right, 0 skipped\n"
    );
}

#[test]
fn blake3_catches_crc32_twins() {
    let t = TempDir::new().unwrap();
    let (l, r, a, b) = twin_folders(&t);
    for flags in [&["--hash", "blake3"][..], &["--hash=blake3"][..]] {
        let out = run2(&l, &r, flags);
        assert_eq!(code(&out), 1, "{}", stderr(&out));
        let expected = format!(
            "  f.bin  (blake3 {} -> {})\n",
            blake3::hash(&a).to_hex(),
            blake3::hash(&b).to_hex()
        );
        assert!(stdout(&out).contains(&expected), "{}", stdout(&out));
    }
}

#[test]
fn verify_json_names_the_offset() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_folder(
        &t.path().join("r"),
        &with(&[("docs/deep/data.bin", &[0, 1, 2, 3, 4, 9, 6, 7])]),
    );
    let out = run2(&l, &r, &["--verify", "--json"]);
    assert_eq!(code(&out), 1);
    let v: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let changed = &v["changed"][0];
    assert_eq!(changed["left"], "docs/deep/data.bin");
    assert_eq!(changed["reason"], "bytes");
    assert_eq!(changed["offset"], 5);
    assert_eq!(v["summary"]["same"], 2);
}

#[test]
fn verify_and_blake3_fall_back_to_crc32_for_zip_entries() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let r = write_zip(
        &t.path().join("r.zip"),
        &with(&[("readme.txt", b"HELLO\n")]),
    );
    for flags in [&["--verify"][..], &["--hash", "blake3"][..]] {
        let out = run2(&l, &r, flags);
        assert_eq!(code(&out), 1);
        let expected = format!(
            "  readme.txt  (crc32 {:08x} -> {:08x})\n",
            crc32fast::hash(b"hello\n"),
            crc32fast::hash(b"HELLO\n")
        );
        assert!(stdout(&out).contains(&expected), "{}", stdout(&out));
        assert!(stdout(&out).contains("2 same, 1 changed"));
    }
}

#[test]
fn unknown_hash_exits_2() {
    let t = TempDir::new().unwrap();
    let l = write_folder(&t.path().join("l"), BASE);
    let out = run2(&l, &l, &["--hash", "md5"]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("unknown --hash 'md5', expected crc32 or blake3"),
        "{}",
        stderr(&out)
    );
}

/// 2 folders with 240 files in 6 sub-folders. Every 4th file has other bytes of
/// the same size, every 6th has a different size, every 9th is on the left only.
/// Each file's name with its left bytes and right bytes (empty when the file is
/// on the left only).
type Files = Vec<(String, Vec<u8>, Vec<u8>)>;

fn many_files(t: &TempDir) -> (PathBuf, PathBuf, Files) {
    let l = t.path().join("l");
    let r = t.path().join("r");
    let mut pairs = Vec::new();
    for i in 0..240u32 {
        let name = format!("s{}/n{:03}.dat", i % 6, (i * 53) % 997);
        let left: Vec<u8> = (0..(i * 131) % 5_000 + 1).map(|j| (j ^ i) as u8).collect();
        let mut right = left.clone();
        if i % 4 == 0 {
            right[0] ^= 0xff;
        }
        if i % 6 == 0 {
            right.push(0);
        }
        let path = l.join(&name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &left).unwrap();
        if i % 9 != 0 {
            let path = r.join(&name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &right).unwrap();
        }
        pairs.push((name, left, if i % 9 == 0 { Vec::new() } else { right }));
    }
    pairs.sort();
    (l, r, pairs)
}

#[test]
fn many_files_print_in_path_order_on_every_run() {
    let t = TempDir::new().unwrap();
    let (l, r, pairs) = many_files(&t);
    let (mut changed, mut only_left, mut same) = (String::new(), String::new(), 0);
    for (name, a, b) in &pairs {
        if b.is_empty() {
            only_left.push_str(&format!("  {name}\n"));
        } else if a.len() != b.len() {
            changed.push_str(&format!(
                "  {name}  (size {} -> {} bytes)\n",
                a.len(),
                b.len()
            ));
        } else if a != b {
            changed.push_str(&format!(
                "  {name}  (crc32 {:08x} -> {:08x})\n",
                crc32fast::hash(a),
                crc32fast::hash(b)
            ));
        } else {
            same += 1;
        }
    }
    let n_changed = changed.lines().count();
    let n_left = only_left.lines().count();
    assert!(same > 50 && n_changed > 50 && n_left > 20);
    let expected = format!(
        "changed:\n{changed}only in left:\n{only_left}\
         summary: {same} same, {n_changed} changed, {n_left} only-left, 0 only-right, 0 skipped\n"
    );
    for _ in 0..5 {
        let out = run2(&l, &r, &[]);
        assert_eq!(code(&out), 1, "{}", stderr(&out));
        assert_eq!(stdout(&out), expected);
    }
    let summary = format!("summary: {same} same, {n_changed} changed");
    for flags in [
        &["--verify"][..],
        &["--hash", "blake3"][..],
        &["--json"][..],
    ] {
        let first = stdout(&run2(&l, &r, flags));
        if flags == ["--json"] {
            let v: Value = serde_json::from_str(&first).unwrap();
            assert_eq!(v["summary"]["changed"], n_changed);
            assert_eq!(v["summary"]["same"], same);
        } else {
            assert!(first.contains(&summary), "{first}");
        }
        for _ in 0..4 {
            assert_eq!(stdout(&run2(&l, &r, flags)), first, "{flags:?}");
        }
    }
}

/// Sets every file under `root` to a modified time 1 hour in the past, such
/// that the cache stores their digests.
fn age_all(root: &Path) {
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for entry in walk(root) {
        fs::File::options()
            .write(true)
            .open(&entry)
            .unwrap()
            .set_modified(then)
            .unwrap();
    }
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for item in fs::read_dir(root).unwrap() {
        let path = item.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn cache_answers_the_2nd_run() {
    let tmp = TempDir::new().unwrap();
    let l = write_folder(&tmp.path().join("l"), BASE);
    let r = write_folder(&tmp.path().join("r"), &with(&[("readme.txt", b"HELLO\n")]));
    age_all(&l);
    age_all(&r);
    let cache = tmp.path().join("digests.json");
    let cache = cache.to_str().unwrap();

    let first = run2(&l, &r, &["--cache", cache]);
    assert_eq!(code(&first), 1);
    let text = stdout(&first);
    assert!(text.contains("readme.txt  (crc32 "), "{text}");
    assert!(
        text.ends_with("cache: 0 from the cache, 6 read\n"),
        "{text}"
    );

    let second = run2(&l, &r, &["--cache", cache]);
    assert_eq!(code(&second), 1);
    let again = stdout(&second);
    assert!(
        again.ends_with("cache: 6 from the cache, 0 read\n"),
        "{again}"
    );
    // Apart from the cache line, the 2 reports are the same bytes.
    assert_eq!(
        text.lines()
            .filter(|l| !l.starts_with("cache:"))
            .collect::<Vec<_>>(),
        again
            .lines()
            .filter(|l| !l.starts_with("cache:"))
            .collect::<Vec<_>>()
    );

    let json = run2(&l, &r, &["--cache", cache, "--json"]);
    let v: Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(v["cache"]["from_cache"], 6);
    assert_eq!(v["cache"]["read"], 0);
    assert_eq!(v["cache"]["file"], cache);

    // Without --cache, the output has no cache line or key.
    let plain = run2(&l, &r, &[]);
    assert!(!stdout(&plain).contains("cache:"));
    let plain_json: Value = serde_json::from_str(&stdout(&run2(&l, &r, &["--json"]))).unwrap();
    assert!(plain_json.get("cache").is_none());
}

#[test]
fn a_broken_cache_file_exits_2() {
    let tmp = TempDir::new().unwrap();
    let l = write_folder(&tmp.path().join("l"), BASE);
    let r = write_folder(&tmp.path().join("r"), BASE);
    let cache = tmp.path().join("digests.json");
    fs::write(&cache, "{ not json").unwrap();
    let out = run2(&l, &r, &["--cache", cache.to_str().unwrap()]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("is not a fsprobe cache file"),
        "{}",
        stderr(&out)
    );
    // The broken file is left as it was.
    assert_eq!(fs::read_to_string(&cache).unwrap(), "{ not json");
}

#[test]
fn threads_flag_gives_the_same_report() {
    let tmp = TempDir::new().unwrap();
    let l = write_folder(&tmp.path().join("l"), BASE);
    let r = write_folder(
        &tmp.path().join("r"),
        &with(&[("readme.txt", b"HELLO\n"), ("docs/new.txt", b"new\n")]),
    );
    let default = run2(&l, &r, &["--json"]);
    assert_eq!(code(&default), 1);
    for n in ["1", "2", "64"] {
        let out = run2(&l, &r, &["--json", "--threads", n]);
        assert_eq!(code(&out), 1, "--threads {n}");
        assert_eq!(stdout(&out), stdout(&default), "--threads {n}");
    }
    let bad = run2(&l, &r, &["--threads", "0"]);
    assert_eq!(code(&bad), 2);
    assert!(
        stderr(&bad).contains("--threads needs a whole number of 1 or more, got '0'"),
        "{}",
        stderr(&bad)
    );
}

#[test]
fn quiet_prints_nothing_and_keeps_the_exit_code() {
    let tmp = TempDir::new().unwrap();
    let l = write_folder(&tmp.path().join("l"), BASE);
    let same = write_folder(&tmp.path().join("same"), BASE);
    let r = write_folder(&tmp.path().join("r"), &with(&[("readme.txt", b"HELLO\n")]));
    for flags in [&["-q"][..], &["--quiet"], &["--quiet", "--json"]] {
        let out = run2(&l, &same, flags);
        assert_eq!(code(&out), 0, "{flags:?}");
        assert_eq!(stdout(&out), "", "{flags:?}");
        let out = run2(&l, &r, flags);
        assert_eq!(code(&out), 1, "{flags:?}");
        assert_eq!(stdout(&out), "", "{flags:?}");
        assert_eq!(stderr(&out), "", "{flags:?}");
    }
    let missing = tmp.path().join("missing");
    let out = run2(&l, &missing, &["--quiet"]);
    assert_eq!(code(&out), 2);
    assert_eq!(stdout(&out), "");
    assert!(!stderr(&out).is_empty());
}
