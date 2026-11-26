use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], paths: &[&Path]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_word-hoard"));
    cmd.args(args);
    for p in paths {
        cmd.arg(p);
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "word-hoard {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn device() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("KOBOeReader");
    hoard_kobo::fixture::create_device(&root).unwrap();
    (tmp, root)
}

#[test]
fn exports_markdown_per_book() {
    let (tmp, root) = device();
    let out = tmp.path().join("out");
    let db_before = fs::read(root.join(".kobo/KoboReader.sqlite")).unwrap();

    let output = run(&["kobo", "export", "--device"], &[&root, Path::new("--out"), &out]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Exported 4 highlights, 1 note and 1 bookmark from 3 books"),
        "{stdout}"
    );

    let md = fs::read_to_string(out.join("Ursula K. Le Guin - The Dispossessed.md")).unwrap();
    assert!(md.contains("source: \"kobo:N418170000001\""));
    let pos = |s: &str| md.find(s).unwrap_or_else(|| panic!("missing {s:?} in\n{md}"));
    assert!(pos("There was a wall.") < pos("You cannot buy") && pos("You cannot buy") < pos("True journey"));
    assert!(md.contains("**Note:** Compare with Graeber."));
    assert!(!md.contains("Deleted highlight"));
    assert!(out.join("Susanna Clarke - Piranesi.md").exists());
    let gone = fs::read_to_string(out.join("Gone Book.md")).unwrap();
    assert!(gone.contains("on_device: false"));

    assert_eq!(fs::read(root.join(".kobo/KoboReader.sqlite")).unwrap(), db_before);
}

#[test]
fn exports_json_with_hidden() {
    let (tmp, root) = device();
    let out = tmp.path().join("json");
    run(
        &["kobo", "export", "--format", "json", "--include-hidden", "--device"],
        &[&root, Path::new("--out"), &out],
    );
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out.join("kobo-annotations.json")).unwrap()).unwrap();
    let total: usize = value
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["annotations"].as_array().unwrap().len())
        .sum();
    assert_eq!(total, 7);
}

#[test]
fn info_and_report() {
    let (_tmp, root) = device();
    let info = String::from_utf8(run(&["kobo", "info", "--device"], &[&root]).stdout).unwrap();
    assert!(info.contains("model        Kobo Libra Colour"), "{info}");
    assert!(info.contains("firmware     4.38.21908"), "{info}");
    assert!(info.contains("books        3 (2 sideloaded, 1 store)"), "{info}");
    assert!(info.contains("hidden       1"), "{info}");

    let report = String::from_utf8(run(&["kobo", "report", "--device"], &[&root]).stdout).unwrap();
    assert!(report.contains("CREATE TABLE Bookmark"));
    assert!(!report.contains("N418170000001"));
}

#[test]
fn refuses_to_export_onto_the_kobo() {
    let (_tmp, root) = device();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_word-hoard"))
        .args(["kobo", "export", "--device"])
        .arg(&root)
        .arg("--out")
        .arg(root.join("highlights"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("doesn't write to the device"));
    assert!(!root.join("highlights").exists());
}

#[test]
fn reads_a_copied_database_file() {
    let (tmp, root) = device();
    let copy = tmp.path().join("copy.sqlite");
    fs::copy(root.join(".kobo/KoboReader.sqlite"), &copy).unwrap();
    let info = String::from_utf8(run(&["kobo", "info", "--db"], &[&copy]).stdout).unwrap();
    assert!(info.contains("DbVersion 174"), "{info}");
}
