use super::*;
use std::io::Cursor;
use tempfile::{TempDir, tempdir};
use zip::{ZipWriter, write::SimpleFileOptions};

struct Fixture {
    _temp: TempDir,
    sorter: Sorter,
    now: Instant,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempdir().unwrap();
        let base = temp.path();
        let config = Config {
            uploads: base.join("uploads"),
            music: base.join("music"),
            books: base.join("audiobooks"),
            dumbdrop_upload_dir: "/app/uploads".into(),
            quiet: Duration::ZERO,
            empty_grace: Duration::from_secs(300),
            reconcile: Duration::from_secs(60),
            max_zip_bytes: 1024 * 1024,
            max_zip_files: 100,
        };
        let sorter = Sorter::new(config).unwrap();
        Self {
            _temp: temp,
            sorter,
            now: Instant::now(),
        }
    }
    fn upload(&self, name: &str, data: &[u8]) -> PathBuf {
        let path = self.sorter.config.uploads.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, data).unwrap();
        path
    }
    fn zip(&self, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        self.upload(name, &zip_data(entries))
    }
    fn scan(&mut self) {
        self.sorter.scan(self.now).unwrap();
    }
    fn cleanup(&mut self, seconds: u64) {
        self.sorter
            .cleanup(self.now + Duration::from_secs(seconds))
            .unwrap();
    }
    fn metadata(&self, text: &str) -> PathBuf {
        self.upload(".metadata/upload.meta", text.as_bytes())
    }
}
fn zip_data(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        z.start_file(*name, SimpleFileOptions::default()).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap().into_inner()
}
#[test]
fn loose_audio_and_books() {
    let mut f = Fixture::new();
    f.upload("song.wav", b"audio");
    f.upload("book.m4b", b"book");
    f.upload("partial.mp3.partial", b"unfinished");
    f.scan();
    assert!(
        f.sorter
            .config
            .music
            .join("miscellaneous/song.wav")
            .exists()
    );
    assert!(f.sorter.config.books.join("book.m4b").exists());
    assert!(f.sorter.config.uploads.join("partial.mp3.partial").exists());
}
#[test]
fn folder_sidecars_and_restart() {
    let mut f = Fixture::new();
    f.upload("Album/Disc 1/song.wav", b"audio");
    f.scan();
    assert!(f.sorter.config.music.join("Album/Disc 1/song.wav").exists());
    assert!(f.sorter.config.uploads.join("Album/Disc 1").exists());
    f.sorter = Sorter::new(f.sorter.config.clone()).unwrap();
    f.upload("Album/cover.jpg", b"cover");
    f.scan();
    assert!(f.sorter.config.music.join("Album/cover.jpg").exists());
}
#[test]
fn duplicates_and_conflicts() {
    let mut f = Fixture::new();
    for bytes in [b"one".as_slice(), b"one", b"two"] {
        f.upload("song.wav", bytes);
        f.scan();
    }
    assert_eq!(
        fs::read(f.sorter.config.music.join("miscellaneous/song.wav")).unwrap(),
        b"one"
    );
    assert_eq!(
        fs::read(f.sorter.config.music.join("miscellaneous/song (1).wav")).unwrap(),
        b"two"
    );
    assert_eq!(
        fs::read_dir(f.sorter.config.music.join("miscellaneous"))
            .unwrap()
            .count(),
        2
    );
}
#[test]
fn zip_success() {
    let mut f = Fixture::new();
    let archive = f.zip(
        "Album.zip",
        &[("song.wav", b"audio"), ("cover.jpg", b"cover")],
    );
    f.scan();
    assert!(!archive.exists());
    assert!(f.sorter.config.music.join("Album/cover.jpg").exists());
}
#[test]
fn nested_zip() {
    let mut f = Fixture::new();
    let bytes = zip_data(&[("book.m4b", b"book")]);
    let archive = f.zip("Outer.zip", &[("Inner.zip", &bytes)]);
    f.scan();
    assert!(!archive.exists());
    assert!(f.sorter.config.books.join("Outer/Inner/book.m4b").exists());
}
#[test]
fn unsupported_zip_retained_across_restart() {
    let mut f = Fixture::new();
    let archive = f.zip("Docs.zip", &[("notes.txt", b"keep")]);
    f.scan();
    f.sorter = Sorter::new(f.sorter.config.clone()).unwrap();
    f.scan();
    assert!(archive.exists());
    assert!(f.sorter.config.uploads.join("Docs/notes.txt").exists());
    assert!(!f.sorter.config.uploads.join("Docs (1)").exists());
}
#[test]
fn reject_traversal_and_zip_limits() {
    let mut f = Fixture::new();
    let archive = f.zip("bad.zip", &[("../escape.wav", b"audio")]);
    assert!(f.sorter.extract(&archive, 0, f.now).is_err());
    assert!(archive.exists());
    assert!(!f.sorter.config.uploads.join("escape.wav").exists());
    let large = f.zip("large.zip", &[("song.wav", b"audio")]);
    f.sorter.config.max_zip_bytes = 1;
    assert!(f.sorter.extract(&large, 0, f.now).is_err());
    f.sorter.config.max_zip_bytes = 100;
    f.sorter.config.max_zip_files = 0;
    assert!(f.sorter.extract(&large, 0, f.now).is_err());
}
#[test]
fn quiet_period_and_changed_file() {
    let mut f = Fixture::new();
    f.sorter.config.quiet = Duration::from_secs(2);
    let path = f.upload("song.wav", b"audio");
    f.scan();
    assert!(path.exists());
    assert!(f.sorter.pending);
    f.now += Duration::from_secs(1);
    fs::write(&path, b"changed audio").unwrap();
    f.scan();
    assert!(path.exists());
    f.now += Duration::from_secs(2);
    f.scan();
    assert!(!path.exists());
}
#[test]
fn metadata_and_archive_caches() {
    let mut f = Fixture::new();
    let path = f.upload("song.mp3", b"invalid but cached");
    assert!(!f.sorter.is_book(&path).unwrap());
    assert_eq!(f.sorter.metadata.len(), 1);
    let old = f.sorter.metadata[&path].0.clone();
    assert!(!f.sorter.is_book(&path).unwrap());
    assert_eq!(f.sorter.metadata[&path].0, old);
    fs::write(&path, b"changed").unwrap();
    f.sorter.is_book(&path).unwrap();
    assert_ne!(f.sorter.metadata[&path].0, old);
    let archive = f.zip("Docs.zip", &[("notes.txt", b"keep")]);
    f.scan();
    assert!(f.sorter.archives.contains_key(&archive));
}
#[test]
fn failed_transfer_keeps_source() {
    let f = Fixture::new();
    let source = f.upload("song.wav", b"audio");
    let blocker = f.sorter.config.music.join("blocker");
    fs::write(&blocker, b"file").unwrap();
    assert!(transfer(&source, &blocker.join("song.wav")).is_err());
    assert!(source.exists());
}
#[test]
fn empty_tree_grace() {
    let mut f = Fixture::new();
    let dir = f.sorter.config.uploads.join("Album/Disc 1");
    fs::create_dir_all(&dir).unwrap();
    f.cleanup(0);
    f.cleanup(299);
    assert!(dir.exists());
    f.cleanup(300);
    assert!(!f.sorter.config.uploads.join("Album").exists());
    assert!(f.sorter.config.uploads.exists());
    assert!(f.sorter.state.exists());
}
#[test]
fn active_metadata_protects_ancestors() {
    let mut f = Fixture::new();
    let dir = f.sorter.config.uploads.join("Album/Disc 1");
    fs::create_dir_all(&dir).unwrap();
    let meta = f.metadata(r#"{"filePath":"/app/uploads/Album/Disc 1/song.wav","partialFilePath":"/app/uploads/Album/Disc 1/song.wav.partial"}"#);
    let other = f.sorter.config.uploads.join("Other");
    fs::create_dir(&other).unwrap();
    f.cleanup(0);
    f.cleanup(1000);
    assert!(dir.exists());
    assert!(!other.exists());
    fs::remove_file(meta).unwrap();
    f.cleanup(1001);
    f.cleanup(1300);
    assert!(dir.exists());
    f.cleanup(1301);
    assert!(!dir.exists());
}
#[test]
fn malformed_metadata_fails_closed() {
    let mut f = Fixture::new();
    let dir = f.sorter.config.uploads.join("Album");
    fs::create_dir(&dir).unwrap();
    f.cleanup(0);
    let mut meta = PathBuf::new();
    for bad in ["{", "{}", r#"{"filePath":"/outside/Album/song.wav"}"#] {
        meta = f.metadata(bad);
        f.cleanup(1000);
        assert!(dir.exists());
        assert!(f.sorter.empty_since.is_empty());
    }
    fs::remove_file(meta).unwrap();
    f.cleanup(1001);
    f.cleanup(1300);
    assert!(dir.exists());
    f.cleanup(1301);
    assert!(!dir.exists());
}
#[test]
fn nonempty_hidden_and_partial_are_kept() {
    let mut f = Fixture::new();
    f.upload("Partial/song.mp3.partial", b"partial");
    f.upload("Hidden/.keep", b"keep");
    f.upload("Unknown/notes.txt", b"keep");
    fs::create_dir_all(f.sorter.config.uploads.join(".private/empty")).unwrap();
    f.cleanup(0);
    f.cleanup(1000);
    for dir in ["Partial", "Hidden", "Unknown", ".private/empty"] {
        assert!(f.sorter.config.uploads.join(dir).exists());
    }
}
#[test]
fn timer_reset_by_files_and_restart() {
    let mut f = Fixture::new();
    let dir = f.sorter.config.uploads.join("Album");
    fs::create_dir(&dir).unwrap();
    f.cleanup(0);
    let file = f.upload("Album/song.partial", b"partial");
    f.cleanup(299);
    fs::remove_file(file).unwrap();
    f.cleanup(300);
    f.cleanup(599);
    assert!(dir.exists());
    f.sorter = Sorter::new(f.sorter.config.clone()).unwrap();
    f.cleanup(600);
    f.cleanup(899);
    assert!(dir.exists());
    f.cleanup(900);
    assert!(!dir.exists());
}
#[cfg(unix)]
#[test]
fn symlinks_never_followed() {
    use std::os::unix::fs::symlink;
    let mut f = Fixture::new();
    let outside = f._temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("song.wav"), b"keep").unwrap();
    symlink(&outside, f.sorter.config.uploads.join("linked")).unwrap();
    f.scan();
    f.cleanup(1000);
    assert!(outside.join("song.wav").exists());
    assert_eq!(fs::read_dir(&f.sorter.config.music).unwrap().count(), 0);
    let source = f.upload("song.wav", b"audio");
    symlink(&outside, f.sorter.config.music.join("linked")).unwrap();
    assert!(transfer(&source, &f.sorter.config.music.join("linked/song.wav")).is_err());
    assert!(source.exists());
}
fn atom(name: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend(name);
    b.extend(body);
    b
}
fn mp4_tag(stik: Option<u8>, genre: Option<&str>) -> Vec<u8> {
    let mut ilst = Vec::new();
    if let Some(kind) = stik {
        let mut data = vec![0, 0, 0, 21, 0, 0, 0, 0];
        data.push(kind);
        ilst.extend(atom(b"stik", &atom(b"data", &data)));
    }
    if let Some(genre) = genre {
        let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0];
        data.extend(genre.as_bytes());
        ilst.extend(atom(b"\xa9gen", &atom(b"data", &data)));
    }
    let mut meta = vec![0; 4];
    meta.extend(atom(b"ilst", &ilst));
    let mut out = atom(b"ftyp", b"M4A \0\0\0\0M4A isom");
    out.extend(atom(b"moov", &atom(b"udta", &atom(b"meta", &meta))));
    out
}
#[test]
fn real_mp4_metadata() {
    let f = Fixture::new();
    for (stik, genre, expected) in [
        (Some(2), None, true),
        (Some(1), None, false),
        (None, Some("Audiobook"), true),
        (None, Some("Rock"), false),
    ] {
        let path = f.upload("book.m4a", &mp4_tag(stik, genre));
        assert_eq!(audiobook(&path), expected, "stik={stik:?}, genre={genre:?}");
    }
}
#[test]
fn real_id3_genre() {
    use lofty::{
        prelude::TagExt,
        tag::{Tag, TagType},
    };
    let f = Fixture::new();
    // A valid empty PCM WAVE, tagged with an ID3 chunk by Lofty.
    let mut wav = b"RIFF".to_vec();
    wav.extend(36u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(44100u32.to_le_bytes());
    wav.extend(88200u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(0u32.to_le_bytes());
    let path = f.upload("spoken.wav", &wav);
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_genre("Spoken Word".into());
    tag.save_to_path(&path, Default::default()).unwrap();
    assert!(audiobook(&path));
}
#[test]
fn python_receipt_compatibility() {
    let mut f = Fixture::new();
    let archive = f.zip("Docs.zip", &[("notes.txt", b"keep")]);
    let target = f.sorter.config.uploads.join("Docs");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("notes.txt"), b"keep").unwrap();
    let receipt = f.sorter.state.join(format!(
        "{:x}.json",
        Sha256::digest(archive.to_string_lossy().as_bytes())
    ));
    save_json(&receipt, &serde_json::json!({"sha256":hash(&archive).unwrap(), "target":target, "staging":f.sorter.state.join("old-staging")})).unwrap();
    f.scan();
    assert!(archive.exists());
    assert!(!f.sorter.config.uploads.join("Docs (1)").exists());
}

#[test]
fn nesting_limit_survives_recovery_scans() {
    let mut f = Fixture::new();
    let mut data = zip_data(&[("book.m4b", b"book")]);
    for _ in 0..6 {
        data = zip_data(&[("Inner.zip", &data)]);
    }
    let archive = f.upload("Outer.zip", &data);
    f.scan();
    f.sorter = Sorter::new(f.sorter.config.clone()).unwrap();
    f.scan();
    assert!(archive.exists());
    assert_eq!(WalkDir::new(&f.sorter.config.books).into_iter().count(), 1);
}

#[test]
fn recovery_from_published_receipt_before_directory_rename() {
    let mut f = Fixture::new();
    let archive = f.zip("Album.zip", &[("song.wav", b"audio")]);
    let staging = f.sorter.state.join("extract-interrupted");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("song.wav"), b"audio").unwrap();
    let receipt = f.sorter.state.join(format!(
        "{:x}.json",
        Sha256::digest(archive.to_string_lossy().as_bytes())
    ));
    save_json(
        &receipt,
        &Receipt {
            sha256: hash(&archive).unwrap(),
            target: f.sorter.config.uploads.join("Album"),
            staging: Some(staging),
        },
    )
    .unwrap();
    f.scan();
    assert!(!archive.exists());
    assert!(f.sorter.config.music.join("Album/song.wav").exists());
}

#[test]
fn real_vorbis_comment_genre() {
    use lofty::{
        prelude::TagExt,
        tag::{Tag, TagType},
    };
    let f = Fixture::new();
    let mut flac = b"fLaC".to_vec();
    flac.extend([0x80, 0, 0, 34]); // Last block: STREAMINFO.
    flac.extend([0; 34]);
    let path = f.upload("spoken.flac", &flac);
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.set_genre("Audiobook".into());
    tag.save_to_path(&path, Default::default()).unwrap();
    assert!(audiobook(&path));
}
