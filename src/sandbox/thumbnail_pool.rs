// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    os::fd::{AsFd, OwnedFd},
    path::Path,
    process::Child,
    sync::{Mutex, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::{Cancellation, ThumbnailRender, protocol};

const MAX_PERSISTENT_THUMBNAIL_WORKERS: usize = 4;
const SPAWN_BACKOFF: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const IDLE_TIMEOUT: Duration = Duration::from_millis(50);

static POOL: OnceLock<Mutex<PoolState>> = OnceLock::new();

fn pool() -> &'static Mutex<PoolState> {
    POOL.get_or_init(|| Mutex::new(PoolState::default()))
}

#[derive(Default)]
struct PoolState {
    idle: Vec<WorkerHandle>,
    workers: Vec<WorkerHandle>,
    total_workers: usize,
    next_generation: u64,
    spawn_backoff_until: Option<Instant>,
}

#[derive(Clone)]
struct WorkerHandle {
    generation: protocol::WorkerGeneration,
    sender: mpsc::Sender<WorkerCommand>,
}

enum WorkerCommand {
    Render(RenderRequest),
    Retire,
    Shutdown,
}

struct RenderRequest {
    snapshot: OwnedFd,
    operation: protocol::Operation,
    requested_edge: u16,
    cancellation: Cancellation,
    reply: mpsc::Sender<RenderResult>,
}

struct RenderResult {
    result: Result<ThumbnailRender, String>,
    reusable: bool,
}

struct WorkerReady {
    handle: WorkerHandle,
}

pub(crate) fn render_persistent_thumbnail(
    path: &Path,
    operation: protocol::Operation,
    requested_edge: i32,
    cancellation: &Cancellation,
) -> Result<ThumbnailRender, String> {
    if cancellation.is_cancelled() {
        return Err("Preview cancelled".to_owned());
    }
    let snapshot = super::sealed_raster_snapshot(path)?;
    if cancellation.is_cancelled() {
        return Err("Preview cancelled".to_owned());
    }
    let requested_edge = requested_edge.clamp(16, i32::from(protocol::MAX_EDGE)) as u16;
    let handle = checkout_worker()?;
    let generation = handle.generation;
    let (reply, result) = mpsc::channel();
    let request = RenderRequest {
        snapshot,
        operation,
        requested_edge,
        cancellation: cancellation.clone(),
        reply,
    };
    if handle.sender.send(WorkerCommand::Render(request)).is_err() {
        note_worker_dead(generation);
        return Err("Thumbnail worker retired before receiving the request".to_owned());
    }
    let result = result
        .recv_timeout(protocol::REQUEST_DEADLINE + Duration::from_secs(1))
        .unwrap_or_else(|_| RenderResult {
            result: Err("The thumbnail worker timed out".to_owned()),
            reusable: false,
        });
    if result.reusable {
        checkin_worker(handle);
    } else {
        note_worker_dead(generation);
    }
    result.result
}

pub(crate) fn retire_idle_thumbnail_worker_for_oneshot() {
    let worker = {
        let mut pool = pool().lock().unwrap_or_else(|poison| poison.into_inner());
        let worker = pool.idle.pop();
        if let Some(worker) = &worker {
            pool.workers
                .retain(|candidate| candidate.generation != worker.generation);
            pool.total_workers = pool.total_workers.saturating_sub(1);
        }
        worker
    };
    if let Some(worker) = worker {
        let _sent = worker.sender.send(WorkerCommand::Retire);
    }
}

pub(crate) fn shutdown_thumbnail_worker_pool() {
    let workers = {
        let mut pool = pool().lock().unwrap_or_else(|poison| poison.into_inner());
        pool.spawn_backoff_until = None;
        pool.total_workers = 0;
        pool.idle.clear();
        std::mem::take(&mut pool.workers)
    };
    for worker in workers {
        let _sent = worker.sender.send(WorkerCommand::Shutdown);
    }
}

fn checkout_worker() -> Result<WorkerHandle, String> {
    if let Some(worker) = pool()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .idle
        .pop()
    {
        return Ok(worker);
    }
    spawn_worker()
}

fn spawn_worker() -> Result<WorkerHandle, String> {
    let generation = reserve_generation()?;
    let (ready, ready_rx) = mpsc::channel();
    thread::Builder::new()
        .name("strata-thumbnail-worker".to_owned())
        .spawn(move || worker_thread(generation, ready))
        .map_err(|error| {
            note_worker_dead(generation);
            format!("Unable to start the thumbnail worker supervisor: {error}")
        })?;
    match ready_rx.recv_timeout(protocol::STARTUP_DEADLINE + Duration::from_secs(1)) {
        Ok(Ok(ready)) => {
            register_worker(ready.handle.clone());
            Ok(ready.handle)
        }
        Ok(Err(error)) => {
            note_worker_dead(generation);
            note_spawn_failure();
            Err(error)
        }
        Err(_) => {
            note_worker_dead(generation);
            note_spawn_failure();
            Err("The thumbnail worker startup timed out".to_owned())
        }
    }
}

fn reserve_generation() -> Result<protocol::WorkerGeneration, String> {
    let mut pool = pool().lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(backoff) = pool.spawn_backoff_until
        && Instant::now() < backoff
    {
        return Err("Thumbnail worker spawning is temporarily backed off".to_owned());
    }
    if pool.total_workers >= MAX_PERSISTENT_THUMBNAIL_WORKERS {
        return Err("No thumbnail worker capacity is available".to_owned());
    }
    pool.next_generation = pool.next_generation.saturating_add(1).max(1);
    let generation = protocol::WorkerGeneration::new(pool.next_generation)
        .map_err(|_| "Unable to allocate thumbnail worker generation".to_owned())?;
    pool.total_workers += 1;
    Ok(generation)
}

fn register_worker(worker: WorkerHandle) {
    pool()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .workers
        .push(worker);
}

fn checkin_worker(worker: WorkerHandle) {
    let generation = worker.generation;
    pool()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .idle
        .push(worker);
    thread::spawn(move || {
        thread::sleep(IDLE_TIMEOUT);
        retire_idle_worker(generation);
    });
}

fn retire_idle_worker(generation: protocol::WorkerGeneration) {
    let worker = {
        let mut pool = pool().lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(index) = pool
            .idle
            .iter()
            .position(|worker| worker.generation == generation)
        else {
            return;
        };
        let worker = pool.idle.remove(index);
        pool.workers
            .retain(|candidate| candidate.generation != worker.generation);
        pool.total_workers = pool.total_workers.saturating_sub(1);
        worker
    };
    let _sent = worker.sender.send(WorkerCommand::Retire);
}

fn note_worker_dead(generation: protocol::WorkerGeneration) {
    let mut pool = pool().lock().unwrap_or_else(|poison| poison.into_inner());
    pool.idle.retain(|worker| worker.generation != generation);
    pool.workers
        .retain(|worker| worker.generation != generation);
    pool.total_workers = pool.total_workers.saturating_sub(1);
}

fn note_spawn_failure() {
    pool()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .spawn_backoff_until = Some(Instant::now() + SPAWN_BACKOFF);
}

fn worker_thread(
    generation: protocol::WorkerGeneration,
    ready: mpsc::Sender<Result<WorkerReady, String>>,
) {
    let (sender, receiver) = mpsc::channel();
    let startup = start_worker(generation, sender.clone());
    let mut runtime = match startup {
        Ok(mut runtime) => {
            if ready
                .send(Ok(WorkerReady {
                    handle: WorkerHandle { generation, sender },
                }))
                .is_err()
            {
                runtime.terminate();
                return;
            }
            runtime
        }
        Err(error) => {
            let _sent = ready.send(Err(error));
            return;
        }
    };
    loop {
        match receiver.recv_timeout(IDLE_TIMEOUT) {
            Ok(WorkerCommand::Render(request)) => {
                let result = runtime.render(
                    request.snapshot,
                    request.operation,
                    request.requested_edge,
                    &request.cancellation,
                );
                let reusable = result.reusable;
                let _sent = request.reply.send(result);
                if !reusable {
                    runtime.terminate();
                    return;
                }
            }
            Ok(WorkerCommand::Retire | WorkerCommand::Shutdown)
            | Err(mpsc::RecvTimeoutError::Disconnected) => {
                runtime.terminate();
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

struct WorkerRuntime {
    child: Child,
    socket: OwnedFd,
    session: protocol::ParentSession,
    generation: protocol::WorkerGeneration,
    _private_output: super::PrivateOutput,
}

fn start_worker(
    generation: protocol::WorkerGeneration,
    _sender: mpsc::Sender<WorkerCommand>,
) -> Result<WorkerRuntime, String> {
    let current_executable = std::env::current_exe()
        .map_err(|error| format!("Unable to locate the Strata executable: {error}"))?;
    let running_executable = std::path::PathBuf::from(format!("/proc/{}/exe", std::process::id()));
    let private_output = super::PrivateOutput::create().map_err(|error| error.to_string())?;
    let executable = super::resolve_renderer_executable(
        &current_executable,
        &running_executable,
        private_output.path(),
    )?;
    let (child, socket) = super::spawn_persistent_thumbnail_worker(&executable)?;
    protocol::expect_ready(socket.as_fd())
        .map_err(|error| format!("Thumbnail worker did not become ready: {error:?}"))?;
    Ok(WorkerRuntime {
        child,
        socket,
        session: protocol::ParentSession::new(generation),
        generation,
        _private_output: private_output,
    })
}

impl WorkerRuntime {
    fn render(
        &mut self,
        snapshot: OwnedFd,
        operation: protocol::Operation,
        requested_edge: u16,
        cancellation: &Cancellation,
    ) -> RenderResult {
        if cancellation.is_cancelled() {
            return RenderResult {
                result: Err("Preview cancelled".to_owned()),
                reusable: true,
            };
        }
        let request = match self
            .session
            .begin_request(self.generation, operation, requested_edge)
        {
            Ok(request) => request,
            Err(error) => {
                return RenderResult {
                    result: Err(format!(
                        "Unable to prepare thumbnail worker request: {error:?}"
                    )),
                    reusable: false,
                };
            }
        };
        if let Err(error) = protocol::send_packet(
            self.socket.as_fd(),
            request,
            &[snapshot.as_fd()],
            protocol::REQUEST_DEADLINE,
        ) {
            return RenderResult {
                result: Err(format!(
                    "Unable to send thumbnail worker request: {error:?}"
                )),
                reusable: false,
            };
        }
        let packet = match protocol::recv_packet(self.socket.as_fd(), protocol::REQUEST_DEADLINE, 1)
        {
            Ok(packet) => packet,
            Err(error) => {
                return RenderResult {
                    result: Err(format!(
                        "Unable to receive thumbnail worker reply: {error:?}"
                    )),
                    reusable: false,
                };
            }
        };
        match self.session.accept_reply(packet) {
            Ok(protocol::JobReply::Success(output)) => {
                let metadata = output.metadata();
                RenderResult {
                    result: output
                        .read_all()
                        .map(|pixels| ThumbnailRender::Raw {
                            pixels,
                            width: i32::from(metadata.width),
                            height: i32::from(metadata.height),
                            stride: usize::try_from(metadata.stride)
                                .expect("validated thumbnail stride"),
                        })
                        .map_err(|error| {
                            format!("Unable to read thumbnail worker output: {error:?}")
                        }),
                    reusable: true,
                }
            }
            Ok(protocol::JobReply::JobFailure(_)) => RenderResult {
                result: Err("The thumbnail worker could not decode the image".to_owned()),
                reusable: true,
            },
            Err(error) => RenderResult {
                result: Err(format!("Invalid thumbnail worker reply: {error:?}")),
                reusable: false,
            },
        }
    }

    fn terminate(&mut self) {
        super::terminate(&mut self.child);
    }
}

#[cfg(test)]
pub(crate) fn idle_worker_count() -> usize {
    pool()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .idle
        .len()
}

#[cfg(test)]
pub(crate) fn total_worker_count() -> usize {
    pool()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .total_workers
}

#[cfg(test)]
#[path = "thumbnail_pool/tests.rs"]
mod tests;
