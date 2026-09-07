// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use tracing_subscriber::prelude::*;

fn progress(group: usize) -> (usize, bool, bool) {
    thumbnail::SETTLE_VIEWS.with(|views| {
        let views = views.borrow();
        let paint = &views[&group].paint;
        (
            paint.cohort.as_ref().map_or(0, Vec::len),
            paint.first,
            paint.ninety,
        )
    })
}

#[test]
fn milestones_follow_paint_and_exclude_prefetch_and_wrong_file_textures() {
    gtk_test(
        "ui::thumbnail::viewport::tests::paint::milestones_follow_paint_and_exclude_prefetch_and_wrong_file_textures",
        || {
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_test_writer())
                .with(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("strata::metrics", tracing::Level::DEBUG),
                )
                .try_init()
                .expect("isolated tracing");
            let fixture = Fixture::new();
            let mut images = Vec::new();
            for index in 0..10 {
                let y = 16.0 + (index / 5) as f64 * 48.0;
                let (image, _) = fixture.target(y);
                fixture
                    .content
                    .move_(&image, 16.0 + (index % 5) as f64 * 40.0, y);
                track(&image, &key(index).path);
                images.push(image);
            }
            let (prefetch, _) = fixture.target(fixture.scroll.vadjustment().page_size() + 8.0);
            track(&prefetch, &key(10).path);
            let texture = gdk::MemoryTexture::new(
                1,
                1,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(vec![30, 80, 120, 255]),
                4,
            )
            .upcast::<gdk::Texture>();
            thumbnail::set_viewport_scrolling(&fixture.scroll, true);
            let group = thumbnail::group_address(Some(&fixture.scroll));
            for (index, image) in images.iter().enumerate().take(8) {
                thumbnail::apply_thumbnail(image, &texture, &key(index).path);
            }
            sample_paint(&fixture.scroll, Instant::now());
            assert_eq!(progress(group), (0, false, false));
            thumbnail::set_viewport_scrolling(&fixture.scroll, false);
            wait_until(|| progress(group).1);
            assert_eq!(progress(group), (10, true, false));
            thumbnail::apply_thumbnail(&images[8], &texture, &key(999).path);
            sample_paint(&fixture.scroll, Instant::now());
            assert_eq!(progress(group), (10, true, false));
            thumbnail::apply_thumbnail(&images[8], &texture, &key(8).path);
            assert!(!progress(group).2);
            wait_until(|| progress(group).2);
            assert_eq!(progress(group), (10, true, true));
            let epoch = thumbnail::SETTLE_VIEWS.with(|views| views.borrow()[&group].paint.epoch);
            thumbnail::set_viewport_scrolling(&fixture.scroll, true);
            assert_eq!(progress(group), (0, false, false));
            thumbnail::SETTLE_VIEWS
                .with(|views| assert_ne!(views.borrow()[&group].paint.epoch, epoch));
        },
    );
}
