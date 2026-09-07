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

The existing one-shot pipeline remains: lookup and rendering run in
`gio::spawn_blocking`, successful PNG data is decoded by the parent before
application, and persistence is best effort. These meanings must be preserved
when later diffs split the pipeline.

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

## Open decisions

S1 (source protection), S2 (persistent decoder state), and S3 (cache/helper
codec trust boundary) remain open. D00 records evidence and can proceed
independently; no persistent worker or unapproved source-FD capability is
introduced by this diff. The proposed architecture and transport/concurrency
choices remain in the local implementation plan until their respective diffs
are reviewed.
