# DumbDrop with automatic media sorting

A small Rust worker sorts completed DumbDrop uploads into a music and audiobook
library. It uses native filesystem notifications, a single processing worker,
and periodic recovery scans. No Python, async runtime, or external media tools
are needed in the runtime container.

## Run on a Raspberry Pi

Use a **64-bit OS** on the Pi 5. Both Docker build stages support ARM64 (and
AMD64); building directly on the Pi selects its native architecture.

```sh
docker compose up -d --build
docker compose logs -f upload-sorter
```

The first build downloads the Rust toolchain and compiles dependencies. The
multi-stage build uses mise and limits compilation to two parallel jobs; none
of the toolchain or build dependencies are included in the runtime image.

The sorter also processes files already in `uploads/` at startup. Back up
important files before enabling it. A lock prevents multiple Rust workers from
processing the same uploads directory concurrently.

### Upgrading from the Python worker

```sh
docker compose stop upload-sorter
docker compose up -d --build upload-sorter
```

Existing library paths, `uploads/.sorter/music-folders.json`, and Python ZIP
receipts are preserved and understood by the Rust worker. Keep the same mounted
paths when upgrading; ZIP receipts contain absolute container paths. Do not run
the old Python worker alongside the Rust worker: Python doesn't use the lock.
`POLL_SECONDS` is replaced by `RECONCILE_SECONDS`.

## Sorting rules

- `uploads/Album/Disc 1/track.flac` → `music/Album/Disc 1/track.flac`.
  Covers, playlists and other companion files in a music folder follow it.
  Recognition persists so companions arriving later follow too.
- Loose music → `music/miscellaneous/`.
- `.m4b`, MP4 media-kind `Audiobook` (`stik=2`), and audio with genre
  `Audiobook` or `Spoken Word` → `audiobooks/`, preserving the uploaded path.
  Other audio, including untagged MP3s, defaults to music.
- Metadata is read locally, never rewritten. Lofty reads ID3, MP4, Vorbis
  comments and other supported tags; embedded artwork and audio properties are
  not loaded for classification. Unsupported metadata formats (notably WMA/ASF)
  or unreadable tags default to music. Use `.m4b` for reliable audiobook routing
  if your source format lacks supported tags.
- ZIPs extract into a sibling directory named after the archive (with a numeric
  suffix if occupied), then follow the same rules. The ZIP is deleted only when
  all extracted contents have been handled successfully. Unsupported contents
  stay extracted in `uploads/`, and the ZIP is retained. Receipts in `.sorter/`
  prevent repeated extraction across restarts.
- Identical files at the intended destination are SHA-256 checked and deduplicated.
  Different contents get `name (1).ext`, etc. Matching folders merge by relative
  path, applying this policy to each file. Files are never overwritten.
- Each move copies into a temporary destination file, verifies SHA-256, syncs
  and publishes without overwriting, and only then removes the source. Source
  and destination can be on separate filesystems. The destination must support
  hard links for atomic publication; otherwise the source is retained and an
  error logged. Local ext4 is a suitable choice.

## Completion, watching and resource usage

DumbDrop writes `filename.partial`, then renames it when the file is complete.
Partial files, hidden paths, and symlinks are ignored. Final files must also
remain unchanged for `QUIET_SECONDS` (default 2).

Native notifications wake the worker when something changes. A bounded event
channel coalesces bursts. Reads made by the sorter don't trigger work, and it
ignores its internal state events. It scans at startup and every
`RECONCILE_SECONDS` (default 60) to recover from missed notifications. If the
watcher cannot start, recovery scans still work. This is also the fallback for
network mounts whose servers don't generate local filesystem events.

Scans walk the upload tree, but audio classification and ZIP checksums are cached
until file size, modification time or file identity changes. Completed files
are processed sequentially with bounded copy/hash buffers. When idle, the
worker waits for an event or its next recovery scan, rather than polling every
two seconds. Files awaiting their quiet period are checked more frequently.

Each completed file moves independently, leaving its upload directory available
for later tracks. Files copied by other means do **not** have DumbDrop's completion
guarantee: use the same `.partial` then rename convention, or increase the quiet
period.

### Empty-directory cleanup

Empty directories, including nested empty trees, are removed after
`EMPTY_DIR_GRACE_SECONDS` (default 300 seconds). Cleanup runs during scans, so
an idle directory may wait up to one additional recovery interval.

Active metadata referencing a directory or its descendants prevents removal and
resets its timer. Unreadable or malformed metadata pauses cleanup and resets all
timers. Abandoned upload metadata therefore protects its directory until
resolved. Timers also restart with the worker. Hidden directories, symlinks,
internal state, and `uploads/` itself are never cleaned up; any remaining file
blocks removal.

`DUMBDROP_UPLOAD_DIR` (default `/app/uploads`) must match the path used **inside
DumbDrop**, so metadata paths can be translated to the sorter's mount. Cleanup
rechecks metadata before removal and uses non-recursive `rmdir`, so new files
prevent deletion. A grace period cannot protect an indefinitely paused batch
with no active upload metadata. Private ZIP extraction directories are pruned
immediately after successful processing.

## ZIP safety and failures

Traversal paths, symlinks, hidden entries, encrypted members and duplicate paths
are rejected without deleting the archive. Default limits are 20 GiB uncompressed,
10,000 entries per archive, and five extraction levels. Size and entry limits are
configurable with `MAX_ZIP_BYTES` and `MAX_ZIP_FILES`. Failures are logged and
inputs retained for retry. Avoid uploading untrusted archives: extracted data
and retained originals both consume disk space. Don't remove `.sorter/` while
processing is pending.

## Permissions

DumbDrop currently creates root-owned upload directories. The sorter defaults
to root, dropping all capabilities except `DAC_OVERRIDE` to write into host-owned
library directories. Its root filesystem is read-only; only the three data
mounts and `/tmp` are writable. Output files are readable (`0644`).

If DumbDrop and all data directories use a shared non-root user, set
`SORTER_UID` and `SORTER_GID` in `.env`; you can also remove `cap_add`. Don't change
only the sorter user while uploads remain owned by root.

## Development with mise

Install [mise](https://mise.jdx.dev/), then:

```sh
mise trust
mise install rust
mise run test       # Unit tests and real-process watcher integration tests
mise run check      # Formatting and Clippy (warnings treated as errors)
mise run build      # Optimized executable: sorter/target/release/upload-sorter
```

`mise.toml` pins Rust; `sorter/Cargo.lock` pins Rust library dependencies. Docker
uses the same mise configuration. Cargo manages crates under the mise-managed
toolchain; OS build packages are installed only in the Docker builder stage.
Tests use temporary directories, never your real uploads or library.

The optimized executable measured approximately 1.8 MiB, with about 3.3 MiB
resident memory in a brief empty-library smoke test on the development AMD64
host. This is not a Pi benchmark or a worst-case memory bound; metadata caches,
filesystem watches, and extraction all affect usage.
