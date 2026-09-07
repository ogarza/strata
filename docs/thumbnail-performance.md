# Thumbnail performance

## Scroll-settle tuning (R3, issue #516)

Icons/List now resume thumbnail admission after 20 ms without a scroll adjustment,
down from 80 ms. Each adjustment still resets the timer, and viewport-first
scheduling and allocation checks remain in place. This removes 60 ms of intentional
quiet-period delay; it does not guarantee processing or completion by the second
display frame. Compare post-scroll fill time and work admitted during flings using
the disposable fixtures below. Keep worker count and build profile fixed when
comparing delays. To roll back this tuning, restore the delay to 80 ms without
reverting other thumbnail work or touching personal caches.

## Viewport-first scheduling (R2, issue #516)

Thumbnail admission now uses allocated widget geometry, not just GTK bind order.
Mapped thumbnails intersecting the viewport take priority over a nearby prefetch
band extending 25% of its width/height beyond each edge. More distant, unmapped,
and unallocated targets stay parked without lookup or render work. Nested scroll
containers are all checked, including horizontal clipping in Columns.

- Binding records interest and schedules one coalesced low-priority main-context
  pump. Geometry inspection never runs inside factory bind or adjustment handlers.
  Adjustment changes pause new admission until a coalesced frame update permits
  the pump to inspect freshly allocated bounds. Temporary pauses retain queued
  lookup/render progress rather than restarting source lookup. Offscreen parks do not poll;
  scrolling, layout, mapping, or a completion wakes the scheduler.
- At most two source lookups are submitted at a time, matching the two lookup
  threads. The remaining work stays in a reorderable GTK-local queue. Visibility
  is checked again at lookup and render dispatch. An offscreen queued request is
  returned to its live consumers' parked state; a visible request can displace
  queued prefetch work when the 64-entry unique-request table is full. Prefetch
  renders also wait for visible source lookups to resolve, so they cannot occupy
  every renderer just before those visible misses become ready.
- The best eligible tier wins, with FIFO ordering inside that tier and the
  existing three-raster burst before eligible heavy work **within the same tier**.
  Total renders remain at most four, including at most one heavy render. A busy
  heavy lane does not prevent useful raster work. Already-dispatched executions
  retain their IDs, permits, revision validation, and reattachment opportunities;
  scrolling never kills a worker to free capacity.
- Icons/List use a quiet-period bind gate (20 ms after R3 tuning). It also pauses
  admission of already-parked requests for that viewport; another window's idle
  viewport can continue. Same-file texture preservation from R1 is unchanged.

### Queue and visible-paint measurements

`LookupWait` and `RenderWait` stage samples measure from admission to the relevant
queue until its worker starts, separately from `Lookup`/`Render` service time.
They exclude time parked before queue admission and reset when a request is
re-admitted. They are not end-to-end bind latency.

With `RUST_LOG=strata::metrics=debug`, `thumbnail post-settle viewport painted`
records `viewport_id`, `epoch`, `milestone` (`first` / `ninety_percent`), `total`,
`ready`, and `elapsed_micros`. Icons/List start a fresh epoch at each scroll
adjustment; elapsed time includes the configured settle gate. A fixed cohort of visible
thumbnail targets is captured at the first post-settle GTK after-paint callback;
prefetch targets are excluded. Existing same-file textures count as ready. Failed,
removed, or rebound cohort members cannot falsely satisfy the 90% threshold;
a later scroll starts a new cohort. Initial viewport registration also starts an
epoch, but other modes do not have the Icons/List scroll-epoch gate.

These are **post-settle GTK paint observations**, not compositor presentation
latency or the first instant an already-present texture became visible during a
fling. The 90% target rounds up. Empty cohorts report neither milestone. Paint
sampling stops at 90% and is disabled unless metrics debug logging is enabled;
callbacks are detached on unmap. Compare identical viewports, fixture sets,
build profiles, and logging levels. Regression-test timings are not benchmarks.

This diff does not change source isolation, cache identity, rendering/persistence,
RAM-before-disk ordering, or the texture-completion lane. Those last two latency
optimizations remain separate follow-ups. No measured speedup is claimed yet.

### Manual acceptance for R2

Use the disposable fixture/XDG setup below and the newly built binary. In Icons
and List, fling several screens forward and backward, stop, and repeat; include
warm-RAM and warm-disk restarts. The currently visible rows should fill before
nearby prefetch, without same-file fallback flashes or wrong-file row reuse.
Repeat with two windows, continuously scrolling one while leaving the other idle,
and with mixed images/PDFs. Check Columns horizontal scrolling and icon-size/view
changes for stranded thumbnails. Recheck source deletion/corruption and custom
icons. Capture a short before/after video and, optionally, the metrics above from
identical disposable fixtures; do not expose private paths or images.

Cleanup removes only the disposable fixture/XDG roots. Roll back only R2 changes
relative to the preserved R1 worktree, not the whole uncommitted correction stack;
never clear personal thumbnail caches.

## Review corrections (R1, issue #516)

The current implementation corrects the lifecycle and scheduling defects found
in the D00–D11 stack. The numbered sections below are delivery history; this
section describes the corrected execution model, with R2 admission updates above.

- Two lookup/decode threads, four render/supervision threads, and one persistence
  thread have separate bounded queues. A slow renderer or PNG persistence cannot
  occupy lookup capacity. The four render permits include heavy work (at most
  one RAW/PDF/video execution) and one-shot thumbnail backends.
- Executing requests retain their execution IDs and deduplication entries when
  the last consumer leaves. Rebinding attaches to the same resolved revision;
  queued requests without consumers are removed. Stale completions cannot release
  a replacement execution's permit. Same-file rebinds keep the displayed texture
  during asynchronous revalidation, and Icons/List scroll-deferred binds keep it
  without starting work. Different-file binds, custom icons, and non-thumbnail
  entries replace it immediately. Failed revalidation clears the retained image;
  stale failures cannot clear a recycled row's new thumbnail.
- The pool has one retirement thread, not one sleeping thread per completion.
  Its 30-second idle deadline is measured from the latest check-in. The helper
  waits for new requests until peer closure; a request deadline is not an idle
  timeout. Zero-FD content-failure replies leave the helper reusable.
- RAII cleanup kills/reaps failed startups and retired workers before releasing
  their resident slots. One-shot transitions likewise reap an idle helper before
  taking its slot. Startup and runtime failures enter a shared 500 ms spawn
  backoff; service failures are not put in the content-failure cache. Shutdown
  wakes retirement and is checked by active render supervision, without waiting
  on GTK.
- Supervision checks the host bwrap PID's start time and sums RSS across its
  descendant processes, including children launched by non-main threads. It
  samples during requests and after replies, retiring on measurement errors or
  excess: 512 MiB per worker tree, 1 GiB summed reported RSS. Source snapshots
  have a separate 512 MiB aggregate staging budget. RSS is sampled, not a cgroup
  hard limit; it does not include every shared/tmpfs backing page. Existing AS,
  file-size, and per-sandbox tmpfs limits remain in force.
- Source identity includes device/inode, size, and nanosecond mtime/ctime. Metadata
  supplied by the browser is a hint, not authoritative. Source resolution runs
  off GTK before RAM reuse; resident hits still reuse textures without decoding
  or spawning. Render, decode completion, and persistence revalidate the revision
  and pathname; staging checks both the opened FD and the pathname. These checks
  detect changes, not atomicity against arbitrary concurrent filesystem writes.
  Slow filesystem syscalls themselves cannot be interrupted by the copy-loop
  deadline; they stay on bounded background threads.
- Persisted thumbnails retain Freedesktop URI/mtime tags and add `Strata::Revision`
  so Strata rejects its own same-second stale entries. Foreign cache entries
  without that extension retain Freedesktop whole-second validation semantics.
- Protocol version 2 requires raw RGBA8 replies in the production pool. Raster
  pixbufs and Cairo PDF surfaces normalize directly to pixels without an internal
  PNG round trip. UI completion constructs a texture without PNG encoding;
  persistence alone encodes the tagged PNG. Shared-cache PNG parsing remains
  parent-side under the previously accepted S3 limitation.

### Required regression coverage

CI builds the actual application and requires the real-bwrap regression test;
missing bubblewrap, namespace support, or the specified executable is a failure.
The real test covers decode-failure recovery, repeated raster/PDF rendering in
one worker, reuse after more than 12 seconds idle, and idle retirement. Adjacent
unit tests cover heavy-only scheduling, active reattachment, stale permit release,
separate execution lanes, failed-readiness reaping, shutdown/deadlines without
GTK progress, staging/RSS bounds, source changes, and raw cache persistence.

```bash
cargo build --all-features
xvfb-run -a env -u WAYLAND_DISPLAY GDK_BACKEND=x11 \
  GTK_A11Y=none NO_AT_BRIDGE=1 STRATA_REQUIRE_GTK_TESTS=1 \
  STRATA_REQUIRE_SANDBOX_TESTS=1 STRATA_TEST_EXECUTABLE="$PWD/target/debug/strata" \
  cargo test --all-targets --all-features
```

Without the executable variable, ordinary developer test runs explicitly report
the real-worker capability skip. Such a run does not substitute for this gate.

### Manual acceptance for R1

Use the disposable fixture/XDG setup below. Open a cold mixed folder and a
PDF-only folder; navigate away/back during work and switch windows/view sizes.
Expect no thumbnail→fallback→thumbnail flash when scrolling/rebinding the same
file, no previous-file thumbnail on recycled rows, no duplicate/wrong-row results,
at most four active renders and one heavy
render, stable helper PIDs, and prompt warm-cache results while renderers are
busy. Include a corrupt image followed by a valid image, revisit after 15 seconds
idle, then wait over 30 seconds after the last job and verify idle helpers exit.
Replace a fixture while work is pending and confirm its new thumbnail/cache
revision wins. Compare transparent/colorful images and PDF output, restart with
warm disk cache, and exit during rendering. Delete only the disposable fixture
and XDG roots afterward; do not clear personal thumbnail caches.

No performance improvement is claimed without a new owner-run comparison.

## D00 baseline observability

This document records the baseline instrumentation for issue #516. D00 does not
change thumbnail scheduling, cache keys, decoding, sandboxing, or worker
lifecycle.

The application emits the following process-wide thumbnail counters in debug
logs: `eligible`, `requested`, `started`, `completed`, `applied`, `cancelled`,
and `stale`. `started` currently includes the existing lookup-and-render job;
it is not a render-only counter. Stage timing samples are recorded separately
for `lookup`, `render`, and `persist`. `parent_decode` and `apply` are reserved
stage names for later instrumentation. Each sample adds one call and elapsed
microseconds using relaxed atomics; no thumbnail payload or path is retained.

The D03 pipeline separates lookup from render admission. Lookup and PNG decode,
one-shot sandbox spawn/wait, and persistence run on a bounded reusable
thumbnail-owned executor; none use `gio::spawn_blocking` or block GTK. A lookup
hit never acquires a render permit. A miss is promoted once to the existing
one-shot sandbox renderer, retaining its deduplicated targets and render permit
through decode. Persistence remains best effort.

## D06a protocol and output transport proof

D06a adds a GTK-free `sandbox::protocol` module for a future persistent thumbnail worker control plane. Production thumbnails and previews still use the existing one-shot sandbox routes; no persistent worker is spawned or routed in this diff.

The protocol uses small fixed `SOCK_SEQPACKET` Unix control packets and sealed memfd output transport. It defines a readiness/version handshake, one active request per worker, parent worker generations, exact request/reply envelopes, bounded thumbnail output metadata, distinct job-failure versus protocol-contract failure handling, descriptor-count and ancillary-truncation validation, required memfd seals, regular-file and length checks, a 4 MiB thumbnail output cap, checked allocation bounds, `EINTR`/`EAGAIN`/peer-closure handling, and `MSG_NOSIGNAL` sends. Tests exercise sequential round trips, unsupported opcode recovery, malformed packets, version/ID/status/ordering errors, descriptor rejection and closure, ancillary truncation, invalid output metadata, missing seals, bad lengths, oversized outputs, peer closure, timeouts, high-entropy maximum-size output, and requested-edge metadata.

D06a does not pass paths or URIs as job capabilities and does not decode untrusted user files outside bwrap. S1, S2, and S3 remained open until D06b authorization.

## D06b sandboxed input-isolation and worker proof

D06b records the approved security decisions for the persistent raster-worker proof: sealed bounded raster snapshot memfds for S1, accepted persistent decoder reuse after that sealed-input proof for S2, and documented parent-side bounded PNG decode exposure for S3. Production thumbnail routing remains one-shot; the worker proof is not the D07 production pool.

The parent-side snapshot helper opens raster sources with `CLOEXEC` and `NONBLOCK`, rejects non-regular or oversized files, copies into an anonymous memfd, detects size/mtime changes during staging, and seals the snapshot against writes, growth, and shrink before it can be sent. The persistent worker command has no source path bind and no writable output bind. The helper entrypoint receives one sealed snapshot fd per request over the D06a protocol, decodes only through that fd, sends sealed output memfds, and remains reusable after bounded decode failures.

The accepted S2 tradeoff means native decoder state can persist across raster files inside a worker. D06b documents that blast radius but does not implement per-file disposable decoder children. The accepted S3 scope keeps parent-side bounded PNG decoding for shared-cache entries; allocation and transport validation do not prove codec safety.

## D10 fixed total render policy

D10 fixes the process-wide total thumbnail render limit at four. The scheduler
applies this same limit to raster, PDF, RAW, video, and every one-shot fallback;
the persistent pool uses the same four-worker ceiling. Workers still grow lazily
only when admitted work needs one, and retire safely after idle timeout. The
existing heavy subset limit remains one within the total limit. Staging, memory,
and idle-resident safeguards remain independent of CPU count.

There is intentionally no user-facing worker preference: the four-slot behavior
is the established D00/D07 comparison baseline, while decoder subprocesses and
provider internals may use additional threads.

## D11 raw-pixel worker replies

D11 changes persistent image/PDF worker replies from PNG bytes to validated
straight-alpha RGBA8 pixels in sealed output memfds. The wire metadata declares
width, height, stride, and exact byte length; the parent rejects overflow,
undersized rows, mismatched lengths, missing seals, and outputs beyond the
existing cap before constructing a `MemoryTexture`.

The parent no longer decodes the persistent worker's PNG reply. It encodes the
validated pixels to PNG only on the bounded persistence path, preserving the
Freedesktop thumbnail cache format. Shared-cache PNG lookup remains parent-side
and bounded but unsandboxed, as required by the accepted S3 limitation. One-shot
RAW/video and preview routes remain unchanged.

## D09 idle viewport scheduling

Thumbnail bind/park admission already avoids a settle timeout for an idle initial viewport. D09 narrows the remaining viewport debounce: adjustment value changes still use the 120 ms scroll settle delay and the 400 ms starvation cap, but geometry/content-only adjustment changes schedule the next main-loop fire without imposing another scroll delay. This keeps firing out of GTK bind/adjustment callbacks while avoiding a redundant wait from relayout or content-size notifications.

The independent viewport groups, offscreen/liveness checks, cancellation behavior, List/Icons bind gate, and persistent worker routing are unchanged.

## D08 heavy-provider migration

D08 extends the persistent pool to PDF thumbnails. PDF requests use the same sealed bounded snapshot memfd, D06a control channel, one-active-request worker lifecycle, and parent-side bounded PNG reply validation as raster images. PDF remains heavy in the scheduler, so heavy concurrency stays at most one within the total four render slots.

RAW and video thumbnails intentionally remain on the one-shot sandbox path in D08. RAW still needs explicit format-hint and per-job workspace validation across dcraw/ImageMagick/simple_dcraw before reuse can safely avoid stale `/tmp/raw-thumb*` outputs. Video still needs a non-full-file source strategy for large files; D08 does not snapshot arbitrary video inputs into RAM. These exceptions stay sandboxed, consume the same render budget, and are documented rather than granting original host paths or fds to persistent decoders.

## D07 persistent raster pool

Production raster-image misses use the D06b sealed-snapshot worker through a lazy process-wide pool. The pool starts with zero helpers and creates a sandbox only after the existing scheduler admits a visible raster miss. Disk-cache hits and RAM hits do not spawn helpers. Healthy raster helpers are reused across rows, folders, views, and windows; each worker still handles one active request at a time over the D06a protocol and receives only a sealed bounded snapshot memfd, never an original source path or host source fd.

Each helper is owned by a thumbnail supervision thread. Startup, request send/receive, reply validation, and output reading are bounded by the protocol deadlines. Crashes, protocol failures, timeouts, or failed startup retire the affected helper, release the scheduler slot through the normal completion path, and enter a short process-wide spawn backoff so a missing or broken sandbox cannot fan out across the 64-entry queue. Decode failures are classified as job failures and leave the worker reusable. Idle helpers retire back to zero after 30 seconds, and application shutdown asks idle helpers to exit. RAW, video, and previews remain on their previous one-shot routes; before starting one of those incompatible one-shot thumbnail backends, the pool retires one idle persistent helper so migration does not silently add a fifth resident sandbox process outside the four-slot render budget.

D07 retains the D06b decisions and limitations. Sealed snapshots protect the original source from persistent raster workers, but decoder/library state may carry between raster files until the helper retires. The parent still performs bounded PNG decoding for shared-cache entries, while persistent worker replies are raw pixels; transport validation is not a codec-safety claim.

## D05 unified render budget

Render misses now use a single four-slot scheduler. RAW, PDF, and video jobs are
classified as heavy and are limited to one active heavy job within those four
slots. Raster work is admitted in a bounded burst of three before an eligible
heavy job, preventing sustained image traffic from starving heavy providers.
Lookup remains independent of this render budget.

Queued jobs retain their provider kind through admission, execution, cancellation,
and retry. Render permits are released by the completed source key, so completion
order cannot release another job's permit. A cancelled consumer is detached while
pending work is removed; stale completions remain rejected. This is still the
one-shot renderer path: persistent workers, watchdogs, and provider-specific
resource supervision are not part of D05.

## D04 resolved source identity

Thumbnail lookup now resolves local regular-file size and modification time on
the thumbnail-owned executor when the browser entry does not have metadata yet.
The resolved revision is used for cache lookup, pending-work rekeying, and
subsequent render results, so thumbnails no longer wait for
`BrowserEvent::MetadataFilled`. Callers for the same path coalesce while a
revision is being resolved. Browser metadata filling still runs for size/date
labels and sorting; it is no longer a thumbnail scheduling dependency.

Resolution follows the existing local-path behavior and returns the original
request key when stat fails or the path is not a regular file. The one-shot
renderer still reopens the canonicalized path, so D04 does not claim atomic
source snapshots or persistent-worker input isolation. Source replacement,
symlink policy, and stronger identity validation remain follow-up concerns.

## D03 bounded thumbnail execution

The process-wide executor has two reusable thumbnail-owned threads, a bounded
64-job work queue, and a bounded thread-safe completion bridge. The existing
64-entry pending-request table remains the global bound for unique pipeline
work; lookup, render, decode, and persistence do not add independent per-stage
request tables. Completion payloads and the persistence queue are bounded, and
cancelled targets are rechecked before expensive work and before application.

Warm disk hits proceed through lookup and decode while all four render permits
are occupied. Render misses use the existing four-entry running render bound;
contention defers work for retry rather than discarding live requests. Duplicate
widgets continue to share one pending lookup/decode/render execution, and the
same cancellation, stale-target, negative-cache, custom-icon, trash, canonical
256 px, and cross-size texture behavior remains in force. The renderer is still
one-shot; persistent sandbox workers are not part of D03.

Stage timing retains separate lookup, render, parent-decode, and persist samples.
`started` now records lookup admission and render admission, while
`lookup_hits`/`lookup_misses` identify the lookup result.

## D02 decoded-texture cache

Ready RAM entries contain one cloneable, normalized `gdk::MemoryTexture` for a
source revision. Binding or applying a ready entry only assigns that paintable;
it does not retain or parse PNG bytes. The display slot remains widget state, so
the same texture is reused across Icons, List, and Columns sizes.

A process-wide thumbnail-owned executor has two reusable decode threads and a
bounded four-job queue. Each unique lookup/render completion is decoded once,
normalized to premultiplied RGBA8, and downloaded with an explicit stride. RAM
accounting uses the returned backing-buffer length after validating stride,
dimensions, and checked size arithmetic rather than assuming every source
texture occupies `width * height * 4`. The executor sends bounded completions
through `glib::idle_add_once`; GTK targets and request registries are touched
only when that thread-safe bridge runs on the main context.

Only a successful parent decode enters the ready cache or persistence queue.
A shared-cache PNG that passes header/tag checks but fails full decoding is a
cache miss and retries through the existing one-shot renderer while retaining
the original render permit and deduplicated targets. A malformed renderer
result is a bounded failure. The Freedesktop `large` bucket, canonical 256 px
render edge, URI/mtime tags, and best-effort persistence layout are unchanged.
D03 moves the lookup, rendering, and persistence execution described above onto
the shared thumbnail-owned executor; D02's texture representation and
thread-safe completion bridge are reused. D03 does not introduce persistent
sandbox workers.

The checked-in `gdk4` 0.11 bindings used here mark `Texture` and
`MemoryTexture` as `Send + Sync`, and GDK's initialization assertion is a no-op
because GDK 4 has no runtime initializer. This permits construction on the
decode threads; widget mutation remains GTK-main-context-only.

Shared-cache PNG remains untrusted codec input decoded in the unsandboxed
parent. Header, dimension, and byte bounds limit work but do not make codec
parsing safe. D02 moves that existing exposure off GTK; it does not resolve or
claim approval of the open S3 trust-boundary decision.

## Reproducible baseline recipe

Use disposable fixtures and an isolated XDG root. Do not remove or modify the
personal `~/.cache/thumbnails/large` cache, drop kernel caches, or reuse an
already-running Strata process.

1. Build a release binary: `cargo build --release`.
2. Create a temporary directory outside the repository and generate or copy a
   non-private fixture set containing ordinary images plus, when available,
   RAW, PDF, and video files. Include malformed and deliberately large files.
   Keep fixture generation single-threaded or otherwise bounded.
3. Set `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`, and `XDG_DATA_HOME` to separate
   subdirectories of that temporary directory. Start a private D-Bus session
   and launch the release binary under a private Xvfb display, for example:

   ```bash
   dbus-run-session -- xvfb-run -a env -u WAYLAND_DISPLAY \
     GDK_BACKEND=x11 GTK_A11Y=none NO_AT_BRIDGE=1 \
     XDG_CACHE_HOME="$ROOT/cache" XDG_CONFIG_HOME="$ROOT/config" \
     XDG_DATA_HOME="$ROOT/data" ./target/release/strata "$ROOT/fixtures"
   ```

4. Record the exact binary revision, display scale, viewport dimensions, view
   mode, fixture manifest, and launcher environment.
5. Run cold-app-cache, warm-disk-cache, and warm-RAM cases several times. Also
   navigate away during active work and switch list/icons/columns and slider
   sizes. Capture logs without publishing private paths.

For each case record first labels/fallbacks, first thumbnail, visible-viewport
completion, lookup/render/persistence timing, navigation latency, helper and
descendant process counts/RSS, and thumbnail counter totals. Report medians
and tails only with enough repeated samples; do not call an uncontrolled run
"cold filesystem". D00 establishes measurements and proposed tolerances; it
makes no performance claim until the owner reviews the results.

## Approved thumbnail worker decisions

S1 is implemented for production raster workers with sealed bounded raster snapshot memfds; no original host source fd or path is sent to the persistent decoder. S2 accepts persistent decoder process reuse after that sealed-input proof and documents the cross-file decoder-state blast radius. S3 remains an accepted exposure for this stage: the parent decodes bounded PNG data from shared-cache entries, and those bounds do not prove codec safety.
