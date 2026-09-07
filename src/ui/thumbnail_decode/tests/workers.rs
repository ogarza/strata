// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

struct Gate(Arc<(Mutex<bool>, Condvar)>);
impl Drop for Gate {
    fn drop(&mut self) {
        *self.0.0.lock().expect("gate state") = true;
        self.0.1.notify_all();
    }
}

#[test]
fn warm_decode_progresses_while_all_render_and_persistence_workers_are_blocked() {
    let _serial = crate::test_support::ASYNC_MAIN_CONTEXT_DEFAULT
        .lock()
        .expect("main-context test lock");
    let gate = Gate(Arc::new((Mutex::new(false), Condvar::new())));
    let (started, rx) = mpsc::channel();
    for index in 0..=crate::sandbox::RENDER_LIMIT {
        let gate = gate.0.clone();
        let started = started.clone();
        let work = move || {
            started.send(()).expect("worker start notification");
            let mut ready = gate.0.lock().expect("gate state");
            while !*ready {
                ready = gate.1.wait(ready).expect("gate wakeup");
            }
        };
        if index == crate::sandbox::RENDER_LIMIT {
            super::super::submit_persist(work, |_| {}).expect("persistence admission");
        } else {
            super::super::submit_render(work, |_| {}).expect("render admission");
        }
    }
    for _ in 0..=crate::sandbox::RENDER_LIMIT {
        rx.recv_timeout(Duration::from_secs(2))
            .expect("worker started");
    }
    let (done, rx) = mpsc::channel();
    super::super::submit_owned(
        || decode_png(PNG.to_vec()),
        move |decoded| {
            done.send(decoded.is_ok()).expect("decode result");
        },
    )
    .expect("lookup admission");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(ok) = rx.try_recv() {
            assert!(ok);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "lookup was blocked by render or persistence"
        );
        glib::MainContext::default().iteration(false);
        std::thread::yield_now();
    }
}
