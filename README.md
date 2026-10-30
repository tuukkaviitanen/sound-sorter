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

## Published Docker image

Every push to `main` publishes the **upload-sorter** image to
`ghcr.io/<owner>/<repository>:latest`, with an additional `sha-<full-commit-sha>`
tag for a specific revision. Repository names are lowercased. This is the sorter
image, not the upstream DumbDrop web application.

The image supports `linux/amd64` and `linux/arm64`; Docker automatically selects
the matching platform. GitHub Actions builds and tests both platforms in parallel
on native runners (no QEMU), reuses per-platform BuildKit caches, and publishes a
combined manifest only after both builds succeed. Publishing uses the built-in
`GITHUB_TOKEN`; no registry secret is required. GHCR package visibility is managed
separately in GitHub's package settings; make the package public for anonymous
pulls. The Compose configuration above continues to build locally.

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

## Automatic Music Assistant synchronization (optional)

The worker can request a [Music Assistant library sync](https://www.music-assistant.io/api/)
after verified transfers, without another service or Home Assistant automation.
It is disabled by default and makes no network requests while `MA_URL` is empty.

1. Create a long-lived access token for an admin user in Music Assistant under
   **Settings → Profile**.
2. Find the **provider instance IDs** in the settings for your music and audiobook
   local-files providers. These are not their display names. Music Assistant must
   already be configured to read the same destination libraries.
3. Copy `.env.example` to `.env` (or add these entries to an existing `.env`), then
   configure the URL, token and relevant provider IDs:

   ```dotenv
   MA_URL=http://192.168.1.50:8095
   MA_TOKEN=your-long-lived-token
   MA_MUSIC_PROVIDER=your-music-provider-instance-id
   MA_AUDIOBOOK_PROVIDER=your-audiobook-provider-instance-id
   ```

4. Rebuild and restart:

   ```sh
   docker compose up -d --build upload-sorter
   docker compose logs -f upload-sorter
   ```

The URL must be reachable **from the container**. `localhost` refers to that
container, not Home Assistant or the Docker host. A base URL or a URL ending in
`/api` is accepted. HTTPS certificate validation is enabled; HTTP should only be
used on a trusted network because the bearer token is sent with the request.
Redirects are not followed. Keep `.env` private; it is excluded from Git and the
Docker build context. Alternatively, mount a token file read-only or as a Docker
secret and set `MA_TOKEN_FILE` to its path inside the container (this takes
precedence over `MA_TOKEN`). The token is not logged or saved in the pending queue.

### Batching and retries

- Successful transfers queue only the affected configured provider. New cover
  art and other companions in music folders also count; identical duplicates,
  unsupported loose files, and failed transfers do not.
- After `MA_SYNC_DEBOUNCE_SECONDS` (default 30) without further library changes,
  one `music/sync` request includes all queued providers. Media types are left
  unrestricted so audiobook providers can sync too. Leave either provider ID
  empty to skip automatic sync for that library.
- Active upload metadata, `.partial` files anywhere in visible uploads, or files
  still waiting to settle postpone the request. Unreadable metadata defers it
  too. An abandoned upload can therefore delay syncing until resolved.
- Each request has a `MA_SYNC_TIMEOUT_SECONDS` timeout (default 5). HTTP errors,
  authentication failures, timeouts, and API-level errors retain the queue and
  retry with exponential backoff from `MA_SYNC_RETRY_SECONDS` (default 30), capped
  at 300 seconds. A request briefly occupies the single worker, but failure never
  undoes a completed file move.
- Pending provider IDs are stored in `uploads/.sorter/ma-sync.json` and recovered
  after a restart, with a fresh quiet period. Only IDs still configured are
  retried. If changing Music Assistant servers, resolve or clear the old pending
  queue while the sorter is stopped. Do not edit state during processing.
- Success means Music Assistant **accepted the sync request**, not that indexing
  has finished. Retrying after a lost response or crash can issue a duplicate
  request; delivery is not exactly-once. External changes made directly to the
  destination library are not tracked by this feature.

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

The worker uses a synchronous HTTP client with Rustls for optional HTTPS calls,
not an async runtime. Resource usage depends on metadata caches, filesystem
watches, TLS and extraction; development-host measurements are not Pi benchmarks
or worst-case memory bounds.
