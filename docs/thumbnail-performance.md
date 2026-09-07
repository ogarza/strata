# Thumbnail performance

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

The accepted S2 tradeoff means native decoder state can persist across raster files inside a worker. D06b documents that blast radius but does not implement per-file disposable decoder children. The accepted S3 scope keeps parent-side bounded PNG decoding for shared-cache entries and validated helper outputs; allocation and transport validation do not prove codec safety.

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

## D09 idle viewport scheduling

Thumbnail bind/park admission already avoids a settle timeout for an idle initial viewport. D09 narrows the remaining viewport debounce: adjustment value changes still use the 120 ms scroll settle delay and the 400 ms starvation cap, but geometry/content-only adjustment changes schedule the next main-loop fire without imposing another scroll delay. This keeps firing out of GTK bind/adjustment callbacks while avoiding a redundant wait from relayout or content-size notifications.

The independent viewport groups, offscreen/liveness checks, cancellation behavior, List/Icons bind gate, and persistent worker routing are unchanged.

## D08 heavy-provider migration

D08 extends the persistent pool to PDF thumbnails. PDF requests use the same sealed bounded snapshot memfd, D06a control channel, one-active-request worker lifecycle, and parent-side bounded PNG reply validation as raster images. PDF remains heavy in the scheduler, so heavy concurrency stays at most one within the total four render slots.

RAW and video thumbnails intentionally remain on the one-shot sandbox path in D08. RAW still needs explicit format-hint and per-job workspace validation across dcraw/ImageMagick/simple_dcraw before reuse can safely avoid stale `/tmp/raw-thumb*` outputs. Video still needs a non-full-file source strategy for large files; D08 does not snapshot arbitrary video inputs into RAM. These exceptions stay sandboxed, consume the same render budget, and are documented rather than granting original host paths or fds to persistent decoders.

## D07 persistent raster pool

Production raster-image misses use the D06b sealed-snapshot worker through a lazy process-wide pool. The pool starts with zero helpers and creates a sandbox only after the existing scheduler admits a visible raster miss. Disk-cache hits and RAM hits do not spawn helpers. Healthy raster helpers are reused across rows, folders, views, and windows; each worker still handles one active request at a time over the D06a protocol and receives only a sealed bounded snapshot memfd, never an original source path or host source fd.

Each helper is owned by a thumbnail supervision thread. Startup, request send/receive, reply validation, and output reading are bounded by the protocol deadlines. Crashes, protocol failures, timeouts, or failed startup retire the affected helper, release the scheduler slot through the normal completion path, and enter a short process-wide spawn backoff so a missing or broken sandbox cannot fan out across the 64-entry queue. Decode failures are classified as job failures and leave the worker reusable. Idle helpers retire back to zero after 30 seconds, and application shutdown asks idle helpers to exit. RAW, video, and previews remain on their previous one-shot routes; before starting one of those incompatible one-shot thumbnail backends, the pool retires one idle persistent helper so migration does not silently add a fifth resident sandbox process outside the four-slot render budget.

D07 retains the D06b decisions and limitations. Sealed snapshots protect the original source from persistent raster workers, but decoder/library state may carry between raster files until the helper retires. The parent still performs bounded PNG decoding for shared-cache entries and validated worker outputs, so transport validation is not a codec-safety claim.

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

S1 is implemented for production raster workers with sealed bounded raster snapshot memfds; no original host source fd or path is sent to the persistent decoder. S2 accepts persistent decoder process reuse after that sealed-input proof and documents the cross-file decoder-state blast radius. S3 remains an accepted exposure for this stage: the parent decodes bounded PNG data from shared-cache entries and validated helper outputs, and those bounds do not prove codec safety.
