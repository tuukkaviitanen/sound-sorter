use anyhow::{Context, Result};
use fs2::FileExt;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::{
    fs::OpenOptions,
    sync::mpsc,
    time::{Duration, Instant},
};
use upload_sorter::{Config, Sorter, ma_sync::MaConfig};

fn main() -> Result<()> {
    let config = Config::from_env()?;
    let mut sorter = Sorter::new(config)?;
    let config = sorter.config.clone();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config.uploads.join(".sorter/worker.lock"))?;
    lock.try_lock_exclusive()
        .context("Another sorter is already running")?;
    sorter.enable_sync(MaConfig::from_env()?, Instant::now())?;
    // Capacity one coalesces bursts without an unbounded event queue.
    let (tx, rx) = mpsc::sync_channel(1);
    // Keep the channel connected even if watcher initialization fails.
    let _keepalive = tx.clone();
    let root = config.uploads.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        match event {
            Ok(event) => {
                // Reading tags and hashes must not trigger another scan.
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                if !event.paths.is_empty()
                    && event.paths.iter().all(|p| {
                        p.strip_prefix(&root)
                            .ok()
                            .and_then(|r| r.components().next())
                            .is_some_and(|c| c.as_os_str() == ".sorter")
                    })
                {
                    return;
                }
                let _ = tx.try_send(());
            }
            Err(error) => {
                eprintln!("Watch error; recovery scan requested: {error}");
                let _ = tx.try_send(());
            }
        }
    });
    if let Ok(watcher) = &mut watcher {
        if let Err(error) = watcher.watch(&config.uploads, RecursiveMode::Recursive) {
            eprintln!("Cannot watch uploads; using periodic scans: {error}");
        }
    } else {
        eprintln!("Cannot initialize file watcher; using periodic scans");
    }
    eprintln!(
        "Watching {}; recovery scans every {}s",
        config.uploads.display(),
        config.reconcile.as_secs()
    );
    let mut last_scan = Instant::now();
    if let Err(error) = sorter.scan(last_scan) {
        eprintln!("Scan failed: {error:#}");
    }
    loop {
        let wait = if sorter.pending {
            config
                .quiet
                .clamp(Duration::from_millis(50), Duration::from_secs(2))
        } else {
            config
                .reconcile
                .saturating_sub(last_scan.elapsed())
                .max(Duration::from_millis(1))
        };
        let wait = sorter
            .sync_wait(Instant::now())
            .map_or(wait, |sync| wait.min(sync));
        let event = rx.recv_timeout(wait).is_ok();
        while rx.try_recv().is_ok() {}
        let now = Instant::now();
        if event
            || sorter.pending
            || now.duration_since(last_scan) >= config.reconcile
            || sorter.sync_wait(now) == Some(Duration::ZERO)
        {
            if let Err(error) = sorter.scan(now) {
                eprintln!("Scan failed; will retry: {error:#}");
                sorter.defer_sync(now);
            } else {
                sorter.sync_if_due(Instant::now());
            }
            last_scan = now;
        }
    }
}
