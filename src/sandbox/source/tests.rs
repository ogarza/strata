// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn replacements_with_identical_size_and_mtime_have_distinct_revisions() {
    let root = tempfile::tempdir().expect("fixture directory");
    let path = root.path().join("source");
    std::fs::write(&path, b"old").expect("original source");
    let before = SourceRevision::read(&path).expect("original revision");
    let times = std::fs::FileTimes::new().set_modified(
        std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
    );
    let replacement = root.path().join("replacement");
    std::fs::write(&replacement, b"new").expect("replacement source");
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .expect("replacement file")
        .set_times(times)
        .expect("preserved mtime");
    std::fs::rename(&replacement, &path).expect("replace source");
    assert!(!before.matches(&path));
    let after = SourceRevision::read(&path).expect("new revision");
    assert_eq!(before.size, after.size);
    assert_eq!(before.modified, after.modified);
}

#[test]
fn subsecond_mutations_and_nonregular_sources_are_rejected() {
    let root = tempfile::tempdir().expect("fixture directory");
    let path = root.path().join("source");
    std::fs::write(&path, b"old").expect("source");
    let file = std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("source file");
    let base = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
    file.set_times(std::fs::FileTimes::new().set_modified(base))
        .expect("original mtime");
    let before = SourceRevision::read(&path).expect("revision");
    file.set_times(
        std::fs::FileTimes::new().set_modified(base + std::time::Duration::from_nanos(1)),
    )
    .expect("subsecond mtime");
    assert!(!before.matches(&path));
    assert!(SourceRevision::read(root.path()).is_err());
}

#[test]
fn snapshot_checks_expected_revision_and_cancellation() {
    let root = tempfile::tempdir().expect("fixture directory");
    let path = root.path().join("source");
    std::fs::write(&path, b"old").expect("source");
    let before = SourceRevision::read(&path).expect("revision");
    std::fs::write(&path, b"changed").expect("changed source");
    assert!(
        crate::sandbox::sealed_raster_snapshot_checked(
            &path,
            before,
            &crate::sandbox::Cancellation::default()
        )
        .is_err()
    );
    let cancellation = crate::sandbox::Cancellation::default();
    cancellation.cancel();
    assert!(
        crate::sandbox::sealed_raster_snapshot_checked(
            &path,
            SourceRevision::read(&path).expect("new revision"),
            &cancellation
        )
        .is_err()
    );
}
