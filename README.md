# DumbDrop with automatic media sorting

Start or update both services:

```sh
docker compose up -d --build
docker compose logs -f upload-sorter
```

The sorter also processes files already in `uploads/` at startup. Back up anything
important before enabling it. Only run one sorter against these directories.

## Rules

- `uploads/Album/Disc 1/track.flac` → `music/Album/Disc 1/track.flac`.
  Covers, playlists and other companion files in a music folder follow it.
  Music-folder recognition persists so companions arriving later also follow.
- Loose audio → `music/miscellaneous/`.
- `.m4b`, MP4 media-kind `Audiobook`, and audio tagged with genre `Audiobook` or
  `Spoken Word` → `audiobooks/`, preserving the uploaded relative path.
  Other audio, including untagged MP3s, defaults to music. Metadata is read, not
  rewritten; no online lookup or transcription is performed.
- ZIPs extract into a sibling directory named after the archive (with a numeric
  suffix if occupied), then follow the same rules. The archive is deleted only
  after the extracted directory has been emptied successfully. Unsupported
  contents stay extracted in `uploads/`, and the ZIP is retained. Persistent
  receipts in `uploads/.sorter/` prevent repeated extraction across restarts.
- Identical files at the intended destination are SHA-256 checked and deduplicated.
  Different contents get `name (1).ext`, etc. Existing files are never overwritten.
  Matching folders merge by relative path, applying that policy to each file.
- Every move copies into a temporary destination file, verifies SHA-256, publishes
  without replacing an existing file, and only then removes the source.

## Upload completion

DumbDrop writes `filename.partial`, then renames to the final name when the file
is complete. Partial and hidden files/directories are ignored. Final files must
also remain unchanged for `QUIET_SECONDS` (default 2); scans run every
`POLL_SECONDS` (default 2).

Each completed file is moved independently; source upload directories remain
available for later tracks during the upload. There is no need to wait for the
whole folder upload to finish. Files copied by other means do **not** have this
completion guarantee: use the same `.partial` then rename convention, or increase
the quiet period.

Empty upload directories (including nested empty trees) are removed after
`EMPTY_DIR_GRACE_SECONDS` (default 300 seconds). Any active upload metadata
referencing a directory or its descendants prevents cleanup and resets its timer.
Malformed or unreadable metadata pauses cleanup and resets all timers; abandoned
upload metadata therefore protects its directory until resolved. Timers also
restart when the sorter restarts. Hidden directories, symlinks, internal state,
and `uploads/` itself are never cleaned up; any remaining file blocks removal.

`DUMBDROP_UPLOAD_DIR` (default `/app/uploads`) must match the path used **inside
DumbDrop**, so the sorter can translate paths in `.metadata/` to its own mount.
Cleanup rechecks metadata before removal and uses only non-recursive `rmdir`, so
new files prevent deletion. The grace period protects gaps between uploads, but
cannot guarantee preservation of empty folders during an indefinitely paused
batch with no active upload metadata. ZIP extraction directories still use
immediate cleanup after successful processing.

## ZIP safety and failures

Traversal paths, symlinks, hidden entries, encrypted members and duplicate ZIP
entries are rejected without deleting the archive. Extraction limits default to
20 GiB uncompressed and 10,000 entries per archive, configurable with
`MAX_ZIP_BYTES` / `MAX_ZIP_FILES`. Failures are logged and inputs retained for
retry. Avoid uploading untrusted archives; extracted data and retained originals
both consume disk space. Do not remove `.sorter/` while processing is pending.

## Permissions

Existing upload directories here are root-owned, as created by DumbDrop. The
sorter therefore defaults to root with only `DAC_OVERRIDE` added back after
all capabilities are dropped, to write into the host-owned library directories.
Its root filesystem is read-only; only the three mounted data directories and
`/tmp` are writable. Output files are readable (`0644`).

If you configure DumbDrop and all three data directories for a shared non-root
user, set `SORTER_UID` and `SORTER_GID` in `.env` to that user's IDs; you can also
remove `cap_add` in that setup. Don't change only the sorter user while uploads
remain owned by root.

## Tests

With Python and the dependency in `sorter/requirements.txt` installed:

```sh
cd sorter
python -m unittest -v
```

Or run tests without host Python dependencies:

```sh
docker compose build upload-sorter
docker compose run --rm --no-deps -v "$PWD/sorter/test_sorter.py:/app/test_sorter.py:ro" upload-sorter python -m unittest -v
```

Tests use temporary directories; they do not modify your real uploads or library.
