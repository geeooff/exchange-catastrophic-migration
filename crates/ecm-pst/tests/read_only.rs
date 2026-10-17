//! A PST source is read, never written.

use std::{
    fs,
    path::{Path, PathBuf},
};

use ecm_pst::{Error, PstFormat, PstSource};

/// A private copy of a fixture, so a test that fails half-way cannot damage the
/// tracked file and tests can run in parallel.
fn fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
        &path,
    )
    .unwrap();
    (dir, path)
}

#[test]
fn reads_the_folder_tree_of_a_unicode_pst() {
    let (_dir, path) = fixture("Empty.pst");
    let source = PstSource::open(&path).unwrap();

    assert_eq!(source.format(), PstFormat::Unicode);
    assert_eq!(source.display_name().as_deref(), Some("Empty"));

    let ipm = source.ipm_folders().unwrap();
    assert_eq!(ipm.name, "Top of Outlook data file");
    let names: Vec<_> = ipm.children.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Deleted Items"]);
    assert_eq!(ipm.total_item_count(), 0);

    let all = source.all_folders().unwrap();
    assert!(
        all.children
            .iter()
            .any(|f| f.name == "Top of Outlook data file"),
        "the store root contains the IPM subtree"
    );
    assert!(all.folder_count() > ipm.folder_count());
}

#[test]
fn check_leaves_the_file_byte_for_byte_unchanged() {
    let (_dir, path) = fixture("Empty.pst");
    let bytes = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();

    let report = ecm_pst::check(&path, true).unwrap();

    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    assert_eq!(report.size, bytes.len() as u64);
    assert_eq!(report.sha256, ecm_pst::sha256_file(&path).unwrap());
}

#[test]
fn a_read_only_file_can_be_checked() {
    let (_dir, path) = fixture("Empty.pst");
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions.clone()).unwrap();

    let result = ecm_pst::check(&path, false);

    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
    result.unwrap();
}

/// While a source is open, nobody else can write to the file: the handle shares
/// read access only.
#[cfg(windows)]
#[test]
fn other_writers_are_locked_out_while_the_source_is_open() {
    let (_dir, path) = fixture("Empty.pst");
    let source = PstSource::open(&path).unwrap();

    let writer = fs::OpenOptions::new().write(true).open(&path);
    assert!(
        writer.is_err(),
        "a writer could open the PST while it is read"
    );
    let reader = fs::File::open(&path);
    assert!(reader.is_ok(), "readers are still allowed");

    drop(source);
    drop(reader);
    assert!(fs::OpenOptions::new().write(true).open(&path).is_ok());
}

#[test]
fn a_file_that_is_not_a_pst_is_a_format_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not.pst");
    fs::write(&path, b"!BDN but not really a PST file").unwrap();

    assert!(matches!(PstSource::open(&path), Err(Error::Format { .. })));
}

#[test]
fn a_missing_file_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.pst");

    assert!(matches!(
        ecm_pst::check(&path, false),
        Err(Error::Io { .. })
    ));
}
