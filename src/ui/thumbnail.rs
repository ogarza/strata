// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use gtk::{gdk, glib, prelude::*};

use crate::{
    model::{FileEntry, MetadataValue},
    sandbox::{Cancellation, ParseOperation},
};

#[path = "thumbnail_decode.rs"]
mod decode;
mod slot;
pub(crate) use slot::ThumbnailSlot;

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Viewport bounding keeps the admitted work fixed.
const MAX_THUMBNAIL_WORKERS: usize = 4;
const MAX_QUEUED_THUMBNAILS: usize = 64;
const FAILED_THUMBNAIL_TTL: Duration = Duration::from_secs(30);
/// Scroll callbacks re-arm this delay so leftover parks are not fired inside layout.
const THUMBNAIL_SETTLE_DELAY: Duration = Duration::from_millis(120);
/// Overlong parks still admit sandbox jobs on a long fling.
const MAX_SETTLE_WAIT: Duration = Duration::from_millis(400);
#[cfg(test)]
const VIEWPORT_OVERSCAN: f32 = 0.25;
const MAX_HEAVY_THUMBNAILS: usize = 1;
const MAX_RASTER_BURST: usize = 3;

thread_local! {
    static ACTIVE_REQUESTS: RefCell<HashMap<usize, ActiveRequest>> =
        RefCell::new(HashMap::new());
    static PENDING_THUMBNAILS: RefCell<HashMap<ThumbnailKey, PendingThumbnail>> =
        RefCell::new(HashMap::new());
    static THUMBNAIL_QUEUE: RefCell<ThumbnailQueue> = RefCell::new(ThumbnailQueue::default());
    static LOOKUP_QUEUE: RefCell<VecDeque<ThumbnailKey>> = const { RefCell::new(VecDeque::new()) };
    static THUMBNAIL_CACHE: RefCell<ThumbnailCache> = RefCell::new(ThumbnailCache::default());
    /// Per-viewport settle groups (key zero is the fallback); one view's fling never postpones another's.
    static SETTLE_VIEWS: RefCell<HashMap<usize, ViewSettle>> = RefCell::new(HashMap::new());
    static TRACKED_CUSTOMIZED_ICONS: RefCell<Vec<TrackedCustomizedIcon>> =
        const { RefCell::new(Vec::new()) };
    static TRACKED_THUMBNAILS: RefCell<Vec<TrackedThumbnail>> = const { RefCell::new(Vec::new()) };
    static REFRESHING_CUSTOMIZED_ICONS: Cell<bool> = const { Cell::new(false) };
}

struct TrackedThumbnail {
    image: glib::WeakRef<ThumbnailSlot>,
    path: PathBuf,
}

struct TrackedCustomizedIcon {
    image: glib::WeakRef<ThumbnailSlot>,
    path: PathBuf,
    icon: String,
    customized: bool,
}

struct ActiveRequest {
    id: u64,
    image: glib::WeakRef<ThumbnailSlot>,
    deferred: Option<DeferredThumbnail>,
}

#[derive(Clone)]
struct DeferredThumbnail {
    key: ThumbnailKey,
    kind: ThumbnailKind,
}

#[derive(Clone)]
struct PendingTarget {
    image_id: usize,
    request: u64,
    image: glib::WeakRef<ThumbnailSlot>,
}
struct SettledPark {
    key: ThumbnailKey,
    kind: ThumbnailKind,
    target: PendingTarget,
}

struct ViewSettle {
    viewport: glib::WeakRef<gtk::ScrolledWindow>,
    pending: Vec<SettledPark>,
    timer: Option<glib::SourceId>,
    first_park: Option<Instant>,
    hooked: bool,
}

#[derive(Clone)]
struct PersistJob {
    path: PathBuf,
    mtime: i64,
    png: Vec<u8>,
}

/// Bounded: slow disk never delays display; over capacity the oldest entry drops.
const MAX_PERSIST_QUEUE: usize = 32;

struct PersistQueue {
    queue: VecDeque<PersistJob>,
}

impl PersistQueue {
    const fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    fn push(&mut self, job: PersistJob) {
        if self.queue.len() >= MAX_PERSIST_QUEUE {
            self.queue.pop_front();
        }
        self.queue.push_back(job);
    }

    fn pop_front(&mut self) -> Option<PersistJob> {
        self.queue.pop_front()
    }

    fn push_front(&mut self, job: PersistJob) {
        self.queue.push_front(job);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.queue.len()
    }
}

// Process-wide: the persistence pump runs on a worker thread, so the queue
// cannot be a main-thread local like the settle state.
static PERSIST_QUEUE: std::sync::Mutex<PersistQueue> = std::sync::Mutex::new(PersistQueue::new());
static PERSIST_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn enqueue_persist(path: PathBuf, mtime: i64, png: Vec<u8>) {
    PERSIST_QUEUE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(PersistJob { path, mtime, png });
    pump_persist_queue();
}

fn pump_persist_queue() {
    use std::sync::atomic::Ordering;
    if PERSIST_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(job) = PERSIST_QUEUE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pop_front()
    else {
        PERSIST_RUNNING.store(false, Ordering::SeqCst);
        return;
    };
    let work_job = job.clone();
    if decode::submit_owned(
        move || {
            let started = std::time::Instant::now();
            super::thumbnail_cache::store(&work_job.path, work_job.mtime, &work_job.png);
            crate::metrics::record_thumbnail_stage(
                crate::metrics::ThumbnailStage::Persist,
                started.elapsed(),
            );
        },
        |_| {
            PERSIST_RUNNING.store(false, Ordering::SeqCst);
            pump_persist_queue();
        },
    )
    .is_err()
    {
        PERSIST_QUEUE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_front(job);
        PERSIST_RUNNING.store(false, Ordering::SeqCst);
        glib::timeout_add_local_once(Duration::from_millis(10), pump_persist_queue);
    }
}

struct PendingThumbnail {
    id: u64,
    kind: ThumbnailKind,
    cancellation: Cancellation,
    targets: Vec<PendingTarget>,
}

#[derive(Clone)]
struct ThumbnailJob {
    id: u64,
    key: ThumbnailKey,
    kind: ThumbnailKind,
    cancellation: Cancellation,
}

struct LookupResult {
    key: ThumbnailKey,
    cached: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ThumbnailKey {
    path: PathBuf,
    modified: Option<i64>,
    file_size: Option<u64>,
}

#[derive(Default)]
struct ThumbnailCache {
    entries: HashMap<ThumbnailKey, CachedThumbnail>,
    recent: VecDeque<ThumbnailKey>,
    byte_count: usize,
}

#[derive(Clone)]
struct ReadyThumbnail {
    texture: gdk::Texture,
    byte_len: usize,
}

#[derive(Clone)]
enum CachedThumbnail {
    Ready(ReadyThumbnail),
    Failed(Instant),
}

enum CacheHit {
    Ready(ReadyThumbnail),
    Failed,
}

impl ThumbnailCache {
    fn get(&mut self, key: &ThumbnailKey) -> Option<CacheHit> {
        let entry = self.entries.get(key)?.clone();
        if matches!(entry, CachedThumbnail::Failed(expires) if expires <= Instant::now()) {
            self.remove(key);
            return None;
        }
        self.recent.retain(|candidate| candidate != key);
        self.recent.push_back(key.clone());
        Some(match entry {
            CachedThumbnail::Ready(texture) => CacheHit::Ready(texture),
            CachedThumbnail::Failed(_) => CacheHit::Failed,
        })
    }

    fn insert(&mut self, key: ThumbnailKey, texture: gdk::Texture, byte_len: usize) {
        self.insert_entry(
            key,
            CachedThumbnail::Ready(ReadyThumbnail { texture, byte_len }),
        );
    }

    fn insert_failure(&mut self, key: ThumbnailKey) {
        self.insert_entry(
            key,
            CachedThumbnail::Failed(Instant::now() + FAILED_THUMBNAIL_TTL),
        );
    }

    fn insert_entry(&mut self, key: ThumbnailKey, entry: CachedThumbnail) {
        self.remove(&key);
        self.byte_count = self.byte_count.saturating_add(entry.byte_len());
        self.recent.push_back(key.clone());
        self.entries.insert(key, entry);
        while self.entries.len() > MAX_CACHE_ENTRIES || self.byte_count > MAX_CACHE_BYTES {
            let Some(oldest) = self.recent.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.byte_count = self.byte_count.saturating_sub(removed.byte_len());
            }
        }
    }

    fn remove(&mut self, key: &ThumbnailKey) {
        if let Some(removed) = self.entries.remove(key) {
            self.byte_count = self.byte_count.saturating_sub(removed.byte_len());
        }
        self.recent.retain(|candidate| candidate != key);
    }
}

impl CachedThumbnail {
    fn byte_len(&self) -> usize {
        match self {
            Self::Ready(ready) => ready.byte_len,
            Self::Failed(_) => 0,
        }
    }
}

#[derive(Default)]
struct ThumbnailQueue {
    running: usize,
    heavy_running: usize,
    raster_burst: usize,
    queued: VecDeque<(ThumbnailKey, ThumbnailKind)>,
    active_kinds: HashMap<ThumbnailKey, ThumbnailKind>,
}

impl ThumbnailQueue {
    #[cfg(test)]
    fn enqueue(&mut self, key: ThumbnailKey) -> bool {
        self.enqueue_kind(key, ThumbnailKind::Image)
    }

    fn enqueue_kind(&mut self, key: ThumbnailKey, kind: ThumbnailKind) -> bool {
        if self.queued.len() >= MAX_QUEUED_THUMBNAILS {
            return false;
        }
        self.queued.push_back((key, kind));
        true
    }

    fn begin_next(&mut self) -> Option<ThumbnailKey> {
        if self.running >= MAX_THUMBNAIL_WORKERS {
            return None;
        }
        let heavy = self.queued.iter().position(|(_, kind)| is_heavy(*kind));
        let raster = self.queued.iter().position(|(_, kind)| !is_heavy(*kind));
        let index = if let Some(heavy) = heavy
            .filter(|_| self.heavy_running < MAX_HEAVY_THUMBNAILS)
            .filter(|_| raster.is_none() || self.raster_burst >= MAX_RASTER_BURST)
        {
            heavy
        } else {
            raster.or(heavy)?
        };
        let (key, kind) = self.queued.remove(index)?;
        self.running += 1;
        if is_heavy(kind) {
            self.heavy_running += 1;
            self.raster_burst = 0;
        } else {
            self.raster_burst += 1;
        }
        self.active_kinds.insert(key.clone(), kind);
        Some(key)
    }

    fn push_front_kind(&mut self, key: ThumbnailKey, kind: ThumbnailKind) {
        self.queued.push_front((key, kind));
    }

    #[cfg(test)]
    fn finish(&mut self) {
        self.running = self.running.saturating_sub(1);
    }

    fn finish_key(&mut self, key: &ThumbnailKey) {
        if let Some(kind) = self.active_kinds.remove(key)
            && is_heavy(kind)
        {
            self.heavy_running = self.heavy_running.saturating_sub(1);
        }
        self.running = self.running.saturating_sub(1);
    }

    fn cancel(&mut self, key: &ThumbnailKey) {
        self.queued.retain(|(queued, _)| queued != key);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThumbnailKind {
    Image,
    RawImage,
    Pdf,
    Video,
}

pub(super) fn set_thumbnail_or_icon(
    image: &ThumbnailSlot,
    entry: &FileEntry,
    fallback_icon: &str,
    icon_size: i32,
    thumbnail_size: i32,
) {
    let Some(path) = entry.local_thumbnail_path() else {
        show_fallback_icon(image, fallback_icon, icon_size);
        return;
    };
    set_thumbnail_for_path(ThumbnailRequest {
        image,
        path,
        kind: if entry.is_directory() {
            None
        } else {
            thumbnail_kind(Path::new(&entry.display_name))
        },
        modified: known_metadata(&entry.modified_unix_seconds),
        file_size: known_metadata(&entry.size),
        fallback_icon,
        icon_size,
        thumbnail_size,
    });
}

pub(super) fn set_thumbnail_or_icon_for_path(
    image: &ThumbnailSlot,
    path: &Path,
    fallback_icon: &str,
    icon_size: i32,
    thumbnail_size: i32,
) {
    set_thumbnail_for_path(ThumbnailRequest {
        image,
        path,
        kind: thumbnail_kind(path),
        modified: None,
        file_size: None,
        fallback_icon,
        icon_size,
        thumbnail_size,
    });
}

/// Bundled to stay under the argument-count lint.
struct ThumbnailRequest<'a> {
    image: &'a ThumbnailSlot,
    path: &'a Path,
    kind: Option<ThumbnailKind>,
    modified: Option<i64>,
    file_size: Option<u64>,
    fallback_icon: &'a str,
    icon_size: i32,
    thumbnail_size: i32,
}

fn set_thumbnail_for_path(request: ThumbnailRequest<'_>) {
    let has_custom_icon = super::theme::ThemeManager::shared()
        .custom_icon(request.path)
        .is_some();
    if has_custom_icon {
        set_fallback_icon(
            request.image,
            Some(request.path),
            request.fallback_icon,
            request.icon_size,
        );
        return;
    }
    let path = request.path.to_path_buf();
    let thumbnail_size = request.thumbnail_size.clamp(16, 256);
    let Some(kind) = request.kind else {
        set_fallback_icon(
            request.image,
            Some(request.path),
            request.fallback_icon,
            request.icon_size,
        );
        return;
    };
    if displayed_thumbnail_matches(request.image, request.path) {
        return;
    }
    let key = ThumbnailKey {
        path: path.clone(),
        modified: request.modified,
        file_size: request.file_size,
    };
    match THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().get(&key)) {
        Some(CacheHit::Ready(ready)) => {
            let (image_id, _) = prepare_thumbnail_target(request.image, thumbnail_size);
            apply_thumbnail(request.image, &ready.texture, &path);
            ACTIVE_REQUESTS.with(|requests| {
                requests.borrow_mut().remove(&image_id);
            });
            return;
        }
        Some(CacheHit::Failed) => {
            set_fallback_icon(
                request.image,
                Some(request.path),
                request.fallback_icon,
                request.icon_size,
            );
            return;
        }
        None => {}
    }
    let (image_id, request_id) = set_fallback_icon(
        request.image,
        Some(request.path),
        request.fallback_icon,
        request.icon_size,
    );
    request.image.set_slot(thumbnail_size);
    let target = register_active_request(request.image, image_id, request_id);
    // Walking ancestors or hooking the viewport during bind can corrupt layout.
    glib::idle_add_local_once(move || {
        if request_is_live(&target) {
            park_thumbnail(key, kind, target);
        }
    });
}

fn viewport_of(image: &impl IsA<gtk::Widget>) -> Option<gtk::ScrolledWindow> {
    let mut ancestor = image.parent();
    while let Some(widget) = ancestor {
        ancestor = widget.parent();
        if let Ok(viewport) = widget.downcast::<gtk::ScrolledWindow>() {
            return Some(viewport);
        }
    }
    None
}

#[cfg(test)]
fn rect_eligible(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    viewport_width: f32,
    viewport_height: f32,
) -> bool {
    let overscan = VIEWPORT_OVERSCAN;
    width > 0.0
        && height > 0.0
        && viewport_width > 0.0
        && viewport_height > 0.0
        && x < viewport_width * (1.0 + overscan)
        && x + width > -viewport_width * overscan
        && y < viewport_height * (1.0 + overscan)
        && y + height > -viewport_height * overscan
}

fn group_address(viewport: Option<&gtk::ScrolledWindow>) -> usize {
    viewport.map_or(0, |viewport| viewport.as_ptr() as usize)
}

fn park_thumbnail(key: ThumbnailKey, kind: ThumbnailKind, target: PendingTarget) {
    crate::metrics::mark_thumbnail_requested();
    let viewport = target.image.upgrade().and_then(|image| viewport_of(&image));
    let group = group_address(viewport.as_ref());
    let viewport_ref = glib::WeakRef::new();
    if let Some(viewport) = &viewport {
        viewport_ref.set(Some(viewport));
    }
    SETTLE_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let settle = views.entry(group).or_insert_with(|| ViewSettle {
            viewport: viewport_ref.clone(),
            pending: Vec::new(),
            timer: None,
            first_park: None,
            hooked: false,
        });
        // A dead viewport's address may be recycled: reset the group instead of joining its stale hooks and pending requests.
        if group != 0 && settle.viewport.upgrade().is_none() {
            if let Some(timer) = settle.timer.take() {
                timer.remove();
            }
            *settle = ViewSettle {
                viewport: viewport_ref.clone(),
                pending: Vec::new(),
                timer: None,
                first_park: None,
                hooked: false,
            };
        }
        settle.pending.push(SettledPark { key, kind, target });
        if settle.first_park.is_none() {
            settle.first_park = Some(Instant::now());
        }
    });
    if let Some(viewport) = viewport {
        hook_viewport(group, &viewport);
    }
    fire_view_group(group);
}

#[cfg(test)]
fn schedule_or_defer(key: ThumbnailKey, kind: ThumbnailKind, target: PendingTarget) {
    park_thumbnail(key, kind, target);
}
fn mark_deferred(key: ThumbnailKey, kind: ThumbnailKind, image_id: usize, request: u64) {
    ACTIVE_REQUESTS.with(|requests| {
        if let Some(active) = requests
            .borrow_mut()
            .get_mut(&image_id)
            .filter(|active| active.id == request)
        {
            active.deferred = Some(DeferredThumbnail { key, kind });
        }
    });
}

/// Fires happen only on the main loop: firing inside binds or adjustment callbacks can walk the
/// widget tree while GTK is mutating it.
fn request_group_fire(group: usize) {
    SETTLE_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let Some(settle) = views.get_mut(&group) else {
            return;
        };
        // No pending rows, no timer: re-arming into an empty queue turns every thumbnail application (which relayouts) into another fire.
        if settle.pending.is_empty() {
            return;
        }
        let overdue = settle
            .first_park
            .is_some_and(|first| first.elapsed() >= MAX_SETTLE_WAIT);
        if let Some(timer) = settle.timer.take() {
            timer.remove();
        }
        let delay = if overdue {
            Duration::ZERO
        } else {
            THUMBNAIL_SETTLE_DELAY
        };
        settle.timer = Some(glib::timeout_add_local_once(delay, move || {
            fire_view_group(group);
        }));
    });
}
fn hook_viewport(group: usize, viewport: &gtk::ScrolledWindow) {
    let hooked = SETTLE_VIEWS.with(|views| {
        views
            .borrow_mut()
            .get_mut(&group)
            .map(|settle| std::mem::replace(&mut settle.hooked, true))
            .unwrap_or(true)
    });
    if hooked {
        return;
    }
    for adjustment in [viewport.vadjustment(), viewport.hadjustment()] {
        adjustment.connect_value_changed(move |_| request_group_fire(group));
        adjustment.connect_changed(move |_| request_group_fire(group));
    }
}

fn fire_view_group(group: usize) {
    let (viewport, drained) = SETTLE_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let Some(settle) = views.get_mut(&group) else {
            return (None, Vec::new());
        };
        if let Some(timer) = settle.timer.take() {
            timer.remove();
        }
        settle.first_park = None;
        let viewport = settle.viewport.upgrade();
        if group != 0 && viewport.is_none() {
            views.remove(&group);
            return (None, Vec::new());
        }
        let drained = std::mem::take(&mut settle.pending);
        (viewport, drained)
    });
    fire_parks(drained, viewport.as_ref());
}

#[cfg(test)]
fn fire_settled_thumbnails() {
    fire_view_group(0);
}

fn request_is_live(target: &PendingTarget) -> bool {
    ACTIVE_REQUESTS.with(|requests| {
        requests
            .borrow()
            .get(&target.image_id)
            .is_some_and(|active| active.id == target.request)
    })
}

fn register_active_request(
    image: &ThumbnailSlot,
    image_id: usize,
    request_id: u64,
) -> PendingTarget {
    let weak_image = glib::WeakRef::new();
    weak_image.set(Some(image));
    ACTIVE_REQUESTS.with(|requests| {
        requests.borrow_mut().insert(
            image_id,
            ActiveRequest {
                id: request_id,
                image: weak_image.clone(),
                deferred: None,
            },
        );
    });
    PendingTarget {
        image_id,
        request: request_id,
        image: weak_image,
    }
}

fn apply_live_thumbnail(target: PendingTarget, texture: gdk::Texture, path: PathBuf) {
    if !request_is_live(&target) {
        crate::metrics::mark_thumbnail_stale();
        return;
    }
    let Some(image) = target.image.upgrade() else {
        crate::metrics::mark_thumbnail_stale();
        ACTIVE_REQUESTS.with(|requests| {
            requests.borrow_mut().remove(&target.image_id);
        });
        return;
    };
    ACTIVE_REQUESTS.with(|requests| {
        requests.borrow_mut().remove(&target.image_id);
    });
    apply_thumbnail(&image, &texture, &path);
    crate::metrics::mark_thumbnail_applied();
}

fn fire_parks(drained: Vec<SettledPark>, _viewport: Option<&gtk::ScrolledWindow>) {
    let mut eligible = 0;
    let mut started = false;
    for park in drained {
        if !request_is_live(&park.target) {
            continue;
        }
        eligible += 1;
        if let Some(hit) = THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().get(&park.key)) {
            match hit {
                CacheHit::Ready(ready) => {
                    apply_live_thumbnail(park.target, ready.texture, park.key.path);
                }
                CacheHit::Failed => {}
            }
            continue;
        }
        let image_id = park.target.image_id;
        let request = park.target.request;
        if schedule_thumbnail(park.key.clone(), park.kind, park.target) {
            started = true;
        } else {
            mark_deferred(park.key, park.kind, image_id, request);
        }
    }
    if started {
        start_thumbnail_jobs();
    }
    crate::metrics::mark_thumbnail_eligible(eligible);
}

fn schedule_thumbnail(key: ThumbnailKey, kind: ThumbnailKind, target: PendingTarget) -> bool {
    PENDING_THUMBNAILS.with(|pending| {
        let mut pending = pending.borrow_mut();
        if let Some(pending) = pending.get_mut(&key) {
            pending.targets.push(target);
            true
        } else if let Some(pending) = pending.iter_mut().find_map(|(existing_key, pending)| {
            (existing_key.path == key.path
                && (existing_key.modified.is_none()
                    || key.modified.is_none()
                    || existing_key.modified == key.modified))
                .then_some(pending)
        }) {
            pending.targets.push(target);
            true
        } else {
            let queued = pending.len() < MAX_QUEUED_THUMBNAILS;
            if queued {
                LOOKUP_QUEUE.with(|queue| queue.borrow_mut().push_back(key.clone()));
                pending.insert(
                    key.clone(),
                    PendingThumbnail {
                        id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
                        kind,
                        cancellation: Cancellation::default(),
                        targets: vec![target],
                    },
                );
            }
            queued
        }
    })
}

fn start_thumbnail_jobs() {
    while let Some(key) = LOOKUP_QUEUE.with(|queue| queue.borrow_mut().pop_front()) {
        let retry_key = key.clone();
        let Some(job) = PENDING_THUMBNAILS.with(|pending| {
            pending.borrow().get(&key).map(|pending| ThumbnailJob {
                id: pending.id,
                key,
                kind: pending.kind,
                cancellation: pending.cancellation.clone(),
            })
        }) else {
            continue;
        };
        crate::metrics::mark_thumbnail_started();
        let lookup_job = job.clone();
        if decode::submit_owned(
            move || {
                let started = Instant::now();
                let key = resolve_source_key(&lookup_job.key);
                let cached = key
                    .modified
                    .and_then(|mtime| super::thumbnail_cache::lookup(&key.path, mtime));
                crate::metrics::record_thumbnail_stage(
                    crate::metrics::ThumbnailStage::Lookup,
                    started.elapsed(),
                );
                LookupResult { key, cached }
            },
            move |result| finish_thumbnail_lookup(job, result),
        )
        .is_err()
        {
            LOOKUP_QUEUE.with(|queue| queue.borrow_mut().push_front(retry_key));
            break;
        }
    }
    start_render_jobs();
}

fn start_render_jobs() {
    while let Some(key) = THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().begin_next()) {
        let Some(job) = PENDING_THUMBNAILS.with(|pending| {
            pending.borrow().get(&key).map(|pending| ThumbnailJob {
                id: pending.id,
                key: key.clone(),
                kind: pending.kind,
                cancellation: pending.cancellation.clone(),
            })
        }) else {
            THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().finish_key(&key));
            continue;
        };
        crate::metrics::mark_thumbnail_started();
        let render_kind = job.kind;
        let render_job = job.clone();
        if decode::submit_owned(
            move || {
                let started = Instant::now();
                let result = render_thumbnail(
                    &render_job.key.path,
                    render_job.kind,
                    super::thumbnail_cache::CANONICAL_MAX_EDGE,
                    &render_job.cancellation,
                );
                crate::metrics::record_thumbnail_stage(
                    crate::metrics::ThumbnailStage::Render,
                    started.elapsed(),
                );
                result
            },
            move |result| finish_thumbnail_render(job, result),
        )
        .is_err()
        {
            THUMBNAIL_QUEUE
                .with(|queue| queue.borrow_mut().push_front_kind(key.clone(), render_kind));
            THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().finish_key(&key));
            break;
        }
    }
}

fn resolve_source_key(key: &ThumbnailKey) -> ThumbnailKey {
    let Ok(metadata) = std::fs::metadata(&key.path) else {
        return key.clone();
    };
    if !metadata.is_file() {
        return key.clone();
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_secs()).ok());
    ThumbnailKey {
        path: key.path.clone(),
        modified: key.modified.or(modified),
        file_size: key.file_size.or(Some(metadata.len())),
    }
}

fn rekey_pending_thumbnail(job: &ThumbnailJob, key: ThumbnailKey) -> Option<ThumbnailJob> {
    PENDING_THUMBNAILS.with(|pending| {
        let mut pending = pending.borrow_mut();
        let current = pending.get(&job.key)?;
        if current.id != job.id {
            return None;
        }
        if key == job.key {
            return Some(job.clone());
        }
        let current = pending.remove(&job.key)?;
        if let Some(existing) = pending.get_mut(&key) {
            existing.targets.extend(current.targets);
            return Some(ThumbnailJob {
                id: existing.id,
                key,
                kind: existing.kind,
                cancellation: existing.cancellation.clone(),
            });
        }
        let id = current.id;
        pending.insert(key.clone(), current);
        Some(ThumbnailJob {
            id,
            key,
            kind: job.kind,
            cancellation: job.cancellation.clone(),
        })
    })
}

fn finish_thumbnail_lookup(mut job: ThumbnailJob, result: LookupResult) {
    if let Some(resolved) = rekey_pending_thumbnail(&job, result.key) {
        job = resolved;
    } else {
        return;
    }
    let cached = result.cached;
    if !pending_thumbnail_matches(&job.key, job.id) {
        return;
    }
    if let Some(png) = cached {
        crate::metrics::mark_thumbnail_lookup_hit();
        let completion_png = png.clone();
        let completion_job = job.clone();
        if decode::submit(png, move |decoded| {
            finish_thumbnail_decode(completion_job, completion_png, false, decoded);
        })
        .is_err()
        {
            finish_thumbnail_failure(job, false);
        }
        return;
    }
    crate::metrics::mark_thumbnail_lookup_miss();
    if THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().enqueue_kind(job.key.clone(), job.kind)) {
        start_render_jobs();
    } else {
        finish_thumbnail_failure(job, false);
    }
}

fn finish_thumbnail_render(job: ThumbnailJob, result: Result<Vec<u8>, String>) {
    match result {
        Ok(png) => {
            let completion_png = png.clone();
            let completion_job = job.clone();
            if decode::submit(png, move |decoded| {
                finish_thumbnail_decode(completion_job, completion_png, true, decoded);
            })
            .is_err()
            {
                finish_thumbnail_failure(job, true);
            }
        }
        Err(_) => finish_thumbnail_failure(job, true),
    }
}

fn finish_thumbnail_decode(
    job: ThumbnailJob,
    png: Vec<u8>,
    rendered: bool,
    decoded: Result<decode::DecodedTexture, String>,
) {
    match decoded {
        Ok(decoded) => {
            let targets = take_pending_targets(&job.key, job.id);
            if rendered {
                finish_thumbnail_slot(&job.key);
            }
            if let Some(targets) = targets {
                crate::metrics::mark_thumbnail_completed();
                THUMBNAIL_CACHE.with(|cache| {
                    cache.borrow_mut().insert(
                        job.key.clone(),
                        decoded.texture.clone(),
                        decoded.byte_len,
                    );
                });
                finish_thumbnail_targets(targets, Some(&decoded.texture), &job.key.path);
                if rendered && let Some(mtime) = job.key.modified {
                    enqueue_persist(job.key.path.clone(), mtime, png);
                }
            }
        }
        Err(_)
            if should_promote_invalid_cache(
                rendered,
                pending_thumbnail_matches(&job.key, job.id),
            ) =>
        {
            finish_thumbnail_render_again(job);
            return;
        }
        Err(_) => finish_thumbnail_failure(job, rendered),
    }
    settle_thumbnail_pipeline();
}

fn finish_thumbnail_render_again(job: ThumbnailJob) {
    if THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().enqueue_kind(job.key.clone(), job.kind)) {
        start_render_jobs();
    } else {
        finish_thumbnail_failure(job, false);
    }
}

fn should_promote_invalid_cache(rendered: bool, pending: bool) -> bool {
    !rendered && pending
}

fn pending_thumbnail_matches(key: &ThumbnailKey, job_id: u64) -> bool {
    PENDING_THUMBNAILS.with(|pending| {
        pending
            .borrow()
            .get(key)
            .is_some_and(|pending| pending.id == job_id)
    })
}

fn finish_thumbnail_failure(job: ThumbnailJob, render_admitted: bool) {
    let targets = take_pending_targets(&job.key, job.id);
    if render_admitted {
        finish_thumbnail_slot(&job.key);
    }
    if let Some(targets) = targets {
        crate::metrics::mark_thumbnail_cancelled();
        THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().insert_failure(job.key.clone()));
        finish_thumbnail_targets(targets, None, &job.key.path);
    }
    settle_thumbnail_pipeline();
}

fn finish_thumbnail_slot(key: &ThumbnailKey) {
    THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().finish_key(key));
}

fn settle_thumbnail_pipeline() {
    start_thumbnail_jobs();
    retry_deferred_thumbnails();
    let counts = crate::metrics::thumbnail_counts();
    tracing::debug!(?counts, "thumbnail pipeline settled");
}

fn retry_deferred_thumbnails() {
    let mut promoted = false;
    loop {
        // ponytail: deferred work is bounded by live GTK image widgets; add an explicit cap if a
        // future non-virtualized producer can create an unbounded number of them.
        let deferred = ACTIVE_REQUESTS.with(|requests| {
            let mut requests = requests.borrow_mut();
            requests.retain(|_, active| active.image.upgrade().is_some());
            requests
                .iter()
                .filter_map(|(image_id, active)| {
                    active.deferred.as_ref().map(|deferred| {
                        (*image_id, active.id, active.image.clone(), deferred.clone())
                    })
                })
                .min_by_key(|(_, request, _, _)| *request)
        });
        let Some((image_id, request, image, deferred)) = deferred else {
            break;
        };
        if !retry_deferred_thumbnail(image_id, request, image, deferred) {
            break;
        }
        promoted = true;
    }
    if promoted {
        start_thumbnail_jobs();
    }
}

fn retry_deferred_thumbnail(
    image_id: usize,
    request: u64,
    image: glib::WeakRef<ThumbnailSlot>,
    deferred: DeferredThumbnail,
) -> bool {
    if !schedule_thumbnail(
        deferred.key,
        deferred.kind,
        PendingTarget {
            image_id,
            request,
            image,
        },
    ) {
        return false;
    }
    ACTIVE_REQUESTS.with(|requests| {
        if let Some(active) = requests
            .borrow_mut()
            .get_mut(&image_id)
            .filter(|active| active.id == request)
        {
            active.deferred = None;
        }
    });
    true
}

fn take_pending_targets(key: &ThumbnailKey, job_id: u64) -> Option<Vec<PendingTarget>> {
    PENDING_THUMBNAILS.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.get(key).is_some_and(|pending| pending.id == job_id) {
            pending.remove(key).map(|pending| pending.targets)
        } else {
            None
        }
    })
}

fn finish_thumbnail_targets(
    targets: Vec<PendingTarget>,
    texture: Option<&gdk::Texture>,
    path: &Path,
) {
    for target in targets {
        if !request_is_live(&target) {
            crate::metrics::mark_thumbnail_stale();
            continue;
        }
        let Some(texture) = texture else {
            ACTIVE_REQUESTS.with(|requests| {
                requests.borrow_mut().remove(&target.image_id);
            });
            continue;
        };
        apply_live_thumbnail(target, texture.clone(), path.to_path_buf());
    }
}

fn known_metadata<T: Copy>(value: &MetadataValue<T>) -> Option<T> {
    match value {
        MetadataValue::Known(value) => Some(*value),
        MetadataValue::Unknown | MetadataValue::Unavailable => None,
    }
}

fn apply_thumbnail(image: &ThumbnailSlot, texture: &gdk::Texture, path: &Path) {
    image.set_texture(texture);
    image.set_opacity(1.0);
    register_displayed_thumbnail(image, path);
}

fn displayed_thumbnail_matches(image: &ThumbnailSlot, path: &Path) -> bool {
    TRACKED_THUMBNAILS.with(|thumbnails| {
        let mut thumbnails = thumbnails.borrow_mut();
        thumbnails.retain(|tracked| tracked.image.upgrade().is_some());
        thumbnails
            .iter()
            .find(|tracked| tracked.image.upgrade().as_ref() == Some(image))
            .is_some_and(|tracked| tracked.path == path)
    })
}

fn register_displayed_thumbnail(image: &ThumbnailSlot, path: &Path) {
    let weak_ref = glib::WeakRef::new();
    weak_ref.set(Some(image));
    TRACKED_THUMBNAILS.with(|thumbnails| {
        let mut thumbnails = thumbnails.borrow_mut();
        thumbnails.retain(|tracked| tracked.image.upgrade().is_some());
        if let Some(existing) = thumbnails
            .iter_mut()
            .find(|tracked| tracked.image.upgrade().as_ref() == Some(image))
        {
            existing.path = path.to_path_buf();
        } else {
            thumbnails.push(TrackedThumbnail {
                image: weak_ref,
                path: path.to_path_buf(),
            });
        }
    });
}

fn clear_displayed_thumbnail(image: &ThumbnailSlot) {
    TRACKED_THUMBNAILS.with(|thumbnails| {
        thumbnails.borrow_mut().retain(|tracked| {
            tracked
                .image
                .upgrade()
                .is_some_and(|tracked_image| tracked_image != *image)
        });
    });
}

pub(super) fn show_fallback_icon(image: &ThumbnailSlot, icon: &str, size: i32) {
    set_fallback_icon(image, None, icon, size);
}

pub(super) fn show_customized_icon(
    image: &ThumbnailSlot,
    path: &Path,
    fallback_icon: &str,
    size: i32,
) {
    set_fallback_icon(image, Some(path), fallback_icon, size);
}

pub(super) fn show_customized_icon_image(
    image: &gtk::Image,
    path: &Path,
    fallback_icon: &str,
    size: i32,
) {
    if image.pixel_size() != size {
        image.set_pixel_size(size);
    }
    if image.width_request() != size || image.height_request() != size {
        image.set_size_request(size, size);
    }
    apply_path_customization_image(image, path, fallback_icon);
}

pub(super) fn cancel_list_item_thumbnails(item: &glib::Object) {
    let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
        return;
    };
    if let Some(child) = item.child() {
        cancel_thumbnails_in(&child);
    }
}

pub(super) fn cancel_thumbnails_in(widget: &gtk::Widget) {
    if let Some(image) = widget.downcast_ref::<ThumbnailSlot>() {
        cancel_thumbnail(image.as_ptr() as usize);
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        child = current.next_sibling();
        cancel_thumbnails_in(&current);
    }
}

fn prepare_thumbnail_target(image: &ThumbnailSlot, size: i32) -> (usize, u64) {
    let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    let image_id = image.as_ptr() as usize;
    cancel_thumbnail(image_id);
    image.set_slot(size);
    (image_id, request)
}

fn set_fallback_icon(
    image: &ThumbnailSlot,
    path: Option<&Path>,
    icon: &str,
    size: i32,
) -> (usize, u64) {
    let ids = prepare_thumbnail_target(image, size);
    clear_displayed_thumbnail(image);
    let (texture, customized) = path_icon_texture(path, icon);
    image.set_fallback(icon, texture.as_ref());
    if let Some(p) = path {
        register_tracked_icon(image, p, icon, customized);
    }
    ids
}

fn path_icon_texture(path: Option<&Path>, fallback_icon: &str) -> (Option<gdk::Texture>, bool) {
    let Some(path) = path else {
        return (crate::assets::primary_icon_paintable(fallback_icon), false);
    };
    let theme_manager = super::theme::ThemeManager::shared();
    let custom_icon = theme_manager.custom_icon(path);
    let color = theme_manager.folder_color(path);
    let customized = custom_icon.is_some() || color.is_some();
    let texture = if fallback_icon == crate::assets::icons::FOLDER
        && let Some(decoration) = custom_icon.as_deref()
    {
        let color = color
            .as_ref()
            .map_or_else(crate::assets::primary_icon_color, |color| {
                color.hex().to_owned()
            });
        crate::assets::folder_decoration_paintable(decoration, &color)
    } else if let Some(emoji) = custom_icon
        .as_deref()
        .and_then(crate::assets::icons::custom_emoji)
    {
        crate::assets::emoji_icon_paintable(emoji)
    } else {
        let rendered_icon = custom_icon.as_deref().unwrap_or(fallback_icon);
        if let Some(color) = color {
            crate::assets::custom_colored_icon_paintable(rendered_icon, color.hex())
        } else {
            crate::assets::primary_icon_paintable(rendered_icon)
        }
    };
    (texture, customized)
}

fn apply_path_customization(image: &ThumbnailSlot, path: &Path, fallback_icon: &str) -> bool {
    let (texture, customized) = path_icon_texture(Some(path), fallback_icon);
    image.set_fallback(fallback_icon, texture.as_ref());
    customized
}

fn apply_path_customization_image(image: &gtk::Image, path: &Path, fallback_icon: &str) -> bool {
    let theme_manager = super::theme::ThemeManager::shared();
    let custom_icon = theme_manager.custom_icon(path);
    let color = theme_manager.folder_color(path);
    let customized = custom_icon.is_some() || color.is_some();

    if fallback_icon == crate::assets::icons::FOLDER
        && let Some(decoration) = custom_icon.as_deref()
    {
        let color = color
            .as_ref()
            .map_or_else(crate::assets::primary_icon_color, |color| {
                color.hex().to_owned()
            });
        crate::assets::set_folder_decoration_icon(image, decoration, &color);
    } else if let Some(emoji) = custom_icon
        .as_deref()
        .and_then(crate::assets::icons::custom_emoji)
    {
        crate::assets::set_emoji_icon(image, emoji);
    } else {
        let rendered_icon = custom_icon.as_deref().unwrap_or(fallback_icon);
        if let Some(color) = color {
            crate::assets::set_custom_colored_icon(image, rendered_icon, color.hex());
        } else {
            crate::assets::set_primary_icon(image, rendered_icon);
        }
    }
    customized
}

fn register_tracked_icon(image: &ThumbnailSlot, path: &Path, icon: &str, customized: bool) {
    let weak_ref = glib::WeakRef::new();
    weak_ref.set(Some(image));
    TRACKED_CUSTOMIZED_ICONS.with(|icons| {
        let mut icons = icons.borrow_mut();
        icons.retain(|t| t.image.upgrade().is_some());
        if let Some(existing) = icons
            .iter_mut()
            .find(|t| t.image.upgrade().as_ref() == Some(image))
        {
            existing.path = path.to_path_buf();
            existing.icon = icon.to_owned();
            existing.customized = customized;
        } else {
            icons.push(TrackedCustomizedIcon {
                image: weak_ref,
                path: path.to_path_buf(),
                icon: icon.to_owned(),
                customized,
            });
        }
    });
}

pub(super) fn refresh_customized_icons(paths: &[PathBuf]) {
    refresh_tracked_icons(|tracked| paths.iter().any(|candidate| candidate == &tracked.path));
}

pub(super) fn refresh_all_customized_icons() {
    refresh_tracked_icons(|_| true);
}

fn refresh_tracked_icons(matches: impl Fn(&TrackedCustomizedIcon) -> bool) {
    if REFRESHING_CUSTOMIZED_ICONS.with(|busy| busy.replace(true)) {
        return;
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REFRESHING_CUSTOMIZED_ICONS.with(|busy| busy.set(false));
        }
    }
    let _reset = Reset;
    let pending = TRACKED_CUSTOMIZED_ICONS.with(|icons| {
        let mut icons = icons.borrow_mut();
        icons.retain(|tracked| tracked.image.upgrade().is_some());
        icons
            .iter()
            .filter(|tracked| matches(tracked))
            .filter_map(|tracked| {
                let image = tracked.image.upgrade()?;
                image
                    .texture()
                    .is_none()
                    .then(|| (image, tracked.path.clone(), tracked.icon.clone()))
            })
            .collect::<Vec<_>>()
    });
    for (image, path, icon) in pending {
        cancel_thumbnail(image.as_ptr() as usize);
        let customized = apply_path_customization(&image, &path, &icon);
        TRACKED_CUSTOMIZED_ICONS.with(|icons| {
            let Ok(mut icons) = icons.try_borrow_mut() else {
                return;
            };
            if let Some(tracked) = icons
                .iter_mut()
                .find(|tracked| tracked.image.upgrade().as_ref() == Some(&image))
            {
                tracked.customized = customized;
            }
        });
    }
}

fn cancel_thumbnail(image_id: usize) {
    ACTIVE_REQUESTS.with(|requests| {
        requests.borrow_mut().remove(&image_id);
    });
    SETTLE_VIEWS.with(|views| {
        views.borrow_mut().retain(|_, settle| {
            settle
                .pending
                .retain(|park| park.target.image_id != image_id);
            !settle.pending.is_empty() || (settle.hooked && settle.viewport.upgrade().is_some())
        });
    });
    let cancelled = PENDING_THUMBNAILS.with(|pending| {
        let mut pending = pending.borrow_mut();
        let mut cancelled = Vec::new();
        pending.retain(|key, thumbnail| {
            thumbnail
                .targets
                .retain(|target| target.image_id != image_id);
            if thumbnail.targets.is_empty() {
                thumbnail.cancellation.cancel();
                cancelled.push(key.clone());
                false
            } else {
                true
            }
        });
        cancelled
    });
    THUMBNAIL_QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        for key in cancelled {
            queue.cancel(&key);
        }
    });
    retry_deferred_thumbnails();
}

fn is_heavy(kind: ThumbnailKind) -> bool {
    matches!(
        kind,
        ThumbnailKind::RawImage | ThumbnailKind::Pdf | ThumbnailKind::Video
    )
}

fn thumbnail_kind(path: &Path) -> Option<ThumbnailKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" => {
            Some(ThumbnailKind::Image)
        }
        "3fr" | "arw" | "cr2" | "cr3" | "dcr" | "dng" | "erf" | "kdc" | "mef" | "mos" | "mrw"
        | "nef" | "nrw" | "orf" | "pef" | "raf" | "raw" | "rw2" | "rwl" | "sr2" | "srf" | "srw"
        | "x3f" => Some(ThumbnailKind::RawImage),
        "pdf" => Some(ThumbnailKind::Pdf),
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "mpeg" | "mpg" | "ogv" => {
            Some(ThumbnailKind::Video)
        }
        _ => None,
    }
}

fn render_thumbnail(
    path: &Path,
    kind: ThumbnailKind,
    size: i32,
    cancellation: &Cancellation,
) -> Result<Vec<u8>, String> {
    let operation = match kind {
        ThumbnailKind::Image => ParseOperation::ThumbnailImage,
        ThumbnailKind::RawImage => ParseOperation::ThumbnailRaw,
        ThumbnailKind::Pdf => ParseOperation::ThumbnailPdf,
        ThumbnailKind::Video => ParseOperation::ThumbnailVideo,
    };
    crate::sandbox::parse(
        path,
        operation,
        size.clamp(16, 256),
        crate::sandbox::MediaPreviewBackend::Software,
        cancellation,
    )
    .map(|output| output.data)
}

#[cfg(test)]
pub(super) fn pending_thumbnail_id(path: &Path) -> Option<u64> {
    PENDING_THUMBNAILS.with(|pending| {
        pending
            .borrow()
            .iter()
            .find_map(|(key, pending)| (key.path == path).then_some(pending.id))
    })
}

#[cfg(test)]
pub(super) fn has_pending_thumbnail(path: &Path) -> bool {
    pending_thumbnail_id(path).is_some()
}

#[cfg(test)]
pub(super) fn hold_thumbnail_workers() {
    THUMBNAIL_QUEUE.with(|queue| queue.borrow_mut().running = MAX_THUMBNAIL_WORKERS);
}

#[cfg(test)]
pub(super) fn clear_thumbnail_runtime() {
    THUMBNAIL_QUEUE.with(|queue| {
        let mut queue = queue.borrow_mut();
        queue.running = 0;
        queue.queued.clear();
    });
    PENDING_THUMBNAILS.with(|pending| pending.borrow_mut().clear());
    LOOKUP_QUEUE.with(|queue| queue.borrow_mut().clear());
    ACTIVE_REQUESTS.with(|requests| requests.borrow_mut().clear());
    SETTLE_VIEWS.with(|views| views.borrow_mut().clear());
}

#[cfg(test)]
mod tests;
