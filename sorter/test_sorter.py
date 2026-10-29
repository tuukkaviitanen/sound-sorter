import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from sorter import Sorter, audiobook


class SorterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        base = Path(self.temp.name)
        self.s = Sorter(base / "uploads", base / "music", base / "audiobooks", quiet=0)

    def upload(self, name, data=b"test audio"):
        path = self.s.uploads / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def archive(self, name, entries):
        path = self.s.uploads / name
        with zipfile.ZipFile(path, "w") as z:
            for key, value in entries.items():
                z.writestr(key, value)
        return path

    def test_loose_music_and_book(self):
        self.upload("track.wav")
        self.upload("book.m4b")
        self.upload("unfinished.mp3.partial")
        self.s.tick()
        self.assertTrue((self.s.music / "miscellaneous/track.wav").exists())
        self.assertTrue((self.s.books / "book.m4b").exists())
        self.assertTrue((self.s.uploads / "unfinished.mp3.partial").exists())

    def test_folder_and_late_sidecar_survive_restart(self):
        self.upload("Album/Disc 1/track.wav")
        self.s.tick()
        self.assertTrue((self.s.music / "Album/Disc 1/track.wav").exists())
        self.assertTrue((self.s.uploads / "Album/Disc 1").is_dir())
        self.upload("Album/cover.jpg")
        self.s = Sorter(self.s.uploads, self.s.music, self.s.books, quiet=0)
        self.s.tick()
        self.assertTrue((self.s.music / "Album/cover.jpg").exists())

    def test_duplicates_and_conflicts(self):
        self.upload("song.wav", b"first")
        self.s.tick()
        self.upload("song.wav", b"first")
        self.s.tick()
        self.assertEqual(len(list((self.s.music / "miscellaneous").iterdir())), 1)
        self.upload("song.wav", b"second")
        self.s.tick()
        self.assertEqual((self.s.music / "miscellaneous/song (1).wav").read_bytes(), b"second")
        self.assertEqual((self.s.music / "miscellaneous/song.wav").read_bytes(), b"first")

    def test_zip_success(self):
        archive = self.archive("Album.zip", {"track.wav": b"audio", "cover.jpg": b"cover"})
        self.s.tick()
        self.assertFalse(archive.exists())
        self.assertTrue((self.s.music / "Album/track.wav").exists())
        self.assertTrue((self.s.music / "Album/cover.jpg").exists())

    def test_unsupported_retained_and_not_extracted_twice(self):
        archive = self.archive("Docs.zip", {"notes.txt": b"keep"})
        self.s.tick()
        self.s.tick()
        self.assertTrue(archive.exists())
        self.assertTrue((self.s.uploads / "Docs/notes.txt").exists())
        self.assertFalse((self.s.uploads / "Docs (1)").exists())

    def test_nested_zip(self):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w") as nested:
            nested.writestr("book.m4b", b"book")
        archive = self.archive("Outer.zip", {"Inner.zip": buffer.getvalue()})
        self.s.tick()
        self.assertFalse(archive.exists())
        self.assertTrue((self.s.books / "Outer/Inner/book.m4b").exists())

    def test_copy_failure_keeps_source(self):
        source = self.upload("song.wav")
        with patch("sorter.shutil.copyfileobj", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.s.transfer(source, self.s.music / "song.wav")
        self.assertTrue(source.exists())
        self.assertFalse((self.s.music / "song.wav").exists())
        self.assertEqual(list(self.s.music.iterdir()), [])

    def test_unsafe_zip_and_size_limit(self):
        archive = self.archive("bad.zip", {"../escape.wav": b"bad"})
        with self.assertRaises(ValueError):
            self.s.extract(archive)
        self.assertFalse((self.s.uploads.parent / "escape.wav").exists())
        self.assertTrue(archive.exists())
        self.s.max_zip_bytes = 1
        archive = self.archive("large.zip", {"track.wav": b"too large"})
        with self.assertRaises(ValueError):
            self.s.extract(archive)

    def test_quiet_period(self):
        self.s.quiet = 60
        path = self.upload("track.wav")
        self.s.tick()
        self.assertTrue(path.exists())

    def test_metadata(self):
        class Audio:
            tags = {"TCON": "Audiobook"}
        with patch("sorter.AudioFile", return_value=Audio()):
            self.assertTrue(audiobook(Path("book.mp3")))
        Audio.tags = {"stik": [2]}
        with patch("sorter.AudioFile", return_value=Audio()):
            self.assertTrue(audiobook(Path("book.m4a")))
        Audio.tags = {"genre": ["Rock"]}
        with patch("sorter.AudioFile", return_value=Audio()):
            self.assertFalse(audiobook(Path("music.flac")))

    def metadata(self, record):
        directory = self.s.uploads / ".metadata"
        directory.mkdir(exist_ok=True)
        path = directory / "upload.meta"
        path.write_text(json.dumps(record))
        return path

    def cleanup_at(self, now):
        with patch("sorter.time.monotonic", return_value=now):
            self.s.cleanup_empty_directories()

    def test_empty_tree_cleanup_after_grace(self):
        directory = self.s.uploads / "Album/Disc 1"
        directory.mkdir(parents=True)
        self.cleanup_at(0)
        self.cleanup_at(299)
        self.assertTrue(directory.exists())
        self.cleanup_at(300)
        self.assertFalse((self.s.uploads / "Album").exists())
        self.assertTrue(self.s.uploads.exists())
        self.assertTrue(self.s.state.exists())

    def test_active_upload_protects_empty_ancestors(self):
        directory = self.s.uploads / "Album/Disc 1"
        directory.mkdir(parents=True)
        metadata = self.metadata({"filePath": "/app/uploads/Album/Disc 1/song.wav",
                                  "partialFilePath": "/app/uploads/Album/Disc 1/song.wav.partial"})
        unrelated = self.s.uploads / "Unrelated"
        unrelated.mkdir()
        self.cleanup_at(0)
        self.cleanup_at(1000)
        self.assertTrue(directory.exists())
        self.assertFalse(unrelated.exists())
        metadata.unlink()
        self.cleanup_at(1001)
        self.cleanup_at(1300)
        self.assertTrue(directory.exists())
        self.cleanup_at(1301)
        self.assertFalse((self.s.uploads / "Album").exists())

    def test_bad_metadata_pauses_and_resets_grace(self):
        directory = self.s.uploads / "Album"
        directory.mkdir()
        self.cleanup_at(0)
        metadata = self.metadata({"filePath": "/app/uploads/Album/song.wav"})
        for invalid in ("{", "{}", '{"filePath": "/outside/Album/song.wav"}'):
            metadata.write_text(invalid)
            self.cleanup_at(1000)
            self.assertTrue(directory.exists())
            self.assertEqual(self.s.empty_since, {})
        metadata.unlink()
        self.cleanup_at(1001)
        self.cleanup_at(1300)
        self.assertTrue(directory.exists())
        self.cleanup_at(1301)
        self.assertFalse(directory.exists())

    def test_nonempty_partial_and_hidden_directories_are_kept(self):
        self.upload("Partial/song.wav.partial")
        self.upload("Hidden/.keep")
        self.upload("Unknown/notes.txt")
        (self.s.uploads / ".private/empty").mkdir(parents=True)
        self.cleanup_at(0)
        self.cleanup_at(1000)
        for name in ("Partial", "Hidden", "Unknown", ".private/empty"):
            self.assertTrue((self.s.uploads / name).exists())

    def test_new_file_resets_empty_timer(self):
        directory = self.s.uploads / "Album"
        directory.mkdir()
        self.cleanup_at(0)
        path = self.upload("Album/song.wav.partial")
        self.cleanup_at(299)
        path.unlink()
        self.cleanup_at(300)
        self.cleanup_at(599)
        self.assertTrue(directory.exists())
        self.cleanup_at(600)
        self.assertFalse(directory.exists())

    def test_restart_starts_fresh_grace(self):
        directory = self.s.uploads / "Album"
        directory.mkdir()
        self.cleanup_at(0)
        self.s = Sorter(self.s.uploads, self.s.music, self.s.books, quiet=0)
        self.cleanup_at(1000)
        self.assertTrue(directory.exists())
        self.cleanup_at(1300)
        self.assertFalse(directory.exists())

    def test_metadata_rechecked_before_removal(self):
        directory = self.s.uploads / "Album"
        directory.mkdir()
        self.cleanup_at(0)
        with patch.object(self.s, "active_upload_directories", side_effect=[set(), {directory}]):
            self.cleanup_at(300)
        self.assertTrue(directory.exists())
        self.assertNotIn(directory, self.s.empty_since)

    def test_new_file_at_removal_is_not_deleted(self):
        directory = self.s.uploads / "Album"
        directory.mkdir()
        self.cleanup_at(0)
        calls = []
        def active():
            calls.append(True)
            if len(calls) == 2:
                self.upload("Album/new.wav.partial")
            return set()
        with patch.object(self.s, "active_upload_directories", side_effect=active):
            self.cleanup_at(300)
        self.assertTrue((directory / "new.wav.partial").exists())

    def test_symlink_not_followed(self):
        outside = Path(self.temp.name) / "outside"
        outside.mkdir()
        (outside / "track.wav").write_bytes(b"keep")
        (self.s.uploads / "linked").symlink_to(outside, target_is_directory=True)
        self.s.tick()
        self.assertTrue((outside / "track.wav").exists())
        self.assertEqual(list(self.s.music.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
