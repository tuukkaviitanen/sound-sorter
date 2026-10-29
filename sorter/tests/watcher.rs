use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !predicate() {
        assert!(Instant::now() < deadline, "Timed out waiting for sorter");
        thread::sleep(Duration::from_millis(50));
    }
}
fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_upload-sorter"));
    command
        .env("UPLOADS_DIR", root.join("uploads"))
        .env("MUSIC_DIR", root.join("music"))
        .env("AUDIOBOOKS_DIR", root.join("books"))
        .env("QUIET_SECONDS", "1")
        .env("RECONCILE_SECONDS", "1")
        .env("EMPTY_DIR_GRACE_SECONDS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
#[test]
fn finalized_uploads_cleanup_and_exclusive_worker() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let uploads = root.join("uploads");
    fs::create_dir_all(uploads.join("Album")).unwrap();
    fs::write(uploads.join("Album/song.wav.partial"), b"in progress").unwrap();
    let _worker = Worker(command(root).spawn().unwrap());
    wait_for(|| uploads.join(".sorter/worker.lock").exists());
    // Give the worker time to acquire its lock, then ensure a second worker exits.
    thread::sleep(Duration::from_millis(200));
    let status = command(root).status().unwrap();
    assert!(!status.success());
    thread::sleep(Duration::from_millis(1200));
    assert!(uploads.join("Album/song.wav.partial").exists());
    assert!(!root.join("music/Album/song.wav").exists());
    fs::rename(
        uploads.join("Album/song.wav.partial"),
        uploads.join("Album/song.wav"),
    )
    .unwrap();
    wait_for(|| root.join("music/Album/song.wav").exists());
    wait_for(|| !uploads.join("Album").exists());
    assert_eq!(
        fs::read(root.join("music/Album/song.wav")).unwrap(),
        b"in progress"
    );
    assert!(uploads.exists());
    assert!(uploads.join(".sorter").exists());
}
#[test]
fn native_notification_triggers_scan_before_reconciliation() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let mut cmd = command(root);
    cmd.env("RECONCILE_SECONDS", "3600");
    let _worker = Worker(cmd.spawn().unwrap());
    wait_for(|| root.join("uploads/.sorter/worker.lock").exists());
    thread::sleep(Duration::from_millis(200));
    fs::write(root.join("uploads/book.m4b.partial"), b"book").unwrap();
    fs::rename(
        root.join("uploads/book.m4b.partial"),
        root.join("uploads/book.m4b"),
    )
    .unwrap();
    wait_for(|| root.join("books/book.m4b").exists());
}
