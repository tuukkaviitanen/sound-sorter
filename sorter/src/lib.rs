use anyhow::{Context, Result, ensure};
use lofty::{
    config::ParseOptions,
    file::{AudioFile, TaggedFileExt},
    mp4::{AtomData, AtomIdent, Mp4File},
    prelude::Accessor,
    probe::Probe,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

#[derive(Clone)]
pub struct Config {
    pub uploads: PathBuf,
    pub music: PathBuf,
    pub books: PathBuf,
    pub dumbdrop_upload_dir: PathBuf,
    pub quiet: Duration,
    pub empty_grace: Duration,
    pub reconcile: Duration,
    pub max_zip_bytes: u64,
    pub max_zip_files: usize,
}
impl Config {
    pub fn from_env() -> Result<Self> {
        fn path(key: &str, default: &str) -> PathBuf {
            std::env::var_os(key)
                .map(PathBuf::from)
                .unwrap_or_else(|| default.into())
        }
        fn number(key: &str, default: u64) -> Result<u64> {
            std::env::var(key).map_or(Ok(default), |s| {
                s.parse().with_context(|| format!("Invalid {key}"))
            })
        }
        Ok(Self {
            uploads: path("UPLOADS_DIR", "/data/uploads"),
            music: path("MUSIC_DIR", "/data/music"),
            books: path("AUDIOBOOKS_DIR", "/data/audiobooks"),
            dumbdrop_upload_dir: path("DUMBDROP_UPLOAD_DIR", "/app/uploads"),
            quiet: Duration::from_secs(number("QUIET_SECONDS", 2)?),
            empty_grace: Duration::from_secs(number("EMPTY_DIR_GRACE_SECONDS", 300)?),
            reconcile: Duration::from_secs(number("RECONCILE_SECONDS", 60)?.max(1)),
            max_zip_bytes: number("MAX_ZIP_BYTES", 20 * 1024 * 1024 * 1024)?,
            max_zip_files: number("MAX_ZIP_FILES", 10000)? as usize,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Signature {
    len: u64,
    modified: SystemTime,
    identity: (u64, u64, i64, i64),
}
fn signature(path: &Path) -> Result<Signature> {
    let m = fs::symlink_metadata(path)?;
    ensure!(!m.file_type().is_symlink(), "Symlink: {}", path.display());
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
    };
    #[cfg(not(unix))]
    let identity = (0, 0, 0, 0);
    Ok(Signature {
        len: m.len(),
        modified: m.modified()?,
        identity,
    })
}
fn hash(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|s| s.to_string_lossy().starts_with('.'))
}
fn extension(path: &Path) -> String {
    path.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase()
}
fn audio(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "mp3"
            | "flac"
            | "wav"
            | "wave"
            | "m4a"
            | "m4b"
            | "aac"
            | "ogg"
            | "opus"
            | "aiff"
            | "aif"
            | "wma"
            | "ape"
            | "alac"
    )
}
fn spoken(text: &str) -> bool {
    let s = text.to_lowercase().replace([' ', '-'], "");
    s.contains("audiobook") || s.contains("spokenword")
}

pub fn audiobook(path: &Path) -> bool {
    if extension(path) == "m4b" {
        return true;
    }
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    // Keep the typed MP4 tag: generic tags do not expose the binary stik atom.
    if matches!(extension(path).as_str(), "m4a" | "alac")
        && let Ok(mut f) = File::open(path)
        && let Ok(mp4) = Mp4File::read_from(&mut f, options)
    {
        if let Some(tag) = mp4.ilst() {
            if let Some(atom) = tag.get(&AtomIdent::Fourcc(*b"stik"))
                && atom.data().any(|value| match value {
                    AtomData::SignedInteger(2) | AtomData::UnsignedInteger(2) => true,
                    AtomData::Unknown { data, .. } => data.as_slice() == [2],
                    _ => false,
                })
            {
                return true;
            }
            if tag.genre().is_some_and(|g| spoken(&g)) {
                return true;
            }
        }
        return false;
    }
    match Probe::open(path).and_then(|p| p.options(options).read()) {
        Ok(file) => file
            .tags()
            .iter()
            .any(|tag| tag.genre().is_some_and(|g| spoken(&g))),
        Err(error) => {
            eprintln!(
                "Cannot read metadata for {}: {error}; defaulting to music",
                path.display()
            );
            false
        }
    }
}

fn safe_destination(path: &Path) -> Result<()> {
    for parent in path.ancestors() {
        match fs::symlink_metadata(parent) {
            Ok(m) => ensure!(
                !m.file_type().is_symlink(),
                "Symlink destination: {}",
                parent.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn unique(path: &Path, number: usize) -> PathBuf {
    if number == 0 {
        return path.into();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{stem} ({number}){ext}"))
}
pub fn transfer(source: &Path, destination: &Path) -> Result<()> {
    safe_destination(destination)?;
    fs::create_dir_all(destination.parent().context("No destination parent")?)?;
    let before = signature(source)?;
    let checksum = hash(source)?;
    let mut number = 0;
    loop {
        let target = unique(destination, number);
        if fs::symlink_metadata(&target).is_ok() {
            safe_destination(&target)?;
            if target.is_file() && hash(&target)? == checksum {
                ensure!(signature(source)? == before, "Source changed");
                fs::remove_file(source)?;
                eprintln!("Removed identical duplicate {}", source.display());
                return Ok(());
            }
            number += 1;
            continue;
        }
        let mut temp = NamedTempFile::new_in(destination.parent().unwrap())?;
        io::copy(&mut File::open(source)?, &mut temp)?;
        temp.as_file().sync_all()?;
        ensure!(
            hash(temp.path())? == checksum && signature(source)? == before,
            "Source changed or verification failed"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        match fs::hard_link(temp.path(), &target) {
            Ok(()) => {
                File::open(target.parent().unwrap())?.sync_all()?;
                fs::remove_file(source)?;
                eprintln!("Moved {} -> {}", source.display(), target.display());
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                number += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}
fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut temp = NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(&mut temp, value)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    sha256: String,
    target: PathBuf,
    staging: Option<PathBuf>,
}

pub struct Sorter {
    pub config: Config,
    state: PathBuf,
    music_folders: HashSet<String>,
    observed: HashMap<PathBuf, (Signature, Instant)>,
    metadata: HashMap<PathBuf, (Signature, bool)>,
    archives: HashMap<PathBuf, (Signature, String)>,
    empty_since: HashMap<PathBuf, (Signature, Instant)>,
    pub pending: bool,
}
impl Sorter {
    pub fn new(mut config: Config) -> Result<Self> {
        for path in [&config.uploads, &config.music, &config.books] {
            safe_destination(path)?;
            fs::create_dir_all(path)?;
        }
        config.uploads = fs::canonicalize(&config.uploads)?;
        let state = config.uploads.join(".sorter");
        safe_destination(&state)?;
        fs::create_dir_all(&state)?;
        let index = state.join("music-folders.json");
        let music_folders = if index.exists() {
            serde_json::from_reader(File::open(index)?)?
        } else {
            HashSet::new()
        };
        Ok(Self {
            config,
            state,
            music_folders,
            observed: HashMap::new(),
            metadata: HashMap::new(),
            archives: HashMap::new(),
            empty_since: HashMap::new(),
            pending: false,
        })
    }
    fn ready(&mut self, path: &Path, now: Instant) -> Result<bool> {
        let sig = signature(path)?;
        let record = self
            .observed
            .entry(path.into())
            .or_insert((sig.clone(), now));
        if record.0 != sig {
            *record = (sig, now);
        }
        let ready = now.duration_since(record.1) >= self.config.quiet;
        self.pending |= !ready;
        Ok(ready)
    }
    fn is_book(&mut self, path: &Path) -> Result<bool> {
        let sig = signature(path)?;
        if let Some((old, book)) = self.metadata.get(path)
            && *old == sig
        {
            return Ok(*book);
        }
        let book = audiobook(path);
        self.metadata.insert(path.into(), (sig, book));
        Ok(book)
    }
    fn files(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let mut extraction_roots = HashSet::new();
        for entry in fs::read_dir(&self.state)? {
            let path = entry?.path();
            if extension(&path) == "json" && path.file_name().unwrap() != "music-folders.json" {
                let receipt: Receipt = serde_json::from_reader(File::open(path)?)?;
                extraction_roots.insert(receipt.target);
            }
        }
        let mut files = Vec::new();
        // Pending ZIP trees are visited only through their owning archive. This
        // preserves nesting limits and retry ordering across recovery scans.
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0 || (!hidden(e.path()) && !extraction_roots.contains(e.path()))
            })
        {
            let entry = entry?;
            if entry.file_type().is_file() && extension(entry.path()) != "partial" {
                files.push(entry.into_path());
            }
        }
        files.sort();
        Ok(files)
    }
    pub fn scan(&mut self, now: Instant) -> Result<()> {
        self.pending = false;
        self.process_tree(&self.config.uploads.clone(), 0, now)?;
        self.observed.retain(|path, _| path.exists());
        self.metadata.retain(|path, _| path.exists());
        self.archives.retain(|path, _| path.exists());
        self.cleanup(now)?;
        Ok(())
    }
    fn process_tree(&mut self, root: &Path, depth: usize, now: Instant) -> Result<()> {
        let files = self.files(root)?;
        let mut changed = false;
        for path in &files {
            if audio(path) && !self.is_book(path)? {
                let relative = path.strip_prefix(&self.config.uploads)?;
                if relative.components().count() > 1 {
                    changed |= self.music_folders.insert(
                        relative
                            .components()
                            .next()
                            .unwrap()
                            .as_os_str()
                            .to_string_lossy()
                            .into(),
                    );
                }
            }
        }
        if changed {
            save_json(&self.state.join("music-folders.json"), &self.music_folders)?;
        }
        for path in files {
            if !path.exists() {
                continue;
            }
            if let Err(error) = self.process_file(&path, depth, now) {
                eprintln!("Left in uploads for retry: {}: {error:#}", path.display());
            }
        }
        Ok(())
    }
    fn process_file(&mut self, path: &Path, depth: usize, now: Instant) -> Result<()> {
        if !self.ready(path, now)? {
            return Ok(());
        }
        let relative = path.strip_prefix(&self.config.uploads)?;
        if extension(path) == "zip" {
            return self.extract(path, depth, now);
        }
        let destination = if audio(path) {
            if self.is_book(path)? {
                self.config.books.join(relative)
            } else if relative.components().count() == 1 {
                self.config.music.join("miscellaneous").join(relative)
            } else {
                self.config.music.join(relative)
            }
        } else if relative.components().count() > 1
            && self.music_folders.contains(
                &relative
                    .components()
                    .next()
                    .unwrap()
                    .as_os_str()
                    .to_string_lossy()
                    .to_string(),
            )
        {
            self.config.music.join(relative)
        } else {
            return Ok(());
        };
        transfer(path, &destination)
    }
    fn extract(&mut self, archive: &Path, depth: usize, now: Instant) -> Result<()> {
        ensure!(depth < 5, "ZIP nesting limit reached");
        let sig = signature(archive)?;
        let checksum = match self.archives.get(archive) {
            Some((old, hash)) if *old == sig => hash.clone(),
            _ => {
                let checksum = hash(archive)?;
                self.archives
                    .insert(archive.into(), (sig.clone(), checksum.clone()));
                checksum
            }
        };
        let receipt_path = self.state.join(format!(
            "{:x}.json",
            Sha256::digest(archive.to_string_lossy().as_bytes())
        ));
        let record: Receipt = if receipt_path.exists() {
            let record: Receipt = serde_json::from_reader(File::open(&receipt_path)?)?;
            ensure!(
                record.sha256 == checksum,
                "Archive changed while extraction pending"
            );
            ensure!(
                record.target.parent() == archive.parent() && record.target != archive,
                "Invalid extraction receipt"
            );
            safe_destination(&record.target)?;
            record
        } else {
            let target_base = archive.with_extension("");
            let mut target = target_base.clone();
            let mut n = 1;
            while fs::symlink_metadata(&target).is_ok() {
                target = unique(&target_base, n);
                n += 1;
            }
            let staging = tempfile::Builder::new()
                .prefix("extract-")
                .tempdir_in(&self.state)?;
            let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
            ensure!(
                zip.len() <= self.config.max_zip_files,
                "Too many ZIP entries"
            );
            let mut size = 0u64;
            let mut seen = HashSet::new();
            for i in 0..zip.len() {
                let file = zip.by_index(i)?;
                size = size.checked_add(file.size()).context("ZIP size overflow")?;
                ensure!(
                    size <= self.config.max_zip_bytes,
                    "ZIP exceeds extraction limit"
                );
                let name = file.name()?;
                ensure!(
                    !name.contains(['\\', ':'])
                        && !name.starts_with('/')
                        && !name.split('/').any(|s| s == ".." || s.starts_with('.')),
                    "Unsafe ZIP path: {name}"
                );
                let enclosed = file.enclosed_name().context("Unsafe ZIP path")?;
                ensure!(
                    !enclosed.as_os_str().is_empty() && seen.insert(enclosed),
                    "Duplicate or empty ZIP entry"
                );
                ensure!(
                    file.unix_mode().is_none_or(|m| m & 0o170000 != 0o120000),
                    "ZIP symlink"
                );
            }
            for i in 0..zip.len() {
                let mut file = zip.by_index(i)?;
                let output = staging
                    .path()
                    .join(file.enclosed_name().context("Unsafe ZIP path")?);
                if file.is_dir() {
                    fs::create_dir_all(output)?;
                    continue;
                }
                fs::create_dir_all(output.parent().unwrap())?;
                let mut out = File::create(&output)?;
                let expected = file.size();
                let actual = io::copy(&mut (&mut file).take(expected.saturating_add(1)), &mut out)?;
                ensure!(actual == expected, "ZIP member size mismatch");
                out.sync_all()?;
            }
            ensure!(
                signature(archive)? == sig,
                "Archive changed during extraction"
            );
            let record = Receipt {
                sha256: checksum.clone(),
                target,
                staging: Some(staging.path().into()),
            };
            save_json(&receipt_path, &record)?;
            let kept = staging.keep();
            fs::rename(kept, &record.target)?;
            record
        };
        if !record.target.exists()
            && let Some(staging) = &record.staging
        {
            ensure!(
                staging.parent() == Some(self.state.as_path()),
                "Invalid staging receipt"
            );
            safe_destination(staging)?;
            if staging.exists() {
                fs::rename(staging, &record.target)?;
            }
        }
        if record.target.exists() {
            self.process_tree(&record.target, depth + 1, now)?;
            // Only private extraction directories are eligible for immediate pruning.
            let mut dirs: Vec<_> = WalkDir::new(&record.target)
                .follow_links(false)
                .into_iter()
                .collect::<std::result::Result<Vec<_>, _>>()?;
            dirs.sort_by_key(|e| std::cmp::Reverse(e.depth()));
            for entry in dirs {
                if entry.file_type().is_dir() {
                    let _ = fs::remove_dir(entry.path());
                }
            }
        }
        if !record.target.exists() {
            ensure!(
                signature(archive)? == sig,
                "Archive changed during processing"
            );
            fs::remove_file(archive)?;
            fs::remove_file(receipt_path)?;
            eprintln!("Deleted successfully processed ZIP {}", archive.display());
        }
        Ok(())
    }
    fn active_directories(&self) -> Result<HashSet<PathBuf>> {
        let mut active = HashSet::new();
        let dir = self.config.uploads.join(".metadata");
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(active),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            ensure!(entry.file_type()?.is_file(), "Unexpected metadata entry");
            let record: serde_json::Value = serde_json::from_reader(File::open(entry.path())?)?;
            let mut found = false;
            for key in ["filePath", "partialFilePath"] {
                if let Some(value) = record.get(key) {
                    let path = Path::new(value.as_str().context("Invalid metadata path")?);
                    let relative = path.strip_prefix(&self.config.dumbdrop_upload_dir)?;
                    ensure!(
                        relative
                            .components()
                            .all(|c| matches!(c, Component::Normal(_))),
                        "Unsafe metadata path"
                    );
                    let translated = self.config.uploads.join(relative);
                    active.extend(translated.ancestors().skip(1).map(Path::to_path_buf));
                    found = true;
                }
            }
            ensure!(found, "Missing metadata paths");
        }
        Ok(active)
    }
    pub fn cleanup(&mut self, now: Instant) -> Result<()> {
        let active = match self.active_directories() {
            Ok(active) => active,
            Err(e) => {
                eprintln!("Pausing cleanup: unreadable upload metadata: {e:#}");
                self.empty_since.clear();
                return Ok(());
            }
        };
        let mut directories = Vec::new();
        for entry in WalkDir::new(&self.config.uploads)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !hidden(e.path()))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.empty_since.clear();
                    return Err(e.into());
                }
            };
            if entry.depth() > 0 && entry.file_type().is_dir() {
                directories.push(entry.into_path());
            }
        }
        directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        let mut empty = HashSet::new();
        let mut expired = Vec::new();
        for dir in directories {
            if active.contains(&dir) {
                continue;
            }
            let children = fs::read_dir(&dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
            if children.iter().any(|child| !empty.contains(&child.path())) {
                continue;
            }
            empty.insert(dir.clone());
            let sig = signature(&dir)?;
            let record = self
                .empty_since
                .entry(dir.clone())
                .or_insert((sig.clone(), now));
            if record.0 != sig {
                *record = (sig, now);
            }
            if now.duration_since(record.1) >= self.config.empty_grace {
                expired.push(dir);
            }
        }
        self.empty_since.retain(|p, _| empty.contains(p));
        for dir in expired {
            match self.active_directories() {
                Ok(active) if active.contains(&dir) => {
                    self.empty_since.remove(&dir);
                    continue;
                }
                Ok(_) => (),
                Err(e) => {
                    self.empty_since.clear();
                    eprintln!("Pausing cleanup: metadata changed: {e:#}");
                    break;
                }
            }
            self.empty_since.remove(&dir);
            match fs::remove_dir(&dir) {
                Ok(()) => eprintln!("Removed empty upload directory {}", dir.display()),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
