// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn raw_pixels_persist_with_freedesktop_tags_and_a_strong_revision() {
    let (_guard, bucket) = BucketGuard::unique("raw-revision");
    let file = tempfile::NamedTempFile::new().expect("source fixture");
    std::fs::write(file.path(), b"source").expect("source bytes");
    file.as_file()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
        )
        .expect("deterministic mtime");
    let revision = crate::sandbox::SourceRevision::read(file.path()).expect("source revision");
    let render = crate::sandbox::ThumbnailRender::Raw {
        pixels: vec![255, 0, 0, 128],
        width: 1,
        height: 1,
        stride: 4,
    };
    super::super::store_render(file.path(), revision, &render);
    let png =
        super::super::lookup_revision(file.path(), revision).expect("persisted raw thumbnail");
    assert_eq!(png_dimensions(&png), Some((256, 256)));
    assert_eq!(
        read_thumb_tags(&png).expect("cache tags").1,
        revision.modified.to_string()
    );
    assert_eq!(
        super::super::read_text_tag(&png, b"Strata::Revision"),
        Some(revision.cache_stamp())
    );
    let mtime = std::fs::metadata(file.path())
        .expect("metadata")
        .modified()
        .expect("mtime");
    std::fs::write(file.path(), b"change").expect("same-size modification");
    file.as_file()
        .set_times(
            std::fs::FileTimes::new().set_modified(mtime + std::time::Duration::from_nanos(1)),
        )
        .expect("subsecond mtime");
    let changed = crate::sandbox::SourceRevision::read(file.path()).expect("changed revision");
    assert_eq!(changed.modified, revision.modified);
    assert!(super::super::lookup_revision(file.path(), changed).is_none());
    std::fs::remove_dir_all(bucket).expect("remove disposable cache");
}

#[test]
fn persistence_drops_results_after_source_replacement() {
    let (_guard, bucket) = BucketGuard::unique("stale-render");
    let root = tempfile::tempdir().expect("source directory");
    let path = root.path().join("image");
    std::fs::write(&path, b"old").expect("source");
    let revision = crate::sandbox::SourceRevision::read(&path).expect("source revision");
    let replacement = root.path().join("replacement");
    std::fs::write(&replacement, b"new").expect("replacement");
    std::fs::rename(replacement, &path).expect("replace source");
    super::super::store_render(
        &path,
        revision,
        &crate::sandbox::ThumbnailRender::Raw {
            pixels: vec![255; 4],
            width: 1,
            height: 1,
            stride: 4,
        },
    );
    assert!(!bucket.exists());
}
