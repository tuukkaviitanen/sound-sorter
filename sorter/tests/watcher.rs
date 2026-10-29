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
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MA_") {
            command.env_remove(key);
        }
    }
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

#[test]
fn worker_batches_uploads_and_calls_music_assistant() {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
    };
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    fs::create_dir(root.join("uploads")).unwrap();
    fs::write(root.join("uploads/song.wav"), b"music").unwrap();
    fs::write(root.join("uploads/book.m4b"), b"book").unwrap();
    let mut cmd = command(root);
    cmd.env(
        "MA_URL",
        format!("http://{}", listener.local_addr().unwrap()),
    )
    .env("MA_TOKEN", "integration-token")
    .env("MA_MUSIC_PROVIDER", "music")
    .env("MA_AUDIOBOOK_PROVIDER", "books")
    .env("MA_SYNC_DEBOUNCE_SECONDS", "1")
    .env("MA_SYNC_TIMEOUT_SECONDS", "2")
    .env("RECONCILE_SECONDS", "3600");
    let _worker = Worker(cmd.spawn().unwrap());
    let mut connection = None;
    wait_for(|| {
        if let Ok((stream, _)) = listener.accept() {
            connection = Some(stream);
        }
        connection.is_some()
    });
    let mut stream = connection.unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut header = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if line.to_lowercase().starts_with("content-length:") {
            length = line.split(':').nth(1).unwrap().trim().parse().unwrap();
        }
        header.push_str(&line);
    }
    assert!(header.contains("Bearer integration-token"));
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["command"], "music/sync");
    assert_eq!(
        request["args"]["providers"],
        serde_json::json!(["books", "music"])
    );
    let response = r#"{"message_id":"upload-sorter-sync","result":null}"#;
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
    wait_for(|| {
        fs::read_to_string(root.join("uploads/.sorter/ma-sync.json"))
            .ok()
            .as_deref()
            == Some("[]")
    });
    assert!(root.join("music/miscellaneous/song.wav").exists());
    assert!(root.join("books/book.m4b").exists());
}
