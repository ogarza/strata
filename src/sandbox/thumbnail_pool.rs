// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, HashSet},
    os::fd::{AsFd, OwnedFd},
    path::Path,
    process::Child,
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
    thread,
    time::{Duration, Instant},
};

use super::{Cancellation, ThumbnailError, ThumbnailRender, protocol};

pub(crate) const RENDER_LIMIT: usize = 4;
const SPAWN_BACKOFF: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_STAGED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_WORKER_RSS: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_RSS: u64 = 1024 * 1024 * 1024;

static POOL: OnceLock<Arc<Pool>> = OnceLock::new();

fn pool() -> &'static Arc<Pool> {
    POOL.get_or_init(|| {
        let pool = Arc::new(Pool::default());
        let weak = Arc::downgrade(&pool);
        thread::Builder::new()
            .name("strata-thumb-retire".to_owned())
            .spawn(move || {
                while let Some(pool) = weak.upgrade() {
                    pool.retire_expired(Instant::now());
                    let state = pool.state.lock().unwrap_or_else(|p| p.into_inner());
                    if state.shutdown && state.total == 0 {
                        break;
                    }
                    let _wait = pool.changed.wait_timeout(state, POLL_INTERVAL);
                }
            })
            .expect("thumbnail retirement thread should start");
        pool
    })
}

#[derive(Default)]
struct Pool {
    state: Mutex<PoolState>,
    changed: Condvar,
}

#[derive(Default)]
struct PoolState {
    idle: Vec<(Instant, WorkerRuntime)>,
    total: usize,
    generation: u64,
    staged: u64,
    rss: HashMap<u64, u64>,
    backoff: Option<Instant>,
    shutdown: bool,
}

pub(crate) struct ResidentSlot {
    pool: Weak<Pool>,
    generation: u64,
}

impl Drop for ResidentSlot {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            let mut state = pool.state.lock().unwrap_or_else(|p| p.into_inner());
            state.total -= 1;
            state.rss.remove(&self.generation);
            pool.changed.notify_all();
        }
    }
}

struct StagingLease {
    pool: Weak<Pool>,
    bytes: u64,
}

impl Drop for StagingLease {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.state.lock().unwrap_or_else(|p| p.into_inner()).staged -= self.bytes;
            pool.changed.notify_all();
        }
    }
}

impl Pool {
    fn check_running(&self) -> Result<(), String> {
        if self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown
        {
            Err("Thumbnail pool is shutting down".to_owned())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn reserve(self: &Arc<Self>) -> Result<ResidentSlot, String> {
        self.reserve_until(Instant::now())
    }

    fn reserve_until(self: &Arc<Self>, deadline: Instant) -> Result<ResidentSlot, String> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.shutdown {
                return Err("Thumbnail pool is shutting down".to_owned());
            }
            if state.total < RENDER_LIMIT {
                break;
            }
            if Instant::now() >= deadline {
                return Err("Thumbnail resident capacity timed out".to_owned());
            }
            state = self
                .changed
                .wait_timeout(state, POLL_INTERVAL)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or("Worker generation exhausted")?;
        state.total += 1;
        Ok(ResidentSlot {
            pool: Arc::downgrade(self),
            generation: state.generation,
        })
    }

    fn checkout(self: &Arc<Self>) -> Result<WorkerRuntime, String> {
        self.check_running()?;
        let idle = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .idle
            .pop();
        if let Some((_, runtime)) = idle {
            return Ok(runtime);
        }
        if self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .backoff
            .is_some_and(|until| Instant::now() < until)
        {
            return Err("Thumbnail worker spawning is temporarily backed off".to_owned());
        }
        let slot = self.reserve_until(Instant::now() + protocol::REQUEST_DEADLINE)?;
        WorkerRuntime::start(slot).inspect_err(|_| self.note_failure())
    }

    fn checkin(&self, runtime: WorkerRuntime) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutdown {
            drop(state);
            drop(runtime);
        } else {
            state.idle.push((Instant::now(), runtime));
            self.changed.notify_all();
        }
    }

    fn note_failure(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).backoff =
            Some(Instant::now() + SPAWN_BACKOFF);
    }

    fn retire_expired(&self, now: Instant) {
        let expired = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let shutdown = state.shutdown;
            let mut expired = Vec::new();
            let mut index = 0;
            while index < state.idle.len() {
                if shutdown || now.saturating_duration_since(state.idle[index].0) >= IDLE_TIMEOUT {
                    expired.push(state.idle.swap_remove(index).1);
                } else {
                    index += 1;
                }
            }
            expired
        };
        // A resident slot is released only after its process tree has been killed and reaped.
        drop(expired);
    }

    fn stage(
        self: &Arc<Self>,
        bytes: u64,
        cancellation: &Cancellation,
    ) -> Result<StagingLease, String> {
        if bytes > MAX_STAGED_BYTES {
            return Err("Thumbnail input exceeds staging budget".to_owned());
        }
        let deadline = Instant::now() + protocol::REQUEST_DEADLINE;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.shutdown || cancellation.is_cancelled() {
                return Err("Thumbnail staging cancelled".to_owned());
            }
            if state.staged + bytes <= MAX_STAGED_BYTES {
                state.staged += bytes;
                return Ok(StagingLease {
                    pool: Arc::downgrade(self),
                    bytes,
                });
            }
            if Instant::now() >= deadline {
                return Err("Thumbnail staging budget timed out".to_owned());
            }
            state = self
                .changed
                .wait_timeout(state, POLL_INTERVAL)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    fn account_rss(&self, generation: u64, bytes: u64) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.rss.insert(generation, bytes);
        if bytes > MAX_WORKER_RSS || state.rss.values().copied().sum::<u64>() > MAX_TOTAL_RSS {
            Err("Thumbnail renderer exceeded its memory budget".to_owned())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn render_persistent_thumbnail(
    path: &Path,
    operation: protocol::Operation,
    requested_edge: i32,
    cancellation: &Cancellation,
) -> Result<ThumbnailRender, ThumbnailError> {
    let pool = pool();
    pool.check_running()?;
    let revision = super::SourceRevision::read(path)?;
    let _staging = pool.stage(revision.size, cancellation)?;
    let snapshot = super::sealed_raster_snapshot_checked(path, revision, cancellation)?;
    let mut runtime = pool.checkout()?;
    let result = runtime.render(
        snapshot,
        operation,
        requested_edge.clamp(16, i32::from(protocol::MAX_EDGE)) as u16,
    );
    match result {
        Ok(reply) => {
            pool.checkin(runtime);
            reply
        }
        Err(error) => {
            pool.note_failure();
            drop(runtime);
            Err(error.into())
        }
    }
}

pub(crate) fn reserve_oneshot() -> Result<ResidentSlot, String> {
    let pool = pool();
    pool.check_running()?;
    let idle = pool
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .idle
        .pop();
    drop(idle);
    pool.reserve_until(Instant::now() + protocol::REQUEST_DEADLINE)
}

pub(crate) fn shutdown_thumbnail_worker_pool() {
    if let Some(pool) = POOL.get() {
        pool.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown = true;
        pool.changed.notify_all();
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            super::terminate(&mut self.0);
        }
    }
}

struct WorkerRuntime {
    child: ChildGuard,
    socket: OwnedFd,
    session: protocol::ParentSession,
    generation: protocol::WorkerGeneration,
    host_start: u64,
    _private_output: super::PrivateOutput,
    // Declared last: reaping must precede release of resident capacity.
    slot: ResidentSlot,
}

impl WorkerRuntime {
    fn start(slot: ResidentSlot) -> Result<Self, String> {
        let current = std::env::current_exe().map_err(|error| error.to_string())?;
        let running = std::path::PathBuf::from(format!("/proc/{}/exe", std::process::id()));
        let private_output = super::PrivateOutput::create().map_err(|error| error.to_string())?;
        let executable =
            super::resolve_renderer_executable(&current, &running, private_output.path())?;
        Self::start_executable(slot, &executable, private_output)
    }

    fn start_executable(
        slot: ResidentSlot,
        executable: &Path,
        private_output: super::PrivateOutput,
    ) -> Result<Self, String> {
        let (child, socket) = super::spawn_persistent_thumbnail_worker(executable)?;
        Self::from_child(slot, child, socket, private_output)
    }

    fn from_child(
        slot: ResidentSlot,
        child: Child,
        socket: OwnedFd,
        private_output: super::PrivateOutput,
    ) -> Result<Self, String> {
        let child = ChildGuard(child);
        let host_start = process_start(child.0.id())?;
        protocol::expect_ready(socket.as_fd())
            .map_err(|error| format!("Thumbnail worker did not become ready: {error:?}"))?;
        let generation = protocol::WorkerGeneration::new(slot.generation)
            .map_err(|error| format!("Invalid worker generation: {error:?}"))?;
        Ok(Self {
            child,
            socket,
            session: protocol::ParentSession::new(generation),
            generation,
            host_start,
            _private_output: private_output,
            slot,
        })
    }

    fn inspect(&mut self) -> Result<(), String> {
        let pool = self
            .slot
            .pool
            .upgrade()
            .ok_or("Thumbnail pool disappeared")?;
        pool.check_running()?;
        if self
            .child
            .0
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("Thumbnail worker exited".to_owned());
        }
        let bytes = process_tree_rss(self.child.0.id(), self.host_start)?;
        pool.account_rss(self.slot.generation, bytes)
    }

    fn render(
        &mut self,
        snapshot: OwnedFd,
        operation: protocol::Operation,
        edge: u16,
    ) -> Result<Result<ThumbnailRender, ThumbnailError>, String> {
        self.render_until(
            snapshot,
            operation,
            edge,
            Instant::now() + protocol::REQUEST_DEADLINE,
        )
    }

    fn render_until(
        &mut self,
        snapshot: OwnedFd,
        operation: protocol::Operation,
        edge: u16,
        deadline: Instant,
    ) -> Result<Result<ThumbnailRender, ThumbnailError>, String> {
        self.inspect()?;
        let request = self
            .session
            .begin_request(self.generation, operation, edge)
            .map_err(|error| format!("Invalid worker session: {error:?}"))?;
        protocol::send_packet(
            self.socket.as_fd(),
            request,
            &[snapshot.as_fd()],
            POLL_INTERVAL,
        )
        .map_err(|error| format!("Unable to send thumbnail request: {error:?}"))?;
        let packet = loop {
            self.inspect()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Thumbnail worker timed out".to_owned());
            }
            match protocol::recv_reply_packet(self.socket.as_fd(), remaining.min(POLL_INTERVAL)) {
                Ok(packet) => break packet,
                Err(protocol::ProtocolError::Timeout) => continue,
                Err(error) => return Err(format!("Unable to receive thumbnail reply: {error:?}")),
            }
        };
        self.inspect()?;
        match self
            .session
            .accept_reply(packet)
            .map_err(|error| format!("Invalid thumbnail reply: {error:?}"))?
        {
            protocol::JobReply::JobFailure(_) => Ok(Err(ThumbnailError::Content)),
            protocol::JobReply::Success(output) => {
                let metadata = output.metadata();
                if metadata.representation != protocol::Representation::Rgba8 {
                    return Err("Worker returned an unsupported representation".to_owned());
                }
                let pixels = output
                    .read_all()
                    .map_err(|error| format!("Invalid thumbnail output: {error:?}"))?;
                Ok(Ok(ThumbnailRender::Raw {
                    pixels,
                    width: i32::from(metadata.width),
                    height: i32::from(metadata.height),
                    stride: metadata.stride as usize,
                }))
            }
        }
    }
}

fn process_start(pid: u32) -> Result<u64, String> {
    let stat =
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|error| error.to_string())?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "Invalid process identity".to_owned())
}

fn process_tree_rss(root: u32, start: u64) -> Result<u64, String> {
    if process_start(root)? != start {
        return Err("Renderer process identity changed".to_owned());
    }
    let mut pending = vec![root];
    let mut visited = HashSet::new();
    let mut bytes = 0u64;
    while let Some(pid) = pending.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if visited.len() > 64 {
            return Err("Too many renderer descendants".to_owned());
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            if pid == root {
                return Err("Renderer disappeared".to_owned());
            }
            continue;
        };
        let rss = status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0);
        bytes = bytes.saturating_add(rss.saturating_mul(1024));
        let tasks =
            std::fs::read_dir(format!("/proc/{pid}/task")).map_err(|error| error.to_string())?;
        for (index, task) in tasks.enumerate() {
            if index >= 128 {
                return Err("Too many renderer threads".to_owned());
            }
            let task = task.map_err(|error| error.to_string())?;
            if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                pending.extend(
                    children
                        .split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok()),
                );
            }
        }
    }
    if process_start(root)? != start {
        return Err("Renderer process identity changed".to_owned());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
