// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
};

use gtk::{gdk, gdk_pixbuf, glib, prelude::*};

const WORKER_COUNT: usize = 2;
/// This queue is shared by lookup, decode, render, and persistence submissions. The
/// thumbnail request table supplies the single global 64-entry pipeline bound.
const MAX_QUEUED_DECODES: usize = 64;
const MAX_QUEUED_COMPLETIONS: usize = MAX_QUEUED_DECODES + WORKER_COUNT;
const MAX_TEXTURE_EDGE: i32 = 512;
const MAX_TEXTURE_BYTES: usize = 512 * 512 * 4;
const MEMORY_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::R8g8b8a8Premultiplied;
const BYTES_PER_PIXEL: usize = 4;

type Completion = Box<dyn FnOnce() + Send + 'static>;

pub(super) struct DecodedTexture {
    pub(super) texture: gdk::Texture,
    pub(super) byte_len: usize,
}

struct DecodeJob {
    work: Box<dyn FnOnce() + Send + 'static>,
}

struct CompletionBridge {
    sender: SyncSender<Completion>,
    receiver: Mutex<Receiver<Completion>>,
    scheduled: AtomicBool,
}

impl CompletionBridge {
    fn new() -> Arc<Self> {
        let (sender, receiver) = sync_channel(MAX_QUEUED_COMPLETIONS);
        Arc::new(Self {
            sender,
            receiver: Mutex::new(receiver),
            scheduled: AtomicBool::new(false),
        })
    }

    fn send(self: &Arc<Self>, completion: Completion) {
        // Backpressure stays on these dedicated workers; one thread-safe GLib source drains
        // the bounded queue instead of adding a main-context source per decoded image.
        if self.sender.send(completion).is_err() {
            return;
        }
        if !self.scheduled.swap(true, Ordering::AcqRel) {
            let bridge = self.clone();
            glib::idle_add_once(move || bridge.drain());
        }
    }

    fn drain(self: Arc<Self>) {
        loop {
            while let Some(completion) = self.try_recv() {
                completion();
            }
            self.scheduled.store(false, Ordering::Release);
            match self.try_recv() {
                Some(completion) => {
                    self.scheduled.store(true, Ordering::Release);
                    completion();
                }
                None => break,
            }
        }
    }

    fn try_recv(&self) -> Option<Completion> {
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        receiver.try_recv().ok()
    }
}

struct DecodeExecutor {
    sender: SyncSender<DecodeJob>,
    completions: Arc<CompletionBridge>,
}

impl DecodeExecutor {
    fn new() -> Self {
        let (sender, receiver) = sync_channel::<DecodeJob>(MAX_QUEUED_DECODES);
        let receiver = Arc::new(Mutex::new(receiver));
        let completions = CompletionBridge::new();
        for index in 0..WORKER_COUNT {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name(format!("strata-thumb-exec-{index}"))
                .spawn(move || worker_loop(&receiver))
                .expect("thumbnail decode worker should start");
        }
        Self {
            sender,
            completions,
        }
    }

    fn submit(&self, job: DecodeJob) -> Result<(), DecodeJob> {
        match self.sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => Err(job),
        }
    }
}

fn worker_loop(receiver: &Mutex<Receiver<DecodeJob>>) {
    loop {
        let job = receiver
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .recv();
        let Ok(job) = job else {
            return;
        };
        (job.work)();
    }
}

fn executor() -> &'static DecodeExecutor {
    static EXECUTOR: OnceLock<DecodeExecutor> = OnceLock::new();
    EXECUTOR.get_or_init(DecodeExecutor::new)
}

fn submit_work<R>(
    work: impl FnOnce() -> R + Send + 'static,
    completion: impl FnOnce(R) + Send + 'static,
) -> Result<(), String>
where
    R: Send + 'static,
{
    let executor = executor();
    let completions = executor.completions.clone();
    executor
        .submit(DecodeJob {
            work: Box::new(move || {
                let result = work();
                completions.send(Box::new(move || completion(result)));
            }),
        })
        .map_err(|_| "thumbnail executor queue is full".to_owned())
}

pub(super) fn submit(
    png: Vec<u8>,
    completion: impl FnOnce(Result<DecodedTexture, String>) + Send + 'static,
) -> Result<(), String> {
    submit_work(
        move || {
            let started = std::time::Instant::now();
            let result = decode_png(png);
            crate::metrics::record_thumbnail_stage(
                crate::metrics::ThumbnailStage::ParentDecode,
                started.elapsed(),
            );
            result
        },
        completion,
    )
}

pub(super) fn submit_raw(
    pixels: Vec<u8>,
    width: i32,
    height: i32,
    stride: usize,
    completion: impl FnOnce(Result<(DecodedTexture, Vec<u8>), String>) + Send + 'static,
) -> Result<(), String> {
    submit_work(
        move || decode_raw(pixels, width, height, stride),
        completion,
    )
}

fn decode_raw(
    pixels: Vec<u8>,
    width: i32,
    height: i32,
    stride: usize,
) -> Result<(DecodedTexture, Vec<u8>), String> {
    if width <= 0 || height <= 0 || width > MAX_TEXTURE_EDGE || height > MAX_TEXTURE_EDGE {
        return Err("thumbnail decoded outside the supported dimensions".to_owned());
    }
    let minimum_stride = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| "thumbnail row size overflowed".to_owned())?;
    let required = stride
        .checked_mul(usize::try_from(height).map_err(|_| "invalid thumbnail height")?)
        .ok_or_else(|| "thumbnail texture size overflowed".to_owned())?;
    if stride < minimum_stride || required != pixels.len() || required > MAX_TEXTURE_BYTES {
        return Err("thumbnail pixel buffer has invalid bounds".to_owned());
    }
    let bytes = glib::Bytes::from_owned(pixels);
    let texture =
        gdk::MemoryTexture::new(width, height, gdk::MemoryFormat::R8g8b8a8, &bytes, stride)
            .upcast();
    let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        width,
        height,
        i32::try_from(stride).map_err(|_| "thumbnail stride overflowed")?,
    );
    let png = pixbuf
        .save_to_bufferv("png", &[])
        .map_err(|error| error.to_string())?;
    Ok((
        DecodedTexture {
            texture,
            byte_len: required,
        },
        png,
    ))
}

pub(super) fn submit_owned<R>(
    work: impl FnOnce() -> R + Send + 'static,
    completion: impl FnOnce(R) + Send + 'static,
) -> Result<(), String>
where
    R: Send + 'static,
{
    submit_work(work, completion)
}

fn decode_png(png: Vec<u8>) -> Result<DecodedTexture, String> {
    // gdk4 0.11 marks textures Send + Sync and its GDK initialization check is a no-op.
    // Full codec parsing is still an unsandboxed S3 trust-boundary limitation.
    let encoded = glib::Bytes::from_owned(png);
    let decoded = gdk::Texture::from_bytes(&encoded).map_err(|error| error.to_string())?;
    let width = decoded.width();
    let height = decoded.height();
    if width <= 0 || height <= 0 || width > MAX_TEXTURE_EDGE || height > MAX_TEXTURE_EDGE {
        return Err("thumbnail decoded outside the supported dimensions".to_owned());
    }

    let minimum_stride = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(BYTES_PER_PIXEL))
        .ok_or_else(|| "thumbnail row size overflowed".to_owned())?;
    let mut downloader = gdk::TextureDownloader::new(&decoded);
    downloader.set_format(MEMORY_FORMAT);
    let (pixels, stride) = downloader.download_bytes();
    let required = stride
        .checked_mul(usize::try_from(height).map_err(|_| "invalid thumbnail height")?)
        .ok_or_else(|| "thumbnail texture size overflowed".to_owned())?;
    if stride < minimum_stride || required != pixels.len() || required > MAX_TEXTURE_BYTES {
        return Err("thumbnail pixel buffer has invalid bounds".to_owned());
    }

    let texture = gdk::MemoryTexture::new(width, height, MEMORY_FORMAT, &pixels, stride).upcast();
    Ok(DecodedTexture {
        texture,
        byte_len: required,
    })
}

#[cfg(test)]
#[path = "thumbnail_decode/tests.rs"]
mod tests;
