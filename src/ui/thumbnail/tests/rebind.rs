// SPDX-License-Identifier: GPL-3.0-or-later

use super::super as thumbnail;
use super::*;

fn current_target(image: &thumbnail::ThumbnailSlot) -> PendingTarget {
    let image_id = image.as_ptr() as usize;
    ACTIVE_REQUESTS.with(|requests| {
        let requests = requests.borrow();
        let active = &requests[&image_id];
        PendingTarget {
            image_id,
            request: active.id,
            image: active.image.clone(),
        }
    })
}

#[test]
fn same_file_rebind_keeps_texture_through_async_revalidation() {
    gtk_test(
        "ui::thumbnail::tests::rebind::same_file_rebind_keeps_texture_through_async_revalidation",
        || {
            super::super::super::theme::ThemeManager::shared();
            let root = tempfile::tempdir().expect("fixture directory");
            let path = root.path().join("photo.png");
            std::fs::write(&path, SAMPLE_PNG).expect("source fixture");
            let texture = sample_texture();
            let key = resolve_source_key(&ThumbnailKey {
                path: path.clone(),
                revision: None,
                modified: None,
                file_size: None,
            });
            THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().insert(key, texture.clone(), 4));
            let image = thumbnail::ThumbnailSlot::new(64);
            let window = gtk::Window::builder().child(&image).build();
            window.present();
            wait_until(|| image.is_mapped());
            let entry = sample_entry(&path);
            bind_thumbnail(&image, &entry);
            wait_until(|| image.texture().is_some());
            for size in [64, 128, 32, 64] {
                cancel_thumbnail(image.as_ptr() as usize);
                thumbnail::show_deferred_thumbnail_or_icon(
                    &image,
                    &entry,
                    crate::assets::icons::PICTURES,
                    size,
                );
                assert_eq!(image.texture().as_ref(), Some(&texture));
                assert!(ACTIVE_REQUESTS.with(|requests| requests.borrow().is_empty()));
                assert!(!has_pending_thumbnail(&path));
                bind_thumbnail_at(&image, &entry, size);
                assert_eq!(image.texture().as_ref(), Some(&texture));
                assert_eq!(image.slot_size(), size);
                let target = current_target(&image);
                wait_until(|| {
                    assert_eq!(image.texture().as_ref(), Some(&texture));
                    !thumbnail::request_is_live(&target)
                });
            }
            window.destroy();
            clear_thumbnail_runtime();
        },
    );
}

#[test]
fn failure_clears_retained_texture_but_stale_failure_cannot_clear_a_recycled_row() {
    gtk_test(
        "ui::thumbnail::tests::rebind::failure_clears_retained_texture_but_stale_failure_cannot_clear_a_recycled_row",
        || {
            super::super::super::theme::ThemeManager::shared();
            let old_path = Path::new("/fixture/old.png");
            let new_path = Path::new("/fixture/new.png");
            let image = thumbnail::ThumbnailSlot::new(64);
            let texture = sample_texture();
            thumbnail::apply_thumbnail(&image, &texture, old_path);
            bind_thumbnail(&image, &sample_entry(old_path));
            let target = current_target(&image);
            assert!(image.texture().is_some());
            finish_thumbnail_targets(vec![target], None, old_path);
            assert!(image.texture().is_none());
            assert!(!thumbnail::displayed_thumbnail_matches(&image, old_path));

            thumbnail::apply_thumbnail(&image, &texture, old_path);
            bind_thumbnail(&image, &sample_entry(old_path));
            let stale_target = current_target(&image);
            thumbnail::show_deferred_thumbnail_or_icon(
                &image,
                &sample_entry(new_path),
                crate::assets::icons::PICTURES,
                64,
            );
            assert!(image.texture().is_none());
            bind_thumbnail(&image, &sample_entry(new_path));
            assert!(
                image.texture().is_none(),
                "a different file must not inherit the previous texture"
            );
            thumbnail::apply_thumbnail(&image, &texture, new_path);
            finish_thumbnail_targets(vec![stale_target], None, old_path);
            assert_eq!(image.texture().as_ref(), Some(&texture));
            clear_thumbnail_runtime();
        },
    );
}

#[test]
fn custom_icons_and_non_thumbnail_entries_replace_retained_textures_immediately() {
    gtk_test(
        "ui::thumbnail::tests::rebind::custom_icons_and_non_thumbnail_entries_replace_retained_textures_immediately",
        || {
            let theme = super::super::super::theme::ThemeManager::shared();
            let path = Path::new("/fixture/photo.png");
            let image = thumbnail::ThumbnailSlot::new(64);
            let texture = sample_texture();
            thumbnail::apply_thumbnail(&image, &texture, path);
            theme.set_custom_icon(path, Some(crate::assets::icons::CUSTOMIZATION_CHOICES[0].0));
            bind_thumbnail(&image, &sample_entry(path));
            assert!(image.texture().is_none());
            theme.set_custom_icon(path, None);
            thumbnail::apply_thumbnail(&image, &texture, path);
            let mut entry = sample_entry(path);
            entry.kind = EntryKind::Directory;
            bind_thumbnail(&image, &entry);
            assert!(image.texture().is_none());
            clear_thumbnail_runtime();
        },
    );
}
