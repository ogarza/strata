// SPDX-License-Identifier: GPL-3.0-or-later

use super::{PendingTarget, ThumbnailSlot};
use gtk::{gdk, glib, prelude::*};
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, time::Instant};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Priority {
    Visible,
    Nearby,
    Paused,
    Deferred,
}

pub(super) fn rect_priority(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    viewport_width: f32,
    viewport_height: f32,
) -> Priority {
    if ![x, y, width, height, viewport_width, viewport_height]
        .into_iter()
        .all(f32::is_finite)
        || width <= 0.0
        || height <= 0.0
        || viewport_width <= 0.0
        || viewport_height <= 0.0
    {
        return Priority::Deferred;
    }
    let intersects = |margin: f32| {
        x < viewport_width * (1.0 + margin)
            && x + width > -viewport_width * margin
            && y < viewport_height * (1.0 + margin)
            && y + height > -viewport_height * margin
    };
    if intersects(0.0) {
        Priority::Visible
    } else if intersects(0.25) {
        Priority::Nearby
    } else {
        Priority::Deferred
    }
}

pub(super) fn ancestors(image: &ThumbnailSlot) -> Vec<gtk::ScrolledWindow> {
    let mut result = Vec::new();
    let mut parent = image.parent();
    while let Some(widget) = parent {
        parent = widget.parent();
        if let Ok(viewport) = widget.downcast::<gtk::ScrolledWindow>() {
            result.push(viewport);
        }
    }
    result
}

pub(super) fn priority(target: &PendingTarget) -> Priority {
    if !super::request_is_live(target) {
        return Priority::Deferred;
    }
    target
        .image
        .upgrade()
        .map_or(Priority::Deferred, |image| widget_priority(&image, true))
}

fn widget_priority(image: &ThumbnailSlot, respect_scroll_gate: bool) -> Priority {
    if !image.is_mapped() || image.width() <= 0 || image.height() <= 0 {
        return Priority::Deferred;
    }
    let viewports = ancestors(image);
    let mut priority = Priority::Visible;
    for viewport in &viewports {
        if respect_scroll_gate && super::viewport_admission_paused(viewport) {
            return Priority::Paused;
        }
        let Some(bounds) = image.compute_bounds(viewport) else {
            return Priority::Deferred;
        };
        let width = (viewport.hadjustment().page_size() as f32).min(viewport.width() as f32);
        let height = (viewport.vadjustment().page_size() as f32).min(viewport.height() as f32);
        priority = priority.max(rect_priority(
            bounds.x(),
            bounds.y(),
            bounds.width(),
            bounds.height(),
            width,
            height,
        ));
    }
    if viewports.is_empty() {
        let Some(root) = image.root().and_downcast::<gtk::Window>() else {
            return Priority::Deferred;
        };
        let Some(bounds) = image.compute_bounds(&root) else {
            return Priority::Deferred;
        };
        priority = rect_priority(
            bounds.x(),
            bounds.y(),
            bounds.width(),
            bounds.height(),
            root.width() as f32,
            root.height() as f32,
        );
    }
    priority
}

struct Observation {
    image: glib::WeakRef<ThumbnailSlot>,
    path: PathBuf,
    active: bool,
}

thread_local! {
    static TARGETS: RefCell<HashMap<usize, Observation>> = RefCell::new(HashMap::new());
}

pub(super) fn track(image: &ThumbnailSlot, path: &std::path::Path) {
    let first = TARGETS.with(|targets| {
        let mut targets = targets.borrow_mut();
        let key = image.as_ptr() as usize;
        let first = targets
            .get(&key)
            .is_none_or(|target| target.image.upgrade().as_ref() != Some(image));
        targets.insert(
            key,
            Observation {
                image: image.downgrade(),
                path: path.to_owned(),
                active: true,
            },
        );
        if first && targets.len().is_multiple_of(64) {
            targets.retain(|_, target| target.image.upgrade().is_some());
        }
        first
    });
    if first {
        // Tick sources run only when mapped and are removed with their widget.
        // The low-priority pump runs after this frame's allocation, never in bind.
        let wake = |image: &ThumbnailSlot| {
            image.add_tick_callback(|_, _| {
                super::start_thumbnail_jobs();
                glib::ControlFlow::Break
            });
        };
        if image.is_mapped() {
            wake(image);
        }
        image.connect_map(wake);
    }
}

pub(super) fn untrack(image: &ThumbnailSlot) {
    TARGETS.with(|targets| {
        if let Some(target) = targets.borrow_mut().get_mut(&(image.as_ptr() as usize)) {
            target.active = false;
        }
    });
}

#[derive(Default)]
pub(super) struct PaintProgress {
    epoch: u64,
    pub(super) started: Option<Instant>,
    cohort: Option<Vec<Observation>>,
    pub(super) first: bool,
    pub(super) ninety: bool,
}

impl PaintProgress {
    pub(super) fn new(started: Instant) -> Self {
        Self {
            epoch: super::NEXT_REQUEST.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            started: Some(started),
            ..Self::default()
        }
    }

    pub(super) fn reset(&mut self, started: Instant) {
        *self = Self::new(started);
    }
}

struct FrameObserver {
    clock: gdk::FrameClock,
    handler: Option<glib::SignalHandlerId>,
}
impl Drop for FrameObserver {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.clock.disconnect(handler);
        }
    }
}

pub(super) fn observe(viewport: &gtk::ScrolledWindow) {
    let observer: Rc<RefCell<Option<FrameObserver>>> = Rc::new(RefCell::new(None));
    let attach = {
        let observer = observer.clone();
        move |viewport: &gtk::ScrolledWindow| {
            if !crate::metrics::thumbnail_viewport_metrics_enabled() || observer.borrow().is_some()
            {
                return;
            }
            let Some(clock) = viewport.frame_clock() else {
                return;
            };
            let weak = viewport.downgrade();
            let handler = clock.connect_after_paint(move |_| {
                if let Some(viewport) = weak.upgrade() {
                    sample_paint(&viewport, Instant::now());
                }
            });
            observer.replace(Some(FrameObserver {
                clock,
                handler: Some(handler),
            }));
        }
    };
    if viewport.is_mapped() {
        attach(viewport);
    }
    viewport.connect_map(attach);
    viewport.connect_unmap(move |_| {
        observer.borrow_mut().take();
    });
}

fn sample_paint(viewport: &gtk::ScrolledWindow, now: Instant) {
    let group = super::group_address(Some(viewport));
    let Some(needs_cohort) = super::SETTLE_VIEWS.with(|views| {
        views
            .borrow()
            .get(&group)
            .filter(|view| !view.scrolling && view.paint.started.is_some() && !view.paint.ninety)
            .map(|view| view.paint.cohort.is_none())
    }) else {
        return;
    };
    let candidates = if needs_cohort {
        TARGETS.with(|targets| {
            targets
                .borrow()
                .values()
                .filter(|target| target.active)
                .filter_map(|target| {
                    let image = target.image.upgrade()?;
                    (super::viewport_of(&image).as_ref() == Some(viewport)
                        && widget_priority(&image, false) == Priority::Visible)
                        .then(|| Observation {
                            image: target.image.clone(),
                            path: target.path.clone(),
                            active: true,
                        })
                })
                .collect::<Vec<_>>()
        })
    } else {
        Vec::new()
    };
    let Some((id, epoch, elapsed, total, ready, first, ninety)) =
        super::SETTLE_VIEWS.with(|views| {
            let mut views = views.borrow_mut();
            let view = views.get_mut(&group)?;
            let started = view.paint.started?;
            if candidates.is_empty() && view.paint.cohort.is_none() {
                return None;
            }
            let cohort = view.paint.cohort.get_or_insert(candidates);
            let total = cohort.len();
            let ready = cohort
                .iter()
                .filter(|target| {
                    target.image.upgrade().is_some_and(|image| {
                        widget_priority(&image, false) == Priority::Visible
                            && super::displayed_thumbnail_matches(&image, &target.path)
                    })
                })
                .count();
            let first = !view.paint.first && ready > 0;
            let ninety = !view.paint.ninety && reached_ninety(total, ready);
            view.paint.first |= first;
            view.paint.ninety |= ninety;
            Some((
                view.id,
                view.paint.epoch,
                now.saturating_duration_since(started),
                total,
                ready,
                first,
                ninety,
            ))
        })
    else {
        return;
    };
    if first {
        crate::metrics::record_thumbnail_viewport(id, epoch, "first", total, ready, elapsed);
    }
    if ninety {
        crate::metrics::record_thumbnail_viewport(
            id,
            epoch,
            "ninety_percent",
            total,
            ready,
            elapsed,
        );
    }
}

fn reached_ninety(total: usize, ready: usize) -> bool {
    total > 0 && ready >= total - total / 10
}

#[cfg(test)]
pub(super) fn clear() {
    TARGETS.with(|targets| targets.borrow_mut().clear());
}

#[cfg(test)]
mod tests;
