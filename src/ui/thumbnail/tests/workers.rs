// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn queued_lookup_cancellation_removes_the_queue_entry_before_rebind() {
    clear_thumbnail_runtime();
    let key = key(602);
    let target = PendingTarget {
        image_id: 1,
        request: 1,
        image: glib::WeakRef::new(),
    };
    assert!(super::super::schedule_thumbnail(
        key.clone(),
        ThumbnailKind::Image,
        target.clone()
    ));
    cancel_thumbnail(1);
    super::super::LOOKUP_QUEUE.with(|queue| assert!(queue.borrow().is_empty()));
    assert!(super::super::schedule_thumbnail(
        key,
        ThumbnailKind::Image,
        target
    ));
    super::super::LOOKUP_QUEUE.with(|queue| assert_eq!(queue.borrow().len(), 1));
    clear_thumbnail_runtime();
}

#[test]
fn abandoned_corrupt_cache_decode_does_not_promote_to_render() {
    clear_thumbnail_runtime();
    let key = key(603);
    let cancellation = crate::sandbox::Cancellation::default();
    PENDING_THUMBNAILS.with(|pending| {
        pending.borrow_mut().insert(
            key.clone(),
            PendingThumbnail {
                queued_at: Instant::now(),
                id: 1,
                executing: true,
                kind: ThumbnailKind::Image,
                cancellation: cancellation.clone(),
                targets: Vec::new(),
            },
        )
    });
    super::super::finish_thumbnail_render_again(ThumbnailJob {
        id: 1,
        key: key.clone(),
        kind: ThumbnailKind::Image,
        cancellation,
    });
    PENDING_THUMBNAILS.with(|pending| assert!(!pending.borrow().contains_key(&key)));
    THUMBNAIL_QUEUE.with(|queue| {
        assert_eq!(queue.borrow().running, 0);
        assert!(queue.borrow().queued.is_empty());
    });
    clear_thumbnail_runtime();
}

#[test]
fn heavy_only_queue_waits_for_the_active_heavy_execution() {
    let mut queue = ThumbnailQueue::default();
    for index in 0..4 {
        assert!(queue.enqueue_kind(key(index), ThumbnailKind::Pdf));
    }
    assert_eq!(queue.begin_next(), Some(key(0)));
    assert!(queue.begin_next().is_none());
    assert_eq!(queue.heavy_running, 1);
    assert!(queue.enqueue_kind(key(4), ThumbnailKind::Image));
    assert_eq!(queue.begin_next(), Some(key(4)));
    assert!(queue.begin_next().is_none());
    queue.finish_key(&key(0));
    assert_eq!(queue.begin_next(), Some(key(1)));
    queue.finish_key(&key(0));
    assert_eq!(queue.running, 2);
    assert_eq!(queue.heavy_running, 1);
}

#[test]
fn active_execution_survives_detachment_and_accepts_reattachment() {
    clear_thumbnail_runtime();
    let key = key(600);
    let cancellation = crate::sandbox::Cancellation::default();
    PENDING_THUMBNAILS.with(|pending| {
        pending.borrow_mut().insert(
            key.clone(),
            PendingThumbnail {
                queued_at: Instant::now(),
                id: 100,
                executing: true,
                kind: ThumbnailKind::Pdf,
                cancellation: cancellation.clone(),
                targets: vec![PendingTarget {
                    image_id: 1,
                    request: 1,
                    image: glib::WeakRef::new(),
                }],
            },
        )
    });
    cancel_thumbnail(1);
    assert!(!cancellation.is_cancelled());
    PENDING_THUMBNAILS.with(|pending| assert!(pending.borrow()[&key].targets.is_empty()));
    assert!(super::super::schedule_thumbnail(
        key.clone(),
        ThumbnailKind::Pdf,
        PendingTarget {
            image_id: 2,
            request: 2,
            image: glib::WeakRef::new()
        }
    ));
    PENDING_THUMBNAILS.with(|pending| {
        let pending = pending.borrow();
        assert_eq!(pending[&key].id, 100);
        assert_eq!(pending[&key].targets.len(), 1);
    });
    clear_thumbnail_runtime();
}

#[test]
fn stale_execution_cannot_release_a_replacement_permit() {
    clear_thumbnail_runtime();
    let key = key(601);
    THUMBNAIL_QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        queue.enqueue_kind(key.clone(), ThumbnailKind::Pdf);
        queue.begin_next().expect("admitted heavy job");
        queue.active_ids.insert(key.clone(), 20);
    });
    super::super::finish_thumbnail_slot(&key, 19);
    THUMBNAIL_QUEUE.with(|queue| assert_eq!(queue.borrow().running, 1));
    super::super::finish_thumbnail_slot(&key, 20);
    THUMBNAIL_QUEUE.with(|queue| assert_eq!(queue.borrow().running, 0));
    super::super::finish_thumbnail_slot(&key, 20);
    THUMBNAIL_QUEUE.with(|queue| assert_eq!(queue.borrow().running, 0));
    clear_thumbnail_runtime();
}

#[test]
fn known_metadata_is_replaced_by_the_opened_sources_actual_revision() {
    let file = tempfile::NamedTempFile::new().expect("source fixture");
    std::fs::write(file.path(), b"fixture").expect("source bytes");
    let unresolved = ThumbnailKey {
        path: file.path().to_owned(),
        revision: None,
        modified: Some(1),
        file_size: Some(1),
    };
    let resolved = resolve_source_key(&unresolved);
    assert_eq!(resolved.file_size, Some(7));
    assert_ne!(resolved.modified, Some(1));
    assert!(super::super::source_matches(&resolved));
    std::fs::write(file.path(), b"new fixture").expect("modified source");
    assert!(!super::super::source_matches(&resolved));
}

#[test]
fn resolving_into_an_existing_execution_only_merges_consumers() {
    clear_thumbnail_runtime();
    let file = tempfile::NamedTempFile::new().expect("source fixture");
    let unresolved = ThumbnailKey {
        path: file.path().to_owned(),
        revision: None,
        modified: None,
        file_size: None,
    };
    let resolved = resolve_source_key(&unresolved);
    for (key, id) in [(&unresolved, 1), (&resolved, 2)] {
        PENDING_THUMBNAILS.with(|pending| {
            pending.borrow_mut().insert(
                key.clone(),
                PendingThumbnail {
                    queued_at: Instant::now(),
                    id,
                    executing: true,
                    kind: ThumbnailKind::Image,
                    cancellation: crate::sandbox::Cancellation::default(),
                    targets: vec![PendingTarget {
                        image_id: id as usize,
                        request: id,
                        image: glib::WeakRef::new(),
                    }],
                },
            )
        });
    }
    let job = ThumbnailJob {
        id: 1,
        key: unresolved,
        kind: ThumbnailKind::Image,
        cancellation: crate::sandbox::Cancellation::default(),
    };
    assert!(super::super::rekey_pending_thumbnail(&job, resolved.clone()).is_none());
    PENDING_THUMBNAILS.with(|pending| {
        let pending = pending.borrow();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[&resolved].id, 2);
        assert_eq!(pending[&resolved].targets.len(), 2);
    });
    clear_thumbnail_runtime();
}
