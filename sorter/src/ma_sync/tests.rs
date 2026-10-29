use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    thread,
};
use tempfile::tempdir;

fn config(url: &str) -> MaConfig {
    MaConfig::new(
        url,
        "secret-test-token".into(),
        Some("local-music".into()),
        Some("local-books".into()),
    )
    .unwrap()
}
fn server(status: u16, body: &str) -> (String, thread::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = body.to_string();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(s) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "No HTTP request received");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "POST /api HTTP/1.1\r\n");
        let mut length = 0;
        let mut authorized = false;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            let lower = line.to_lowercase();
            if lower.starts_with("content-length:") {
                length = line.split(':').nth(1).unwrap().trim().parse().unwrap();
            }
            if lower.starts_with("authorization:") {
                authorized = line.trim_end() == "authorization: Bearer secret-test-token"
                    || line.trim_end() == "Authorization: Bearer secret-test-token";
            }
        }
        assert!(authorized);
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).unwrap();
        let request: Value = serde_json::from_slice(&bytes).unwrap();
        write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    (url, handle)
}
#[test]
fn validates_configuration() {
    for bad in [
        "ftp://host",
        "http://user:password@host",
        "http://host?token=secret",
        "http://host/#fragment",
        "not a url",
    ] {
        assert!(MaConfig::new(bad, "token".into(), Some("music".into()), None).is_err());
    }
    assert!(MaConfig::new("http://host", "".into(), Some("music".into()), None).is_err());
    assert!(MaConfig::new("http://host", "token".into(), None, None).is_err());
    assert_eq!(
        config("http://host:8095/api/").endpoint,
        "http://host:8095/api"
    );
}
#[test]
fn batches_changes_and_persists_without_token() {
    let dir = tempdir().unwrap();
    let state = dir.path().join("ma-sync.json");
    let now = Instant::now();
    let mut sync = MusicAssistant::new(config("http://localhost:8095"), &state, now).unwrap();
    assert_eq!(sync.wait(now), None);
    sync.mark(false, now).unwrap();
    sync.mark(false, now + Duration::from_secs(20)).unwrap();
    sync.mark(true, now + Duration::from_secs(25)).unwrap();
    assert!(!sync.due(now + Duration::from_secs(54)));
    assert!(sync.due(now + Duration::from_secs(55)));
    assert_eq!(sync.providers.len(), 2);
    let text = std::fs::read_to_string(&state).unwrap();
    assert!(!text.contains("secret"));
    let loaded = MusicAssistant::new(config("http://localhost:8095"), &state, now).unwrap();
    assert_eq!(loaded.providers, sync.providers);
}
#[test]
fn sends_scoped_request_and_clears_queue() {
    let (url, handle) = server(200, r#"{"message_id":"upload-sorter-sync","result":null}"#);
    let dir = tempdir().unwrap();
    let state = dir.path().join("ma-sync.json");
    let now = Instant::now();
    let mut sync = MusicAssistant::new(config(&url), &state, now).unwrap();
    sync.mark(false, now).unwrap();
    sync.mark(true, now).unwrap();
    sync.send(now + Duration::from_secs(30)).unwrap();
    let request = handle.join().unwrap();
    assert_eq!(request["command"], "music/sync");
    assert_eq!(
        request["args"]["providers"],
        json!(["local-books", "local-music"])
    );
    assert!(request["args"].get("media_types").is_none());
    assert!(sync.providers.is_empty());
    let stored: BTreeSet<String> = serde_json::from_reader(File::open(state).unwrap()).unwrap();
    assert!(stored.is_empty());
}
#[test]
fn errors_retain_queue_and_retry_with_backoff() {
    let dir = tempdir().unwrap();
    let state = dir.path().join("ma-sync.json");
    let now = Instant::now();
    let mut sync = MusicAssistant::new(config("http://127.0.0.1:1"), &state, now).unwrap();
    sync.mark(false, now).unwrap();
    for (offset, delay) in [(30, 30), (60, 60), (120, 120), (240, 240), (480, 300)] {
        let current = now + Duration::from_secs(offset);
        assert!(sync.send(current).is_err());
        assert_eq!(sync.wait(current), Some(Duration::from_secs(delay)));
        assert_eq!(sync.providers.len(), 1);
    }
    let (url, handle) = server(200, r#"{"message_id":"upload-sorter-sync","result":null}"#);
    sync.config.endpoint = format!("{url}/api");
    sync.send(now + Duration::from_secs(800)).unwrap();
    handle.join().unwrap();
    assert!(sync.providers.is_empty());
}
#[test]
fn api_error_malformed_body_and_http_failure_are_retried() {
    for (status, body) in [
        (
            200,
            r#"{"message_id":"upload-sorter-sync","error_code":5,"details":"secret"}"#,
        ),
        (200, "not JSON"),
        (200, r#"{"result":null}"#),
        (401, "Unauthorized"),
        (302, "redirect"),
    ] {
        let (url, handle) = server(status, body);
        let dir = tempdir().unwrap();
        let now = Instant::now();
        let mut sync =
            MusicAssistant::new(config(&url), &dir.path().join("queue.json"), now).unwrap();
        sync.mark(false, now).unwrap();
        let error = sync.send(now).unwrap_err();
        assert!(!format!("{error:#}").contains("secret"));
        handle.join().unwrap();
        assert_eq!(sync.providers.len(), 1);
    }
}
#[test]
fn timeout_retains_queue() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = config(&format!("http://{}", listener.local_addr().unwrap()));
    cfg.timeout = Duration::from_millis(50);
    let dir = tempdir().unwrap();
    let now = Instant::now();
    let mut sync = MusicAssistant::new(cfg, &dir.path().join("queue.json"), now).unwrap();
    sync.mark(false, now).unwrap();
    let start = Instant::now();
    assert!(sync.send(now).is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(sync.providers.len(), 1);
}
#[test]
fn missing_provider_is_not_queued() {
    let dir = tempdir().unwrap();
    let now = Instant::now();
    let mut cfg = config("http://host");
    cfg.audiobook_provider = None;
    let mut sync = MusicAssistant::new(cfg, &dir.path().join("queue.json"), now).unwrap();
    sync.mark(true, now).unwrap();
    assert_eq!(sync.wait(now), None);
}
