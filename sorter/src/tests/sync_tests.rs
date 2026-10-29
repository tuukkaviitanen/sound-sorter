use super::*;
use crate::ma_sync::MaConfig;
fn enabled() -> Fixture {
    let mut f = Fixture::new();
    let mut config = MaConfig::new(
        "http://127.0.0.1:1",
        "test-token".into(),
        Some("music".into()),
        Some("books".into()),
    )
    .unwrap();
    config.retry = Duration::from_secs(7);
    f.sorter.enable_sync(Some(config), f.now).unwrap();
    f
}
fn queued(f: &Fixture) -> Vec<String> {
    let path = f.sorter.state.join("ma-sync.json");
    if path.exists() {
        serde_json::from_reader(File::open(path).unwrap()).unwrap()
    } else {
        Vec::new()
    }
}
#[test]
fn successful_routes_queue_only_affected_providers() {
    let mut f = enabled();
    f.upload("song.wav", b"music");
    f.scan();
    assert_eq!(queued(&f), ["music"]);
    f.upload("book.m4b", b"book");
    f.scan();
    assert_eq!(queued(&f), ["books", "music"]);
    // The pending-sync JSON must not be mistaken for a ZIP receipt on future scans.
    f.scan();
}
#[test]
fn duplicates_unknown_files_and_failures_dont_queue_sync() {
    let mut f = enabled();
    let destination = f.sorter.config.music.join("miscellaneous");
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.join("song.wav"), b"same").unwrap();
    f.upload("song.wav", b"same");
    f.upload("notes.txt", b"unknown");
    f.scan();
    assert!(queued(&f).is_empty());
    // A file where a destination directory is required makes publication fail.
    fs::write(f.sorter.config.music.join("Album"), b"blocker").unwrap();
    f.upload("Album/song.wav", b"keep");
    f.scan();
    assert!(f.sorter.config.uploads.join("Album/song.wav").exists());
    assert!(queued(&f).is_empty());
}
#[test]
fn zip_and_late_sidecars_queue_music() {
    let mut f = enabled();
    f.zip(
        "Album.zip",
        &[("song.wav", b"audio"), ("cover.jpg", b"cover")],
    );
    f.scan();
    assert_eq!(queued(&f), ["music"]);
    f.upload("Album/new-cover.jpg", b"new cover");
    f.scan();
    assert_eq!(queued(&f), ["music"]);
}
#[test]
fn active_metadata_partial_and_bad_metadata_defer_requests() {
    for mode in ["active", "partial", "invalid"] {
        let mut f = enabled();
        f.upload("song.wav", b"music");
        f.scan();
        match mode {
            "active" => {
                f.metadata(r#"{"filePath":"/app/uploads/Album/next.wav"}"#);
            }
            "partial" => {
                f.upload("Album/next.wav.partial", b"partial");
            }
            _ => {
                f.metadata("{");
            }
        }
        let due = f.now + Duration::from_secs(30);
        f.sorter.sync_if_due(due);
        // HTTP failures would schedule 7s, so 30s proves upload-state deferral.
        assert_eq!(f.sorter.sync_wait(due), Some(Duration::from_secs(30)));
        assert_eq!(queued(&f), ["music"]);
    }
}
#[test]
fn disabled_sync_has_no_queue_or_deadline() {
    let mut f = Fixture::new();
    f.upload("song.wav", b"music");
    f.scan();
    assert!(queued(&f).is_empty());
    assert_eq!(f.sorter.sync_wait(f.now), None);
}
#[test]
fn pending_sync_survives_worker_restart() {
    let mut f = enabled();
    f.upload("book.m4b", b"book");
    f.scan();
    f.sorter = Sorter::new(f.sorter.config.clone()).unwrap();
    let cfg = MaConfig::new(
        "http://127.0.0.1:1",
        "new-token".into(),
        None,
        Some("books".into()),
    )
    .unwrap();
    f.sorter.enable_sync(Some(cfg), f.now).unwrap();
    assert_eq!(queued(&f), ["books"]);
    assert_eq!(f.sorter.sync_wait(f.now), Some(Duration::from_secs(30)));
}
