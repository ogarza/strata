// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

#[test]
fn offscreen_parks_do_not_spin_and_scroll_wakes_them_without_rebinding() {
    gtk_test(
        "ui::thumbnail::viewport::tests::scheduling::offscreen_parks_do_not_spin_and_scroll_wakes_them_without_rebinding",
        || {
            let fixture = Fixture::new();
            let (_image, target) = fixture.target(1200.0);
            thumbnail::LOOKUP_ACTIVE.with(|active| active.borrow_mut().extend([10001, 10002]));
            thumbnail::park_thumbnail(key(0), thumbnail::ThumbnailKind::Image, target.clone());
            wait_until(|| thumbnail::PIPELINE_PUMP.with(|pump| pump.borrow().is_none()));
            thumbnail::LOOKUP_QUEUE.with(|queue| assert!(queue.borrow().is_empty()));
            thumbnail::PENDING_THUMBNAILS.with(|pending| assert!(pending.borrow().is_empty()));
            thumbnail::ACTIVE_REQUESTS
                .with(|requests| assert!(requests.borrow()[&target.image_id].deferred.is_some()));
            for _ in 0..8 {
                thumbnail::pump_thumbnail_jobs();
            }
            thumbnail::PIPELINE_PUMP.with(|pump| assert!(pump.borrow().is_none()));
            fixture.scroll.vadjustment().set_value(1200.0);
            wait_until(|| {
                thumbnail::PENDING_THUMBNAILS.with(|pending| pending.borrow().contains_key(&key(0)))
            });
            assert_eq!(priority(&target), Priority::Visible);
        },
    );
}

#[test]
fn nested_horizontal_clipping_is_checked_before_dispatch() {
    gtk_test(
        "ui::thumbnail::viewport::tests::scheduling::nested_horizontal_clipping_is_checked_before_dispatch",
        || {
            let image = ThumbnailSlot::new(32);
            let inner = gtk::ScrolledWindow::builder()
                .child(&image)
                .min_content_width(240)
                .build();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            spacer.set_size_request(800, 100);
            row.append(&spacer);
            row.append(&inner);
            let outer = gtk::ScrolledWindow::builder().child(&row).build();
            let window = gtk::Window::builder()
                .default_width(240)
                .default_height(240)
                .child(&outer)
                .build();
            window.present();
            wait_until(|| image.is_mapped() && image.width() > 0);
            let target = thumbnail::register_active_request(&image, image.as_ptr() as usize, 4000);
            thumbnail::LOOKUP_ACTIVE.with(|active| active.borrow_mut().extend([10001, 10002]));
            thumbnail::park_thumbnail(key(0), thumbnail::ThumbnailKind::Image, target.clone());
            assert_eq!(priority(&target), Priority::Deferred);
            assert_eq!(ancestors(&image), vec![inner, outer.clone()]);
            outer.hadjustment().set_value(800.0);
            wait_until(|| priority(&target) == Priority::Visible);
            wait_until(|| {
                thumbnail::PENDING_THUMBNAILS.with(|pending| pending.borrow().contains_key(&key(0)))
            });
            window.destroy();
            thumbnail::clear_thumbnail_runtime();
        },
    );
}

#[test]
fn temporary_pause_retains_render_progress_without_restarting_lookup() {
    gtk_test(
        "ui::thumbnail::viewport::tests::scheduling::temporary_pause_retains_render_progress_without_restarting_lookup",
        || {
            let fixture = Fixture::new();
            let (_image, target) = fixture.target(16.0);
            thumbnail::schedule_thumbnail(key(0), thumbnail::ThumbnailKind::Image, target);
            thumbnail::LOOKUP_QUEUE.with(|queue| queue.borrow_mut().clear());
            thumbnail::THUMBNAIL_QUEUE.with(|queue| {
                queue
                    .borrow_mut()
                    .enqueue_kind(key(0), thumbnail::ThumbnailKind::Image)
            });
            thumbnail::set_viewport_scrolling(&fixture.scroll, true);
            thumbnail::pump_thumbnail_jobs();
            thumbnail::PENDING_THUMBNAILS
                .with(|pending| assert!(!pending.borrow()[&key(0)].executing));
            thumbnail::THUMBNAIL_QUEUE.with(|queue| {
                assert_eq!(queue.borrow().running, 0);
                assert_eq!(
                    queue.borrow().queued.front().map(|(key, _)| key),
                    Some(&key(0))
                );
            });
            thumbnail::LOOKUP_QUEUE.with(|queue| assert!(queue.borrow().is_empty()));
        },
    );
}

#[test]
fn prefetch_cannot_occupy_renderers_while_visible_lookups_are_pending() {
    gtk_test(
        "ui::thumbnail::viewport::tests::scheduling::prefetch_cannot_occupy_renderers_while_visible_lookups_are_pending",
        || {
            let fixture = Fixture::new();
            let (_near_image, near) =
                fixture.target(fixture.scroll.vadjustment().page_size() + 8.0);
            let (_visible_image, visible) = fixture.target(16.0);
            thumbnail::schedule_thumbnail(key(0), thumbnail::ThumbnailKind::Image, near);
            thumbnail::LOOKUP_QUEUE.with(|queue| queue.borrow_mut().clear());
            thumbnail::THUMBNAIL_QUEUE.with(|queue| {
                queue
                    .borrow_mut()
                    .enqueue_kind(key(0), thumbnail::ThumbnailKind::Image)
            });
            thumbnail::schedule_thumbnail(key(1), thumbnail::ThumbnailKind::Image, visible.clone());
            thumbnail::dispatch_render_jobs();
            thumbnail::THUMBNAIL_QUEUE.with(|queue| {
                assert_eq!(queue.borrow().running, 0);
                assert_eq!(queue.borrow().queued.len(), 1);
            });
            let id = thumbnail::PENDING_THUMBNAILS.with(|pending| {
                let mut pending = pending.borrow_mut();
                let lookup = pending.get_mut(&key(1)).expect("visible lookup");
                lookup.executing = true;
                lookup.id
            });
            thumbnail::LOOKUP_QUEUE.with(|queue| queue.borrow_mut().clear());
            thumbnail::LOOKUP_ACTIVE.with(|active| active.borrow_mut().insert(id));
            thumbnail::dispatch_render_jobs();
            thumbnail::THUMBNAIL_QUEUE.with(|queue| assert_eq!(queue.borrow().running, 0));
            thumbnail::cancel_thumbnail(visible.image_id);
            assert!(!thumbnail::visible_lookup_pending());
        },
    );
}

struct LookupBlockers(Vec<std::sync::mpsc::Sender<()>>);
impl LookupBlockers {
    fn new() -> Self {
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (release, receive) = std::sync::mpsc::channel();
            let (started, running) = std::sync::mpsc::channel();
            thumbnail::decode::submit_owned(
                move || {
                    started.send(()).expect("lookup blocker started");
                    let _released = receive.recv_timeout(Duration::from_secs(5));
                },
                |_| {},
            )
            .expect("lookup blocker admission");
            running
                .recv_timeout(Duration::from_secs(5))
                .expect("lookup worker running");
            releases.push(release);
        }
        Self(releases)
    }
}
impl Drop for LookupBlockers {
    fn drop(&mut self) {
        for release in &self.0 {
            let _sent = release.send(());
        }
    }
}

#[test]
fn production_pump_submits_two_lookups_and_reorders_remaining_work_after_scroll() {
    gtk_test(
        "ui::thumbnail::viewport::tests::scheduling::production_pump_submits_two_lookups_and_reorders_remaining_work_after_scroll",
        || {
            let fixture = Fixture::new();
            let blockers = LookupBlockers::new();
            let root = tempfile::tempdir().expect("sources");
            let texture = gdk::MemoryTexture::new(
                1,
                1,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(vec![30, 80, 120, 255]),
                4,
            )
            .upcast::<gdk::Texture>();
            let mut images = Vec::new();
            let mut keys = Vec::new();
            for (index, y) in [16.0, 64.0, 112.0, 160.0, 1200.0].into_iter().enumerate() {
                let (image, target) = fixture.target(y);
                let path = root.path().join(format!("{index}.png"));
                texture.save_to_png(&path).expect("source PNG");
                let mut source = key(index);
                source.path = path;
                let resolved = thumbnail::resolve_source_key(&source);
                thumbnail::THUMBNAIL_CACHE
                    .with(|cache| cache.borrow_mut().insert(resolved, texture.clone(), 4));
                thumbnail::schedule_thumbnail(
                    source.clone(),
                    thumbnail::ThumbnailKind::Image,
                    target,
                );
                keys.push(source);
                images.push(image);
            }
            thumbnail::pump_thumbnail_jobs();
            thumbnail::LOOKUP_ACTIVE.with(|active| assert_eq!(active.borrow().len(), 2));
            thumbnail::LOOKUP_QUEUE.with(|queue| assert_eq!(queue.borrow().len(), 2));
            thumbnail::PENDING_THUMBNAILS.with(|pending| {
                assert_eq!(
                    pending
                        .borrow()
                        .values()
                        .filter(|pending| pending.executing)
                        .count(),
                    2
                )
            });
            fixture.scroll.vadjustment().set_value(1200.0);
            wait_until(|| widget_priority(&images[4], true) == Priority::Visible);
            thumbnail::pump_thumbnail_jobs();
            thumbnail::LOOKUP_QUEUE.with(|queue| {
                assert_eq!(queue.borrow().iter().collect::<Vec<_>>(), vec![&keys[4]])
            });
            drop(blockers);
            thumbnail::start_thumbnail_jobs();
            wait_until(|| images[4].texture().is_some());
            thumbnail::LOOKUP_ACTIVE.with(|active| assert!(active.borrow().is_empty()));
            thumbnail::THUMBNAIL_QUEUE.with(|queue| assert_eq!(queue.borrow().running, 0));
            assert!(images[2].texture().is_none());
            assert!(images[3].texture().is_none());
        },
    );
}
