// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use gtk::{gdk, glib, prelude::*};

use super::{DecodeExecutor, DecodeJob, MAX_QUEUED_DECODES, WORKER_COUNT, decode_png, submit};

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn blocked_job(gate: Arc<(Mutex<bool>, Condvar)>, started: Arc<AtomicUsize>) -> DecodeJob {
    DecodeJob {
        work: Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let (ready, wake) = &*gate;
            let mut ready = ready.lock().expect("decode gate should not be poisoned");
            while !*ready {
                ready = wake.wait(ready).expect("decode gate should remain usable");
            }
            Err("released test decode".to_owned())
        }),
        completion: Box::new(|_| {}),
    }
}

#[test]
fn executor_bounds_running_and_queued_decodes() {
    let executor = DecodeExecutor::new();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let started = Arc::new(AtomicUsize::new(0));
    for _ in 0..WORKER_COUNT {
        assert!(
            executor
                .submit(blocked_job(gate.clone(), started.clone()))
                .is_ok(),
            "workers should accept their running jobs"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while started.load(Ordering::SeqCst) != WORKER_COUNT {
        assert!(Instant::now() < deadline, "decode workers did not start");
        std::thread::yield_now();
    }
    for _ in 0..MAX_QUEUED_DECODES {
        assert!(
            executor
                .submit(blocked_job(gate.clone(), started.clone()))
                .is_ok(),
            "bounded queue should accept configured capacity"
        );
    }
    assert!(
        executor
            .submit(blocked_job(gate.clone(), started.clone()))
            .is_err(),
        "work beyond running plus queued capacity must be rejected"
    );
    *gate.0.lock().expect("decode gate should not be poisoned") = true;
    gate.1.notify_all();
}

#[test]
fn invalid_png_is_not_a_decoded_texture() {
    assert!(decode_png(b"not a png".to_vec()).is_err());
}

#[test]
fn worker_completion_is_delivered_on_the_main_context() {
    let _serial = crate::test_support::ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .expect("the async test lock should not be poisoned");
    let caller = std::thread::current().id();
    let (sender, receiver) = mpsc::sync_channel(1);
    submit(PNG.to_vec(), move |decoded| {
        sender
            .send((std::thread::current().id(), decoded))
            .expect("test receiver should remain live");
    })
    .expect("decode should be admitted");

    let deadline = Instant::now() + Duration::from_secs(2);
    let (completion_thread, decoded) = loop {
        if let Ok(result) = receiver.try_recv() {
            break result;
        }
        assert!(Instant::now() < deadline, "decode completion timed out");
        glib::MainContext::default().iteration(true);
    };
    let decoded = decoded.expect("fixture should decode");
    assert_eq!(completion_thread, caller);
    assert!(decoded.texture.is::<gdk::MemoryTexture>());
    assert_eq!(
        decoded.texture.format(),
        gdk::MemoryFormat::R8g8b8a8Premultiplied
    );
    assert_eq!(decoded.byte_len, 4);
}
