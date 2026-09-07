// SPDX-License-Identifier: GPL-3.0-or-later

mod paint;
mod scheduling;

use super::super as thumbnail;
use super::*;
use crate::test_support::gtk_test;
use std::time::Duration;

fn key(index: usize) -> thumbnail::ThumbnailKey {
    thumbnail::ThumbnailKey {
        path: PathBuf::from(format!("/viewport-fixture/{index}.png")),
        revision: None,
        modified: None,
        file_size: None,
    }
}

#[track_caller]
fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "viewport test deadline");
        glib::MainContext::default().iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Fixture {
    window: gtk::Window,
    scroll: gtk::ScrolledWindow,
    content: gtk::Fixed,
}
impl Fixture {
    fn new() -> Self {
        let content = gtk::Fixed::new();
        content.set_size_request(180, 2400);
        let scroll = gtk::ScrolledWindow::builder().child(&content).build();
        let window = gtk::Window::builder()
            .default_width(240)
            .default_height(240)
            .child(&scroll)
            .build();
        window.present();
        wait_until(|| scroll.is_mapped() && scroll.vadjustment().page_size() > 100.0);
        Self {
            window,
            scroll,
            content,
        }
    }
    fn target(&self, y: f64) -> (ThumbnailSlot, PendingTarget) {
        let image = ThumbnailSlot::new(32);
        self.content.put(&image, 16.0, y);
        wait_until(|| image.is_mapped() && image.width() > 0);
        let id = thumbnail::NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let target = thumbnail::register_active_request(&image, image.as_ptr() as usize, id);
        (image, target)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.window.destroy();
        thumbnail::clear_thumbnail_runtime();
    }
}

#[test]
fn separates_intersection_prefetch_and_offscreen_in_both_axes() {
    assert_eq!(
        rect_priority(10.0, 10.0, 32.0, 32.0, 200.0, 200.0),
        Priority::Visible
    );
    assert_eq!(
        rect_priority(-20.0, 10.0, 32.0, 32.0, 200.0, 200.0),
        Priority::Visible
    );
    assert_eq!(
        rect_priority(200.0, 10.0, 32.0, 32.0, 200.0, 200.0),
        Priority::Nearby
    );
    assert_eq!(
        rect_priority(10.0, 200.0, 32.0, 32.0, 200.0, 200.0),
        Priority::Nearby
    );
    assert_eq!(
        rect_priority(250.0, 10.0, 32.0, 32.0, 200.0, 200.0),
        Priority::Deferred
    );
    assert_eq!(
        rect_priority(10.0, -82.0, 32.0, 32.0, 200.0, 200.0),
        Priority::Deferred
    );
    assert_eq!(
        rect_priority(f32::NAN, 0.0, 32.0, 32.0, 200.0, 200.0),
        Priority::Deferred
    );
    assert_eq!(
        rect_priority(0.0, 0.0, 32.0, 32.0, f32::INFINITY, 200.0),
        Priority::Deferred
    );
    assert_eq!(
        rect_priority(0.0, 0.0, 0.0, 32.0, 200.0, 200.0),
        Priority::Deferred
    );
}

#[test]
fn visible_render_jobs_pass_prefetch_without_changing_worker_or_heavy_limits() {
    let mut queue = thumbnail::ThumbnailQueue::default();
    for index in 0..48 {
        assert!(queue.enqueue_kind(key(index), thumbnail::ThumbnailKind::Image));
    }
    assert!(queue.enqueue_kind(key(48), thumbnail::ThumbnailKind::Pdf));
    for index in 49..53 {
        assert!(queue.enqueue_kind(key(index), thumbnail::ThumbnailKind::Image));
    }
    let priority = |key: &thumbnail::ThumbnailKey| {
        if key
            .path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<usize>().ok())
            .is_some_and(|index| index >= 48)
        {
            Priority::Visible
        } else {
            Priority::Nearby
        }
    };
    for index in [49, 50, 51, 48] {
        assert_eq!(queue.begin_next_prioritized(priority), Some(key(index)));
    }
    assert!(queue.begin_next_prioritized(priority).is_none());
    assert_eq!(queue.heavy_running, 1);
    queue.finish_key(&key(49));
    assert_eq!(queue.begin_next_prioritized(priority), Some(key(52)));
    assert_eq!(queue.running, 4);
}

#[test]
fn blocked_visible_heavy_work_does_not_waste_raster_capacity() {
    let mut queue = thumbnail::ThumbnailQueue::default();
    queue.enqueue_kind(key(0), thumbnail::ThumbnailKind::Pdf);
    assert_eq!(queue.begin_next(), Some(key(0)));
    queue.enqueue_kind(key(1), thumbnail::ThumbnailKind::Pdf);
    queue.enqueue_kind(key(2), thumbnail::ThumbnailKind::Image);
    assert_eq!(
        queue.begin_next_prioritized(|candidate| if candidate == &key(1) {
            Priority::Visible
        } else {
            Priority::Nearby
        }),
        Some(key(2))
    );
    assert!(
        queue
            .begin_next_prioritized(|_| Priority::Visible)
            .is_none()
    );
    queue.finish_key(&key(0));
    assert_eq!(
        queue.begin_next_prioritized(|_| Priority::Visible),
        Some(key(1))
    );
}

#[test]
fn ninety_percent_rounds_up_without_overflow_or_empty_success() {
    assert!(!reached_ninety(0, 0));
    assert!(!reached_ninety(16, 14));
    assert!(reached_ninety(16, 15));
    assert!(reached_ninety(10, 9));
    assert!(!reached_ninety(1, 0));
    assert!(reached_ninety(usize::MAX, usize::MAX));
}

#[test]
fn lookup_dispatch_rechecks_geometry_and_keeps_only_two_outstanding() {
    gtk_test(
        "ui::thumbnail::viewport::tests::lookup_dispatch_rechecks_geometry_and_keeps_only_two_outstanding",
        || {
            let fixture = Fixture::new();
            let (_near, near) = fixture.target(fixture.scroll.vadjustment().page_size() + 8.0);
            let (_visible, visible) = fixture.target(16.0);
            let (_far, far) = fixture.target(1200.0);
            for (index, target) in [(0, near.clone()), (1, visible.clone()), (2, far.clone())] {
                assert!(thumbnail::schedule_thumbnail(
                    key(index),
                    thumbnail::ThumbnailKind::Image,
                    target
                ));
            }
            assert_eq!(priority(&near), Priority::Nearby);
            assert_eq!(priority(&visible), Priority::Visible);
            assert_eq!(priority(&far), Priority::Deferred);
            assert_eq!(thumbnail::next_lookup_key(), Some(key(1)));
            thumbnail::LOOKUP_ACTIVE.with(|active| active.borrow_mut().extend([10001, 10002]));
            assert!(thumbnail::next_lookup_key().is_none());
            fixture.scroll.vadjustment().set_value(1200.0);
            wait_until(|| priority(&far) == Priority::Visible);
            thumbnail::LOOKUP_ACTIVE.with(|active| {
                active.borrow_mut().remove(&10001);
            });
            assert_eq!(thumbnail::next_lookup_key(), Some(key(2)));
            assert!(thumbnail::next_lookup_key().is_none());
        },
    );
}

#[test]
fn scroll_gate_is_per_viewport_and_executing_jobs_survive_reprioritization() {
    gtk_test(
        "ui::thumbnail::viewport::tests::scroll_gate_is_per_viewport_and_executing_jobs_survive_reprioritization",
        || {
            let first = Fixture::new();
            let second = Fixture::new();
            let (_image, target) = first.target(16.0);
            let (_other_image, other) = second.target(16.0);
            thumbnail::schedule_thumbnail(key(0), thumbnail::ThumbnailKind::Image, target.clone());
            thumbnail::schedule_thumbnail(key(1), thumbnail::ThumbnailKind::Image, other.clone());
            thumbnail::PENDING_THUMBNAILS.with(|pending| {
                pending
                    .borrow_mut()
                    .get_mut(&key(0))
                    .expect("active request")
                    .executing = true;
            });
            thumbnail::set_viewport_scrolling(&first.scroll, true);
            assert_eq!(priority(&target), Priority::Paused);
            assert_eq!(priority(&other), Priority::Visible);
            thumbnail::defer_pending(&key(0));
            thumbnail::PENDING_THUMBNAILS
                .with(|pending| assert!(!pending.borrow()[&key(0)].cancellation.is_cancelled()));
            assert_eq!(thumbnail::next_lookup_key(), Some(key(1)));
            thumbnail::set_viewport_scrolling(&first.scroll, false);
            assert_eq!(priority(&target), Priority::Visible);
        },
    );
}

#[test]
fn full_prefetch_queue_makes_room_for_visible_consumers() {
    gtk_test(
        "ui::thumbnail::viewport::tests::full_prefetch_queue_makes_room_for_visible_consumers",
        || {
            let fixture = Fixture::new();
            thumbnail::LOOKUP_ACTIVE.with(|active| active.borrow_mut().extend([10001, 10002]));
            let mut images = Vec::new();
            for index in 0..thumbnail::MAX_QUEUED_THUMBNAILS {
                let (image, target) =
                    fixture.target(fixture.scroll.vadjustment().page_size() + 8.0);
                thumbnail::schedule_thumbnail(key(index), thumbnail::ThumbnailKind::Image, target);
                images.push(image);
            }
            let (_image, visible) = fixture.target(16.0);
            thumbnail::mark_deferred(
                key(100),
                thumbnail::ThumbnailKind::Image,
                visible.image_id,
                visible.request,
            );
            thumbnail::retry_deferred_thumbnails();
            thumbnail::PENDING_THUMBNAILS.with(|pending| {
                assert_eq!(pending.borrow().len(), thumbnail::MAX_QUEUED_THUMBNAILS);
                assert!(pending.borrow().contains_key(&key(100)));
            });
            thumbnail::ACTIVE_REQUESTS.with(|requests| {
                assert_eq!(
                    requests
                        .borrow()
                        .values()
                        .filter(|active| active.deferred.is_some())
                        .count(),
                    1
                );
            });
            thumbnail::LOOKUP_ACTIVE.with(|active| active.borrow_mut().clear());
            assert_eq!(thumbnail::next_lookup_key(), Some(key(100)));
        },
    );
}
