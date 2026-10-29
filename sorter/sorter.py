"""Sort finalized DumbDrop uploads without moving directories still being uploaded."""
import hashlib
import json
import logging
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import tempfile
import time
import zipfile

from mutagen import File as AudioFile

LOG = logging.getLogger("sorter")
AUDIO = {".mp3", ".flac", ".wav", ".wave", ".m4a", ".m4b", ".aac", ".ogg", ".opus", ".aiff", ".aif", ".wma", ".ape", ".alac"}


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def audiobook(path):
    if path.suffix.lower() == ".m4b":
        return True
    try:
        audio = AudioFile(path)
        tags = audio.tags if audio is not None else None
        if not tags:
            return False
        for key, value in tags.items():
            key = str(key).lower()
            # MP4 stik=2 is Audiobook. Genre tags support ID3, Vorbis and ASF.
            if key == "stik" and 2 in value:
                return True
            if key in {"tcon", "genre", "wm/genre", "©gen", "----:com.apple.itunes:genre"}:
                text = str(value).lower().replace("-", "").replace(" ", "")
                if "audiobook" in text or "spokenword" in text:
                    return True
    except Exception as exc:
        LOG.warning("Cannot read metadata for %s: %s (defaulting to music)", path, exc)
    return False


class Sorter:
    def __init__(self, uploads, music, books, quiet=2, max_zip_bytes=20 * 1024**3, max_zip_files=10000,
                 empty_dir_grace=300, dumbdrop_upload_dir="/app/uploads"):
        self.uploads, self.music, self.books = map(Path, (uploads, music, books))
        self.quiet = quiet
        self.max_zip_bytes, self.max_zip_files = max_zip_bytes, max_zip_files
        self.state = self.uploads / ".sorter"
        for directory in (self.uploads, self.music, self.books, self.state):
            directory.mkdir(parents=True, exist_ok=True)
        self.observed = {}
        self.empty_since = {}
        self.empty_dir_grace = max(0, empty_dir_grace)
        self.dumbdrop_upload_dir = Path(dumbdrop_upload_dir)
        self.music_index = self.state / "music-folders.json"
        self.music_folders = set(json.loads(self.music_index.read_text())) if self.music_index.exists() else set()

    def ready(self, path):
        info = path.stat()
        signature = (info.st_size, info.st_mtime_ns)
        old = self.observed.get(path)
        now = time.monotonic()
        if old is None or old[0] != signature:
            self.observed[path] = (signature, now)
            return self.quiet == 0
        return now - old[1] >= self.quiet

    def transfer(self, source, destination):
        """Copy, verify, publish without overwrite, then remove the source."""
        destination.parent.mkdir(parents=True, exist_ok=True)
        # Refuse symlink components, even if someone modified the library externally.
        if any(p.is_symlink() for p in (destination, *destination.parents)):
            raise ValueError(f"Symlink destination: {destination}")
        def source_signature():
            info = source.stat()
            return info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns

        before = source_signature()
        checksum = digest(source)
        candidate, number = destination, 1
        while candidate.exists():
            if candidate.is_file() and not candidate.is_symlink() and digest(candidate) == checksum:
                if source_signature() != before:
                    raise RuntimeError(f"Source changed: {source}")
                source.unlink()
                LOG.info("Removed identical duplicate %s", source)
                return
            candidate = destination.with_name(f"{destination.stem} ({number}){destination.suffix}")
            number += 1
        fd, name = tempfile.mkstemp(prefix=".sorter-", dir=destination.parent)
        temp = Path(name)
        try:
            with os.fdopen(fd, "wb") as output, source.open("rb") as incoming:
                shutil.copyfileobj(incoming, output)
                output.flush()
                os.fsync(output.fileno())
            if digest(temp) != checksum or source_signature() != before:
                raise RuntimeError(f"Source changed or verification failed: {source}")
            os.chmod(temp, 0o644)
            os.link(temp, candidate)  # Atomic no-clobber publication, including across mounts.
            source.unlink()
            LOG.info("Moved %s -> %s", source, candidate)
        finally:
            temp.unlink(missing_ok=True)

    def extract(self, archive, depth=0):
        if depth >= 5:
            raise ValueError("ZIP nesting limit reached")
        checksum = digest(archive)
        receipt = self.state / (hashlib.sha256(str(archive).encode()).hexdigest() + ".json")
        if receipt.exists():
            record = json.loads(receipt.read_text())
            if record["sha256"] != checksum:
                raise ValueError(f"Archive changed while extraction is pending: {archive}")
            target = Path(record["target"])
        else:
            target = archive.with_suffix("")
            count = 1
            while target.exists():
                target = archive.with_name(f"{archive.stem} ({count})")
                count += 1
            staging = Path(tempfile.mkdtemp(prefix="extract-", dir=self.state))
            try:
                with zipfile.ZipFile(archive) as z:
                    members = z.infolist()
                    if len(members) > self.max_zip_files or sum(i.file_size for i in members) > self.max_zip_bytes:
                        raise ValueError("ZIP exceeds configured extraction limits")
                    seen = set()
                    for item in members:
                        name = PurePosixPath(item.filename)
                        mode = item.external_attr >> 16
                        if (not name.parts or name.is_absolute() or ".." in name.parts
                                or "\\" in item.filename or ":" in item.filename
                                or any(p.startswith(".") for p in name.parts)
                                or stat.S_ISLNK(mode) or item.flag_bits & 1):
                            raise ValueError(f"Unsafe or unsupported ZIP member: {item.filename}")
                        if item.filename in seen:
                            raise ValueError("Duplicate ZIP member")
                        seen.add(item.filename)
                    z.extractall(staging)
                # Save recovery receipt before publication, so a crash cannot re-extract twice.
                record = {"sha256": checksum, "target": str(target), "staging": str(staging)}
                temp_receipt = receipt.with_suffix(".tmp")
                temp_receipt.write_text(json.dumps(record))
                temp_receipt.replace(receipt)
                staging.rename(target)
            finally:
                if not receipt.exists():
                    shutil.rmtree(staging, ignore_errors=True)
        if not target.exists() and record.get("staging") and Path(record["staging"]).exists():
            Path(record["staging"]).rename(target)
        if target.exists():
            self.process_tree(target, depth + 1, prune=True)
        if not target.exists():
            if digest(archive) != checksum:
                raise RuntimeError("Archive changed during processing")
            archive.unlink()
            receipt.unlink()
            LOG.info("Deleted successfully processed ZIP %s", archive)

    def process_tree(self, root, depth=0, prune=False):
        files = [p for p in root.rglob("*") if p.is_file() and not p.is_symlink()
                 and not any(part.startswith(".") for part in p.relative_to(root).parts)
                 and not any(parent.is_symlink() for parent in p.parents)]
        music_files = [p for p in files if p.suffix.lower() in AUDIO and not audiobook(p)]
        # Mark music folders and descendants to preserve covers, playlists and other extras.
        music_roots = {p.relative_to(self.uploads).parts[0] for p in music_files}
        if not music_roots.issubset(self.music_folders):
            self.music_folders.update(music_roots)
            temp_index = self.music_index.with_suffix(".tmp")
            temp_index.write_text(json.dumps(sorted(self.music_folders)))
            temp_index.replace(self.music_index)
        music_roots = self.music_folders
        for path in sorted(files):
            try:
                if not path.exists() or path.name.endswith(".partial") or not self.ready(path):
                    continue
                relative = path.relative_to(self.uploads)
                suffix = path.suffix.lower()
                if suffix == ".zip":
                    self.extract(path, depth)
                elif suffix in AUDIO:
                    if audiobook(path):
                        destination = self.books / relative
                    else:
                        destination = self.music / (relative if len(relative.parts) > 1 else Path("miscellaneous") / path.name)
                    self.transfer(path, destination)
                elif relative.parts[0] in music_roots and len(relative.parts) > 1:
                    self.transfer(path, self.music / relative)
            except Exception:
                LOG.exception("Left in uploads for retry: %s", path)
        # Ordinary upload directories use delayed, metadata-aware cleanup.
        # Extracted directories are private to this ZIP and can be pruned now.
        if prune:
            for directory in sorted((p for p in root.rglob("*") if p.is_dir() and not p.is_symlink()), reverse=True):
                try:
                    directory.rmdir()
                except OSError:
                    pass
            try:
                root.rmdir()
            except OSError:
                pass

    def active_upload_directories(self):
        """Translate DumbDrop's container paths; unreadable metadata fails closed."""
        active = set()
        metadata = self.uploads / ".metadata"
        if not metadata.exists():
            return active
        for entry in metadata.iterdir():
            if entry.is_symlink() or not entry.is_file():
                raise ValueError(f"Unexpected upload metadata entry: {entry}")
            record = json.loads(entry.read_text())
            paths = [record[key] for key in ("filePath", "partialFilePath") if key in record]
            if not paths:
                raise ValueError(f"Missing upload paths in {entry}")
            for value in paths:
                relative = Path(value).relative_to(self.dumbdrop_upload_dir)
                if ".." in relative.parts:
                    raise ValueError(f"Invalid upload path in {entry}")
                path = self.uploads / relative
                active.update(path.parents)
        return active

    def cleanup_empty_directories(self):
        try:
            active = self.active_upload_directories()
        except (OSError, ValueError, TypeError):
            LOG.warning("Pausing empty-directory cleanup: unreadable upload metadata")
            self.empty_since.clear()
            return

        directories = []
        def scan_error(error):
            raise error

        try:
            for parent, names, _ in os.walk(self.uploads, followlinks=False, onerror=scan_error):
                names[:] = [name for name in names if not name.startswith(".")
                            and not (Path(parent) / name).is_symlink()]
                directories.extend(Path(parent) / name for name in names)
        except OSError:
            LOG.warning("Pausing empty-directory cleanup: unable to scan uploads")
            self.empty_since.clear()
            return

        now = time.monotonic()
        empty = set()
        expired = []
        for directory in sorted(directories, key=lambda p: len(p.parts), reverse=True):
            try:
                # An empty tree qualifies as a unit, so nested folders don't each
                # add another grace period. Hidden files and symlinks block cleanup.
                if directory in active or any(child not in empty for child in directory.iterdir()):
                    continue
                empty.add(directory)
                info = directory.stat()
                signature = (info.st_dev, info.st_ino, info.st_mtime_ns)
                old = self.empty_since.get(directory)
                if old is None or old[0] != signature:
                    self.empty_since[directory] = (signature, now)
                if now - self.empty_since[directory][1] >= self.empty_dir_grace:
                    expired.append(directory)
            except OSError:
                continue
        self.empty_since = {p: value for p, value in self.empty_since.items() if p in empty}
        for directory in expired:
            try:
                # Recheck metadata immediately before removal, not only at scan start.
                if directory in self.active_upload_directories():
                    self.empty_since.pop(directory, None)
                    continue
            except (OSError, ValueError, TypeError):
                LOG.warning("Pausing empty-directory cleanup: upload metadata changed")
                self.empty_since.clear()
                return
            try:
                directory.rmdir()  # Never recursive: new files prevent deletion.
                self.empty_since.pop(directory, None)
                LOG.info("Removed empty upload directory %s", directory)
            except OSError:
                self.empty_since.pop(directory, None)

    def tick(self):
        # Do not recurse into internal metadata/staging directories.
        for child in sorted(self.uploads.iterdir()):
            if child.name.startswith(".") or child.is_symlink():
                continue
            if child.is_dir():
                self.process_tree(child)
        # Process loose files separately so music folder detection stays scoped.
        for child in sorted(self.uploads.iterdir()):
            if child.is_file() and not child.name.startswith(".") and not child.is_symlink():
                try:
                    if child.name.endswith(".partial") or not self.ready(child):
                        continue
                    if child.suffix.lower() == ".zip":
                        self.extract(child)
                    elif child.suffix.lower() in AUDIO:
                        self.transfer(child, (self.books if audiobook(child) else self.music / "miscellaneous") / child.name)
                except Exception:
                    LOG.exception("Left in uploads for retry: %s", child)
        self.observed = {p: value for p, value in self.observed.items() if p.exists()}
        self.cleanup_empty_directories()


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    sorter = Sorter(os.getenv("UPLOADS_DIR", "/data/uploads"), os.getenv("MUSIC_DIR", "/data/music"),
                    os.getenv("AUDIOBOOKS_DIR", "/data/audiobooks"), float(os.getenv("QUIET_SECONDS", "2")),
                    int(os.getenv("MAX_ZIP_BYTES", str(20 * 1024**3))), int(os.getenv("MAX_ZIP_FILES", "10000")),
                    float(os.getenv("EMPTY_DIR_GRACE_SECONDS", "300")),
                    os.getenv("DUMBDROP_UPLOAD_DIR", "/app/uploads"))
    while True:
        try:
            sorter.tick()
        except Exception:
            LOG.exception("Scan failed; retrying")
        time.sleep(float(os.getenv("POLL_SECONDS", "2")))


if __name__ == "__main__":
    main()
