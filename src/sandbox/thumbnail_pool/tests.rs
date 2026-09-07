// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    sync::{Mutex, mpsc},
    thread,
    time::Duration,
};

static POOL_TESTS: Mutex<()> = Mutex::new(());

use super::{
    WorkerCommand, WorkerHandle, checkin_worker, idle_worker_count, note_spawn_failure,
    reserve_generation, retire_idle_thumbnail_worker_for_oneshot, shutdown_thumbnail_worker_pool,
    total_worker_count,
};

#[test]
fn pool_starts_empty_and_disk_hits_do_not_spawn_helpers() {
    let _serial = POOL_TESTS.lock().expect("pool tests should serialize");
    shutdown_thumbnail_worker_pool();

    assert_eq!(idle_worker_count(), 0);
    assert_eq!(total_worker_count(), 0);
}

#[test]
fn idle_helpers_retire_to_zero() {
    let _serial = POOL_TESTS.lock().expect("pool tests should serialize");
    shutdown_thumbnail_worker_pool();
    let generation = reserve_generation().expect("generation");
    let (sender, receiver) = mpsc::channel();
    checkin_worker(WorkerHandle { generation, sender });

    assert_eq!(idle_worker_count(), 1);
    assert_eq!(total_worker_count(), 1);
    thread::sleep(Duration::from_millis(200));

    assert_eq!(idle_worker_count(), 0);
    assert_eq!(total_worker_count(), 0);
    assert!(matches!(receiver.try_recv(), Ok(WorkerCommand::Retire)));
}

#[test]
fn one_shot_fallback_retires_one_idle_raster_worker() {
    let _serial = POOL_TESTS.lock().expect("pool tests should serialize");
    shutdown_thumbnail_worker_pool();
    let generation = reserve_generation().expect("generation");
    let (sender, receiver) = mpsc::channel();
    checkin_worker(WorkerHandle { generation, sender });

    retire_idle_thumbnail_worker_for_oneshot();

    assert_eq!(idle_worker_count(), 0);
    assert_eq!(total_worker_count(), 0);
    assert!(matches!(receiver.try_recv(), Ok(WorkerCommand::Retire)));
}

#[test]
fn spawn_backoff_bounds_repeated_start_failures() {
    let _serial = POOL_TESTS.lock().expect("pool tests should serialize");
    shutdown_thumbnail_worker_pool();
    note_spawn_failure();

    assert!(reserve_generation().is_err());
    thread::sleep(Duration::from_millis(700));

    assert!(reserve_generation().is_ok());
    shutdown_thumbnail_worker_pool();
}
