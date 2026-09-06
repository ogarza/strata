// SPDX-License-Identifier: GPL-3.0-or-later

use super::{ensure_rename_field, icons_card_extent, new_card, parts, rename_field, set_slot};
use crate::test_support::gtk_test;
use gtk::prelude::*;

#[test]
fn card_keeps_a_fixed_size_request() {
    gtk_test(
        "ui::icons_cell::tests::card_keeps_a_fixed_size_request",
        || {
            let card = new_card(64);
            let (width, height) = icons_card_extent(64);
            assert_eq!(card.width_request(), width);
            assert_eq!(card.height_request(), height);
            let (icon, _) = parts(&card).expect("icon and label");
            icon.set_pixel_size(512);
            icon.set_size_request(512, 256);
            set_slot(&card, 64);
            assert_eq!(card.width_request(), width);
            assert_eq!(card.height_request(), height);
        },
    );
}

#[test]
fn grid_view_maps_plain_cards() {
    gtk_test("ui::icons_cell::tests::grid_view_maps_plain_cards", || {
        let created = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let factory = gtk::SignalListItemFactory::new();
        let created_for_setup = created.clone();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            created_for_setup.set(created_for_setup.get() + 1);
            item.set_child(Some(&new_card(64)));
        });
        let model = gtk::StringList::new(&["a", "b", "c", "d", "e", "f"]);
        let view = gtk::GridView::new(Some(gtk::NoSelection::new(Some(model))), Some(factory));
        let window = gtk::Window::new();
        window.set_default_size(480, 320);
        window.set_child(Some(&view));
        window.present();
        let context = gtk::glib::MainContext::default();
        for _ in 0..32 {
            context.iteration(false);
        }
        assert!(
            created.get() >= 1,
            "GridView must instantiate at least one icons card"
        );
        window.close();
    });
}

#[test]
fn new_card_has_no_rename_entry_until_needed() {
    gtk_test(
        "ui::icons_cell::tests::new_card_has_no_rename_entry_until_needed",
        || {
            let card = new_card(64);
            let (width, height) = icons_card_extent(64);
            assert!(rename_field(&card).is_none());
            let field = ensure_rename_field(&card).expect("rename field");
            assert!(field.has_css_class("inline-rename"));
            assert!(!gtk::prelude::WidgetExt::is_visible(&field));
            assert_eq!(card.width_request(), width);
            assert_eq!(card.height_request(), height);
            assert!(rename_field(&card).is_some());
        },
    );
}
