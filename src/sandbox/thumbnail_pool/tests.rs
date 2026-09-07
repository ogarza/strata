// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::MutexGuard,
};

fn state(pool: &Pool) -> MutexGuard<'_, PoolState> {
    pool.state.lock().expect("pool state")
}

fn sleeping_child() -> Child {
    Command::new("sleep")
        .arg("60")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("sleep fixture")
}

fn fake_runtime(pool: &Arc<Pool>) -> (WorkerRuntime, OwnedFd) {
    let slot = pool.reserve().expect("resident slot");
    let child = ChildGuard(sleeping_child());
    let host_start = process_start(child.0.id()).expect("child identity");
    let generation = protocol::WorkerGeneration::new(slot.generation).expect("generation");
    let (socket, peer) = protocol::control_socketpair().expect("socketpair");
    (
        WorkerRuntime {
            child,
            socket,
            session: protocol::ParentSession::new(generation),
            generation,
            host_start,
            _private_output: super::super::PrivateOutput::create().expect("private output"),
            slot,
        },
        peer,
    )
}

#[test]
fn resident_capacity_is_released_only_after_process_reaping() {
    let pool = Arc::new(Pool::default());
    assert_eq!(state(&pool).total, 0);
    let mut residents = Vec::new();
    for _ in 0..RENDER_LIMIT {
        residents.push(fake_runtime(&pool));
    }
    assert!(pool.reserve().is_err());
    let pid = residents[0].0.child.0.id();
    residents.remove(0);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(state(&pool).total, RENDER_LIMIT - 1);
    assert!(pool.reserve().is_ok());
}

#[test]
fn idle_retirement_uses_last_checkin_and_does_not_spawn_per_job_threads() {
    let pool = Arc::new(Pool::default());
    let (runtime, _peer) = fake_runtime(&pool);
    let pid = runtime.child.0.id();
    let base = Instant::now();
    pool.checkin(runtime);
    for _ in 0..100 {
        let runtime = pool.checkout().expect("reuse runtime");
        assert_eq!(runtime.child.0.id(), pid);
        pool.checkin(runtime);
    }
    state(&pool).idle[0].0 = base + Duration::from_secs(10);
    pool.retire_expired(base + IDLE_TIMEOUT);
    assert_eq!(state(&pool).idle.len(), 1);
    pool.retire_expired(base + IDLE_TIMEOUT + Duration::from_secs(10));
    assert_eq!(state(&pool).total, 0);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}

#[test]
fn failed_readiness_kills_and_reaps_spawned_child() {
    let pool = Arc::new(Pool::default());
    let slot = pool.reserve().expect("resident slot");
    let child = sleeping_child();
    let pid = child.id();
    let (socket, _peer) = protocol::control_socketpair().expect("socketpair");
    assert!(
        WorkerRuntime::from_child(
            slot,
            child,
            socket,
            super::super::PrivateOutput::create().expect("private output")
        )
        .is_err()
    );
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(state(&pool).total, 0);
}

#[test]
fn shutdown_interrupts_waits_without_main_context_progress() {
    let pool = Arc::new(Pool::default());
    let (mut runtime, peer) = fake_runtime(&pool);
    let pid = runtime.child.0.id();
    let shutdown_pool = pool.clone();
    let join = thread::spawn(move || {
        protocol::recv_packet(peer.as_fd(), Duration::from_secs(2), 1).expect("request");
        state(&shutdown_pool).shutdown = true;
        thread::sleep(Duration::from_millis(200));
    });
    let snapshot = protocol::sealed_memfd("fixture", b"fixture").expect("snapshot");
    assert!(
        runtime
            .render(snapshot, protocol::Operation::ThumbnailPng, 16)
            .is_err()
    );
    drop(runtime);
    join.join().expect("shutdown thread");
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(state(&pool).total, 0);
    assert!(pool.reserve().is_err());
}

#[test]
fn deadline_reaps_a_worker_that_never_replies() {
    let pool = Arc::new(Pool::default());
    let (mut runtime, _peer) = fake_runtime(&pool);
    let pid = runtime.child.0.id();
    let snapshot = protocol::sealed_memfd("fixture", b"fixture").expect("snapshot");
    assert!(
        runtime
            .render_until(
                snapshot,
                protocol::Operation::ThumbnailPng,
                16,
                Instant::now() + Duration::from_millis(100)
            )
            .is_err()
    );
    drop(runtime);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(state(&pool).total, 0);
}

#[test]
fn crash_backoff_and_staging_and_rss_budgets_are_process_wide() {
    let pool = Arc::new(Pool::default());
    pool.note_failure();
    assert!(pool.checkout().is_err());
    assert_eq!(state(&pool).total, 0);
    let cancellation = Cancellation::default();
    let lease = pool
        .stage(MAX_STAGED_BYTES, &cancellation)
        .expect("staging lease");
    assert_eq!(state(&pool).staged, MAX_STAGED_BYTES);
    cancellation.cancel();
    assert!(pool.stage(1, &cancellation).is_err());
    drop(lease);
    assert_eq!(state(&pool).staged, 0);
    assert!(pool.account_rss(1, MAX_WORKER_RSS + 1).is_err());
    assert!(pool.account_rss(1, MAX_WORKER_RSS).is_ok());
    assert!(pool.account_rss(2, MAX_WORKER_RSS).is_ok());
    assert!(pool.account_rss(3, 1).is_err());
}

#[test]
fn resource_measurement_follows_descendants_and_checks_host_identity() {
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 60 & wait"]).process_group(0);
    let child = ChildGuard(command.spawn().expect("process tree"));
    let pid = child.0.id();
    let start = process_start(pid).expect("child identity");
    assert!(process_tree_rss(pid, start).is_ok());
    assert!(process_tree_rss(pid, start + 1).is_err());
}

#[test]
fn real_bwrap_worker_reuses_pid_after_failure_idle_and_pdf() {
    let Some(executable) = std::env::var_os("STRATA_TEST_EXECUTABLE") else {
        assert!(
            std::env::var_os("STRATA_REQUIRE_SANDBOX_TESTS").is_none(),
            "STRATA_TEST_EXECUTABLE must name the built application for required sandbox tests"
        );
        eprintln!(
            "SKIP real worker test: set STRATA_TEST_EXECUTABLE and STRATA_REQUIRE_SANDBOX_TESTS=1"
        );
        return;
    };
    let pool = Arc::new(Pool::default());
    let mut runtime = WorkerRuntime::start_executable(
        pool.reserve().expect("resident slot"),
        Path::new(&executable),
        super::super::PrivateOutput::create().expect("private output"),
    )
    .expect("real sandbox must start");
    let pid = runtime.child.0.id();
    let input = protocol::sealed_memfd("bad-input", b"not an image").expect("snapshot");
    assert!(matches!(
        runtime
            .render(input, protocol::Operation::ThumbnailPng, 16)
            .expect("job reply"),
        Err(ThumbnailError::Content)
    ));
    pool.checkin(runtime);
    // Longer than the old helper request timeout, shorter than pool idle retirement.
    thread::sleep(protocol::REQUEST_DEADLINE + Duration::from_millis(100));
    let mut runtime = pool.checkout().expect("reuse runtime");
    assert_eq!(runtime.child.0.id(), pid);
    let pixbuf = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 4, 3)
        .expect("pixbuf fixture");
    pixbuf.fill(0xff000080);
    let png = pixbuf.save_to_bufferv("png", &[]).expect("PNG fixture");
    for _ in 0..3 {
        let input = protocol::sealed_memfd("image", &png).expect("snapshot");
        let output = runtime
            .render(input, protocol::Operation::ThumbnailPng, 16)
            .expect("job reply")
            .expect("image pixels");
        assert!(matches!(
            output,
            ThumbnailRender::Raw {
                width: 16,
                height: 12,
                ..
            }
        ));
        assert_eq!(runtime.child.0.id(), pid);
    }
    let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]/Resources<<>>>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF";
    let input = protocol::sealed_memfd("pdf", pdf).expect("PDF snapshot");
    assert!(matches!(
        runtime
            .render(input, protocol::Operation::ThumbnailPdf, 16)
            .expect("PDF job reply")
            .expect("PDF pixels"),
        ThumbnailRender::Raw {
            width: 16,
            height: 16,
            ..
        }
    ));
    pool.checkin(runtime);
    pool.retire_expired(Instant::now() + IDLE_TIMEOUT);
    assert_eq!(state(&pool).total, 0);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}
