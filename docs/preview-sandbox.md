# Preview sandbox

Strata treats files shown while browsing as untrusted. Native parsers do not receive the user's normal filesystem or network access.

## Sandboxed providers

The following providers run in a short-lived helper process:

- GDK Pixbuf image and camera RAW loaders;
- Poppler PDF thumbnail and page rendering;
- ImageMagick and `dcraw`/`simple_dcraw` RAW fallbacks; and
- `ffmpegthumbnailer` media thumbnails.

Image previews are normalized to PNG by the helper. Video previews are limited to the first 30 seconds, at most 1280 pixels on either axis, and at most 30 frames per second. Hardware acceleration is enabled by default except when an unset preference is paired with an AMD Polaris GPU; those systems start with software previews but can opt in from General settings. Automatic mode tries VA-API, then Vulkan, then the software VP8 fallback. A selected VA-API or Vulkan backend falls directly back to software if it fails. Hardware paths produce H.264/AAC MP4 with both dimensions aligned to 16 pixels; the software path produces unchanged VP8/Opus WebM output. This keeps GStreamer from parsing the selected untrusted file directly. Plain-text previews remain in-process and are limited to 1 MB; they do not invoke a native format parser.

Thumbnail rendering uses a four-slot scheduler, limits RAW/PDF/video heavy work to one of those slots, and queues at most 64 unique requests. Raster image and PDF misses use a lazy persistent worker pool after the parent copies the source into a sealed bounded snapshot memfd. RAW and video thumbnails plus previews remain on their existing one-shot helper paths. Live rows deferred by a full queue are retried as capacity opens, duplicate requests share one render, rows leaving the view cancel queued work that has no remaining targets, and failed renders are cached for 30 seconds to prevent retry loops.

## Isolation and limits

Strata starts its own executable in a bubblewrap sandbox. The sandbox has:

- a new user, mount, PID, IPC, UTS, cgroup, and network namespace;
- read-only access to `/usr`, required runtime libraries and font/ImageMagick configuration, the Strata executable, and exactly one canonicalized input file;
- writable access only to private mode-0700 output and temporary directories;
- an empty environment with a nonexistent home directory;
- a 512 MB input limit for raster and PDF parsing, a 2 GB address-space limit allowing modern image loaders to start their isolated worker threads, a 512 MB sandbox file-size limit for decoder buffers, and a 32 MB parent-side output limit;
- a 12-second wall-clock limit for image, PDF, and thumbnail rendering, plus a 10-second CPU limit; and
- a 30-second wall-clock limit for media previews, which have no cumulative CPU limit because FFmpeg uses multiple threads. Hardware attempts are limited to 8 seconds each and 12 seconds collectively so the software fallback retains time to run.

Accelerated media previews receive only the devices required by their policy: VA-API gets safe `/dev/dri/renderD<digits>` nodes, while Vulkan and Automatic may also get `/dev/nvidia<digits>` and `/dev/nvidiactl`. They receive read-only `/sys` access for driver discovery. Software media previews, image, PDF, and thumbnail helpers receive no GPU devices or `/sys` mount.

Strata reads PCI vendor and device IDs from `/sys/class/drm/renderD*/device` to detect AMD Polaris 10, 11, and 12 devices (`0x67c0–0x67df`, `0x67e0–0x67ff`, and `0x6980–0x699f`). Because preview encoding can hang on them, an unset acceleration preference resolves to software when any Polaris render node is present. The settings remain available as an explicit opt-in, after which the selected hardware policy receives the render node normally. Nodes with unreadable metadata retain the non-Polaris default.

GPU acceleration expands the media helper's attack surface into the installed userspace and kernel GPU drivers; policy-specific device access keeps that exposure media-only and the existing namespaces and resource limits still apply.

External thumbnail providers have bounded stdout and discarded stderr. The parent accepts only a size- and dimension-bounded PNG, MP4 with an `ftyp` signature, or WebM with an EBML signature. Failed or unavailable hardware attempts advance to the next backend, while a failed final software attempt produces the normal unavailable-preview result. Cancellation or timeout kills the renderer process group and bubblewrap, whose PID namespace also tears down descendants that create a new process group. A missing bubblewrap installation, renderer crash, malformed result, timeout, or permission failure is fail-closed and produces the normal fallback icon or **Preview unavailable** message.

## Persistent thumbnail protocol proof

The protocol module introduced for D06a defines, documents, and tests the persistent thumbnail worker control plane. D06b proved one reusable raster worker, D07 routes production raster-image misses through a lazy supervised pool, and D08 adds PDF thumbnails to that pool using the same sealed-snapshot protocol. Production RAW/video thumbnails and previews still use one-shot helpers.

Control packets are exactly 48 bytes on a `SOCK_SEQPACKET` Unix socketpair created with `CLOEXEC` and `NONBLOCK`. The parent should map the helper side through ordinary `Stdio::from(OwnedFd)` when a helper seam is added, rather than passing path or URI job capabilities or using broad unsafe fd-inheritance policy. The packet envelope contains:

- magic `STTP` and wire version `1`;
- message type (`Ready`, `Request`, or `Reply`);
- nonzero request ID for requests/replies;
- operation (`ThumbnailPng` initially) and requested edge, bounded to 1–256 px;
- status (`Ok`, bounded decode failure, unsupported operation, or protocol failure);
- declared output representation (`Png` initially), width, height, stride, and byte length.

Startup expects a readiness/version packet within the two-second startup deadline. Each worker has at most one active request. Parent-side sessions include a worker generation and reject mismatched generations, duplicate active requests, unsolicited replies, wrong reply IDs, and out-of-order packets. Unsupported opcodes and decode failures are bounded job failures and may leave the loop usable; malformed framing, bad versions, descriptor-contract failures, and ordering violations are protocol-contract failures and retire the worker.

Successful replies carry one sealed regular memfd, never a large inline PNG/raw frame. The parent validates ancillary truncation, descriptor count, `MSG_CMSG_CLOEXEC`, regular-file status, required seals (`SEAL`, `SHRINK`, `GROW`, and `WRITE`), `fstat` length, the 4 MiB thumbnail output cap, dimensions, representation, stride metadata, and checked allocation size before reading. Unexpected descriptors are owned and dropped on rejection paths. Send/receive paths handle `EINTR`, nonblocking `EAGAIN` as bounded wait failure, peer closure, and `MSG_NOSIGNAL` send behavior.

D06b's approved source-protection strategy is a sealed bounded raster snapshot memfd. The parent opens the source with `CLOEXEC` and `NONBLOCK`, verifies it is a regular file within the raster input cap, copies it into an anonymous memfd, detects size/mtime changes during the copy, and applies `SEAL`, `SHRINK`, `GROW`, and `WRITE` before the worker receives it. The persistent raster worker receives that sealed snapshot fd by `SCM_RIGHTS`; it does not receive a path, URI, original source fd, or writable output directory. The worker decodes through `/proc/self/fd/<snapshot-fd>`, replies with a sealed output memfd, and remains reusable after bounded decode failures.

Persistent decoder process reuse after the sealed-input proof is accepted. This intentionally improves worker reuse but means decoder/library state can carry from one raster file to the next until the worker exits for protocol failure, crash, timeout, resource retirement, idle policy, or application shutdown. This is a documented blast-radius tradeoff, not per-file decoder-state isolation.

The persistent pool starts with zero helpers and spawns only for admitted image/PDF misses. Disk-cache hits do not create a sandbox. Startup and each request/reply are supervised by the worker owner thread with bounded protocol deadlines. Decode failures are bounded job failures and leave the helper reusable; crash, timeout, protocol failure, failed startup, or shutdown retire the helper. Helpers idle out after 30 seconds. Repeated startup failure enters process-wide backoff so missing bubblewrap or namespace support cannot create a spawn storm. Before an incompatible one-shot thumbnail backend starts, one idle persistent helper is retired so the migration period remains within the same total render budget.

RAW remains one-shot until format hints and unique per-job dcraw/ImageMagick/simple_dcraw workspaces are validated for persistent reuse. Video remains one-shot until a source strategy avoids full-file RAM staging for large media. Both exceptions stay sandboxed and accounted in D08.

S3 remains an accepted exposure for this scope: the parent still performs bounded PNG decoding for shared-cache entries and validated worker outputs. Header, dimension, byte, fd, and seal checks bound allocations and transport behavior, but they do not prove the PNG codec is safe or sandbox cache decoding. Raw replies or sandboxed cache decoding remain possible follow-up designs, not D06b behavior.
