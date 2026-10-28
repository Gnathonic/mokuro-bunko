# M1: LibraryStore shim and LocalStore — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** All library filesystem access in bunko-library, bunko-dav and bunko-server goes through a new `bunko-store` shim, with a `LocalStore` that forwards to `std::fs`. Behaviour is unchanged.

**Architecture:** A new crate, `bunko-store`, defines the `LibraryStore` trait, its value types (`Meta`, `FileType`, `File`, `ReadDir`/`DirEntry`, `OpenOptions`, `WriteIntent`), a process mount table that dispatches by the longest path prefix, the `fs` and `afs` shim modules, and `LocalStore`. Unmounted paths, and paths under a mounted `LocalStore`, both end in exactly the std call that runs today. The three crates swap `std::fs`/`tokio::fs`/`Path::exists`-style calls for the shim. A clippy `disallowed-methods` gate keeps new code from bypassing it.

**Tech Stack:** Rust 2024 (rust-version 1.88), parking_lot, notify 8 (feature `watch`), tokio (feature `tokio`).

**Spec:** `docs/design/0.8-remote-storage.md` §3 (interface), §4 (LocalStore), §13 M1 testing, §14 M1 row.

## Global Constraints

- M1 is a pure refactor: no behaviour change; `cargo test --workspace` stays green, Windows leg included.
- `golden_library.rs` stays byte-identical; `a_warm_python_cache_is_read_back_without_recompiling` holds, pre-1970 mtimes included.
- `py_mtime` must read the same `mtime()`/`mtime_nsec()` as today on unix and the same `modified()` fallback elsewhere.
- No change to the DB schema, file names, temp names, the ETag format, `Stamp`, or `PathWriteLocks`.
- `DirEntry::metadata()` stays lazy for `Std` entries (no extra `lstat` in PathCase scans or PROPFIND listings).
- Each caller keeps its own listing semantics (lossy vs skip non-UTF-8, symlink handling): the shim does not filter or sort.
- `clippy --workspace --all-targets -D warnings` passes; the gate has zero hits outside `#[allow]`ed test code.
- PROPFIND Depth 1 of 2,000 entries and `build_volume_manifest` stay within 3 % of 0.7.3.
- Licences: no new non-workspace dependencies.

## Review Focus

1. **An unmounted path inside the base dir** (`users/`, `.processing/`, `mokuro.db`): must keep using std exactly. This is tested in Task 1 (`unmounted_paths_use_std`).
2. **A rename from an unmounted staging dir into the library** (`collect.rs` install from `.processing`): must still be one `rename`, and must still return the EXDEV `CrossesDevices` error that `move_into_place` falls back on. This is tested in Task 1 (`rename_from_unmounted_into_local_store_is_std_rename`) and Task 6 (the existing `ocr_sched` suites).
3. **A pre-1970 or nanosecond mtime** read through `Meta`: `py_mtime` must equal today's value bit for bit. This is tested in Task 1 (`meta_std_matches_std_values`) and Task 4 (golden warm cache).
4. **A file vanishing mid-listing** (`read_dir` then `metadata` on an entry that was just deleted): the error surfaces per entry as today. This is tested in Task 1 (`read_dir_entry_vanishing_is_per_entry_error`).
5. **Case-only rename on a case-insensitive host** (`kingdom`→`Kingdom`): `same_file` through `Meta::dev/ino` must still detect it. This is tested in Task 5 by the existing `path_case` and `dav_path_case` suites, which run on the CI Windows leg.

---

## File structure

```
crates/bunko-store/
  Cargo.toml
  src/lib.rs        re-exports; crate docs
  src/meta.rs       Meta, RemoteMeta, FileType, Version
  src/store.rs      LibraryStore trait, Caps, Health, Capacity, WriteIntent, Mode, Origin,
                    Durability, Published, OpId, LocalPath, WatchEvent, WatchGuard
  src/file.rs       File enum, RemoteFile trait, OpenOptions, ReadDir, DirEntry, RemoteEntry
  src/mount.rs      mount table (mount, store_for, MountGuard)
  src/fs.rs         sync shim (std::fs mirror + publish + exists/is_file/is_dir)
  src/afs.rs        tokio shim (feature "tokio")
  src/local.rs      LocalStore (+ watch behind feature "watch")
  tests/local.rs    passthrough / routing tests
```

Changed: `bunko-library` (fsutil.rs, index.rs, compiler.rs, sidecar.rs, manifest.rs, service.rs, archive/*), `bunko-dav` (path_case.rs, resource.rs, paths.rs, propfind.rs, cache.rs, upload.rs, methods/*, sidecars.rs, lib.rs), `bunko-server` (library/watcher.rs, library/runtime.rs, thumbs/mod.rs, ocr/{collect,upgrade,owed,types,settle,claim,api,bench,...}.rs, admin, app.rs), plus a `clippy.toml` in each of the three crates.

---

### Task 1: the `bunko-store` crate (types, trait, mount table, sync shim, LocalStore)

**Files:**
- Create: `crates/bunko-store/Cargo.toml`, `src/{lib,meta,store,file,mount,fs,local}.rs`, `tests/local.rs`
- Modify: root `Cargo.toml` (`[workspace.dependencies] bunko-store = { path = "crates/bunko-store" }`)

**Interfaces:**
- Produces (used by every later task):
  - `bunko_store::Meta` with `len() -> u64`, `is_file()`, `is_dir()`, `is_symlink()`, `file_type() -> FileType`, `modified() -> io::Result<SystemTime>`, `unix_mtime() -> Option<(i64, i64)>`, `dev() -> Option<u64>`, `ino() -> Option<u64>`, `version() -> Option<&Version>`, `own() -> bool`, `as_std() -> Option<&std::fs::Metadata>`.
  - `bunko_store::FileType` with `is_file()`, `is_dir()`, `is_symlink()`.
  - `bunko_store::fs::{metadata, symlink_metadata, read_dir, read, read_to_string, write, create_dir, create_dir_all, remove_file, remove_dir, remove_dir_all, rename, copy, canonicalize, read_link, exists, try_exists, is_file, is_dir, publish, File, OpenOptions, ReadDir, DirEntry}`.
  - `bunko_store::{LibraryStore, LocalStore, mount, store_for, MountGuard, WriteIntent, Mode, Origin, Durability, Published, Version}`.

- [ ] **Step 1: Cargo manifest**

```toml
[package]
name = "bunko-store"
description = "The library storage seam of mokuro-bunko: one interface over the local disk and remote backends"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[features]
default = []
tokio = ["dep:tokio"]
watch = ["dep:notify"]

[dependencies]
parking_lot.workspace = true
tracing.workspace = true
tokio = { workspace = true, optional = true }
notify = { workspace = true, optional = true }

[dev-dependencies]
tempfile.workspace = true
tokio = { workspace = true, features = ["test-util"] }
```

- [ ] **Step 2: Write the failing tests** (`tests/local.rs`)

```rust
use std::io::{Read, Write};
use std::sync::Arc;

use bunko_store::fs;
use bunko_store::{mount, store_for, LocalStore, Mode, Origin, Durability, WriteIntent};

fn tmp() -> tempfile::TempDir { tempfile::tempdir().unwrap() }

#[test]
fn unmounted_paths_use_std() {
    let d = tmp();
    let p = d.path().join("a.txt");
    fs::write(&p, b"hi").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"hi");
    assert!(store_for(&p).is_none());
    assert!(fs::is_file(&p));
    assert!(fs::exists(&p));
    assert!(!fs::is_dir(&p));
}

#[test]
fn meta_std_matches_std_values() {
    let d = tmp();
    let p = d.path().join("old.bin");
    std::fs::write(&p, b"12345").unwrap();
    // pre-1970 with nanoseconds
    let t = std::time::UNIX_EPOCH - std::time::Duration::new(86_400 * 400, 123_456_789);
    std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
    let ours = fs::metadata(&p).unwrap();
    let std_meta = std::fs::metadata(&p).unwrap();
    assert_eq!(ours.len(), std_meta.len());
    assert_eq!(ours.modified().unwrap(), std_meta.modified().unwrap());
    assert!(ours.is_file() && !ours.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(ours.unix_mtime(), Some((std_meta.mtime(), std_meta.mtime_nsec())));
        assert_eq!(ours.dev(), Some(std_meta.dev()));
        assert_eq!(ours.ino(), Some(std_meta.ino()));
    }
}

#[test]
fn longest_prefix_mount_wins_and_guard_unmounts() {
    let d = tmp();
    let outer = d.path().join("lib");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let g1 = mount(Arc::new(LocalStore::new(&outer)));
    let g2 = mount(Arc::new(LocalStore::new(&inner)));
    assert_eq!(store_for(&inner.join("x")).unwrap().root(), inner.as_path());
    assert_eq!(store_for(&outer.join("y")).unwrap().root(), outer.as_path());
    drop(g2);
    assert_eq!(store_for(&inner.join("x")).unwrap().root(), outer.as_path());
    drop(g1);
    assert!(store_for(&outer.join("y")).is_none());
}

#[test]
fn local_store_round_trip_through_shim() {
    let d = tmp();
    let root = d.path().join("library");
    std::fs::create_dir_all(&root).unwrap();
    let _g = mount(Arc::new(LocalStore::new(&root)));
    let series = root.join("Kingdom");
    fs::create_dir_all(&series).unwrap();
    let staged = series.join(".v1.cbz.upload-x.tmp");
    let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&staged).unwrap();
    f.write_all(b"zipbytes").unwrap();
    f.sync_all().unwrap();
    drop(f);
    let dest = series.join("v1.cbz");
    let intent = WriteIntent { mode: Mode::CreateNew, origin: Origin::Admin, durability: Durability::Sync };
    fs::publish(&staged, &dest, &intent).unwrap();
    assert!(!staged.exists());
    let mut s = String::new();
    fs::File::open(&dest).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "zipbytes");
    let names: Vec<_> = fs::read_dir(&series).unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, vec!["v1.cbz".to_string()]);
    let e = fs::read_dir(&series).unwrap().next().unwrap().unwrap();
    assert!(e.file_type().unwrap().is_file());
    assert_eq!(e.metadata().unwrap().len(), 8);
    fs::remove_file(&dest).unwrap();
    fs::remove_dir(&series).unwrap();
    assert!(!fs::exists(&series));
}

#[test]
fn rename_from_unmounted_into_local_store_is_std_rename() {
    let d = tmp();
    let root = d.path().join("library");
    let outside = d.path().join(".processing");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let _g = mount(Arc::new(LocalStore::new(&root)));
    let src = outside.join("result.mokuro");
    std::fs::write(&src, b"{}").unwrap();
    let dst = root.join("v.mokuro");
    fs::rename(&src, &dst).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"{}");
    assert!(!src.exists());
}

#[test]
fn read_dir_entry_vanishing_is_per_entry_error() {
    let d = tmp();
    std::fs::write(d.path().join("a"), b"").unwrap();
    std::fs::write(d.path().join("b"), b"").unwrap();
    let entries: Vec<_> = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap()).collect();
    std::fs::remove_file(d.path().join("a")).unwrap();
    let results: Vec<_> = entries.iter().map(|e| e.metadata().is_ok()).collect();
    assert_eq!(results.iter().filter(|ok| **ok).count(), 1);
}

#[test]
fn copy_and_canonicalize_pass_through() {
    let d = tmp();
    let a = d.path().join("a");
    std::fs::write(&a, b"xyz").unwrap();
    let b = d.path().join("b");
    assert_eq!(fs::copy(&a, &b).unwrap(), 3);
    assert_eq!(fs::canonicalize(&b).unwrap(), std::fs::canonicalize(&b).unwrap());
    assert_eq!(fs::read_to_string(&b).unwrap(), "xyz");
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p bunko-store`
Expected: FAIL to compile (`bunko_store` items not defined).

- [ ] **Step 4: Implement the crate**

`src/meta.rs`:

```rust
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// An opaque content version (a provider's cver). Local files have none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version(pub Arc<str>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType { File, Dir, Symlink, Other }

impl FileType {
    pub fn is_file(&self) -> bool { matches!(self, FileType::File) }
    pub fn is_dir(&self) -> bool { matches!(self, FileType::Dir) }
    pub fn is_symlink(&self) -> bool { matches!(self, FileType::Symlink) }
    pub fn from_std(t: std::fs::FileType) -> Self {
        if t.is_symlink() { FileType::Symlink }
        else if t.is_dir() { FileType::Dir }
        else if t.is_file() { FileType::File }
        else { FileType::Other }
    }
}

/// Metadata of a remote entry (§5.2 effective mtime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMeta {
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_s: i64,
    pub is_dir: bool,
    pub dev: u64,
    pub ino: u64,
    pub cver: Option<Version>,
    pub own: bool,
}

/// `std::fs::Metadata`, or a remote entry's equivalent.
#[derive(Debug, Clone)]
pub enum Meta { Std(std::fs::Metadata), Remote(RemoteMeta) }

impl Meta {
    pub fn len(&self) -> u64 { match self { Meta::Std(m) => m.len(), Meta::Remote(r) => r.size } }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn is_file(&self) -> bool { match self { Meta::Std(m) => m.is_file(), Meta::Remote(r) => !r.is_dir } }
    pub fn is_dir(&self) -> bool { match self { Meta::Std(m) => m.is_dir(), Meta::Remote(r) => r.is_dir } }
    pub fn is_symlink(&self) -> bool { match self { Meta::Std(m) => m.is_symlink(), Meta::Remote(_) => false } }
    pub fn file_type(&self) -> FileType {
        match self {
            Meta::Std(m) => FileType::from_std(m.file_type()),
            Meta::Remote(r) => if r.is_dir { FileType::Dir } else { FileType::File },
        }
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        match self {
            Meta::Std(m) => m.modified(),
            Meta::Remote(r) => Ok(system_time_from_ns(r.mtime_ns)),
        }
    }
    /// `(st_mtime, st_mtime_nsec)` where the platform has them (unix `MetadataExt`,
    /// every remote entry); `None` on other platforms, whose callers use `modified()`.
    pub fn unix_mtime(&self) -> Option<(i64, i64)> {
        match self {
            #[cfg(unix)]
            Meta::Std(m) => { use std::os::unix::fs::MetadataExt; Some((m.mtime(), m.mtime_nsec())) }
            #[cfg(not(unix))]
            Meta::Std(_) => None,
            Meta::Remote(r) => Some((r.mtime_ns.div_euclid(1_000_000_000) as i64,
                                     r.mtime_ns.rem_euclid(1_000_000_000) as i64)),
        }
    }
    pub fn dev(&self) -> Option<u64> {
        match self {
            #[cfg(unix)]
            Meta::Std(m) => { use std::os::unix::fs::MetadataExt; Some(m.dev()) }
            #[cfg(not(unix))]
            Meta::Std(_) => None,
            Meta::Remote(r) => Some(r.dev),
        }
    }
    pub fn ino(&self) -> Option<u64> {
        match self {
            #[cfg(unix)]
            Meta::Std(m) => { use std::os::unix::fs::MetadataExt; Some(m.ino()) }
            #[cfg(not(unix))]
            Meta::Std(_) => None,
            Meta::Remote(r) => Some(r.ino),
        }
    }
    pub fn version(&self) -> Option<&Version> { match self { Meta::Std(_) => None, Meta::Remote(r) => r.cver.as_ref() } }
    pub fn own(&self) -> bool { match self { Meta::Std(_) => false, Meta::Remote(r) => r.own } }
    pub fn as_std(&self) -> Option<&std::fs::Metadata> { match self { Meta::Std(m) => Some(m), Meta::Remote(_) => None } }
}

impl From<std::fs::Metadata> for Meta { fn from(m: std::fs::Metadata) -> Self { Meta::Std(m) } }

fn system_time_from_ns(ns: i128) -> SystemTime {
    if ns >= 0 { UNIX_EPOCH + Duration::from_nanos(ns as u64) }
    else { UNIX_EPOCH - Duration::from_nanos((-ns) as u64) }
}
```

`src/store.rs`:

```rust
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::file::{File, OpenOptions, ReadDir};
use crate::meta::{Meta, Version};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub remote: bool,
    pub case_sensitive: bool,
    pub symlinks: bool,
    /// Files have real local paths (X-Accel, mmap, `local_path` in place).
    pub local_paths: bool,
    pub atomic_replace: bool,
    pub server_copy: bool,
    pub trash_on_delete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked { StorageFull, TransferQuota, DailyLimit }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Online,
    Syncing { done: u64, total: u64 },
    Degraded { retry_at: SystemTime },
    Offline,
    NeedsReauth,
    Blocked(Blocked),
    Suspect,
    LeaseHeld { by: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity { pub total: u64, pub available: u64 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode { Replace { base: Option<Version> }, CreateNew }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin { Compiler, Ocr { claim: String }, Upgrade, Thumbs, Status, Dav { user: Option<String> }, Admin }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability { Sync, Deferred }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteIntent { pub mode: Mode, pub origin: Origin, pub durability: Durability }

impl WriteIntent {
    /// What a plain `rename` into a store means (§3.4).
    pub fn default_publish() -> Self {
        WriteIntent { mode: Mode::Replace { base: None }, origin: Origin::Admin, durability: Durability::Sync }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(pub u64);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published { pub version: Option<Version>, pub sha256: Option<[u8; 32]>, pub op: Option<OpId> }

/// A path a whole-file consumer (mmap, OCR engines) can open.
pub enum LocalPath { InPlace(PathBuf), Spooled(Arc<dyn SpoolPin>) }

impl LocalPath {
    pub fn path(&self) -> &Path {
        match self { LocalPath::InPlace(p) => p, LocalPath::Spooled(pin) => pin.path() }
    }
}

/// Keeps a spooled copy alive while held.
pub trait SpoolPin: Send + Sync { fn path(&self) -> &Path; }

/// One relevant change in the library tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// `path` appeared (created, or the destination of a move).
    Added { path: PathBuf, is_dir: bool },
    /// `path` vanished (deleted, or the source of a move).
    Removed { path: PathBuf, is_dir: bool },
    /// Events were lost: rescan.
    Rescan,
}

impl WatchEvent {
    pub fn path(&self) -> Option<&Path> {
        match self {
            WatchEvent::Added { path, .. } | WatchEvent::Removed { path, .. } => Some(path),
            WatchEvent::Rescan => None,
        }
    }
}

/// Watching runs while this value lives.
pub struct WatchGuard(pub Box<dyn Send>);

pub type WatchSink = Arc<dyn Fn(WatchEvent) + Send + Sync>;

/// The storage seam (spec §3.3). Synchronous: bunko-library is synchronous by design,
/// and async callers already wrap filesystem work in `spawn_blocking`.
pub trait LibraryStore: Send + Sync + 'static {
    fn root(&self) -> &Path;
    fn caps(&self) -> Caps;
    fn health(&self) -> Health { Health::Online }
    // metadata
    fn metadata(&self, p: &Path) -> io::Result<Meta>;
    fn symlink_metadata(&self, p: &Path) -> io::Result<Meta>;
    fn read_dir(&self, p: &Path) -> io::Result<ReadDir>;
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;
    fn read_link(&self, p: &Path) -> io::Result<PathBuf>;
    // content
    fn open(&self, p: &Path) -> io::Result<File>;
    fn local_path(&self, p: &Path, expect: Option<&Version>) -> io::Result<LocalPath>;
    fn content_sha256(&self, _p: &Path) -> io::Result<Option<[u8; 32]>> { Ok(None) }
    // mutation
    fn open_with(&self, p: &Path, o: &OpenOptions) -> io::Result<File>;
    fn publish(&self, staged: &Path, dest: &Path, w: &WriteIntent) -> io::Result<Published>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
    fn remove_file(&self, p: &Path) -> io::Result<()>;
    fn remove_dir(&self, p: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, p: &Path) -> io::Result<()>;
    fn create_dir(&self, p: &Path) -> io::Result<()>;
    fn create_dir_all(&self, p: &Path) -> io::Result<()>;
    // environment
    fn watch(&self, sink: WatchSink) -> io::Result<WatchGuard>;
    fn capacity(&self) -> Option<Capacity> { None }
    fn recently_removed(&self, _p: &Path) -> Option<SystemTime> { None }
    /// The variant PathCase should fold a request to when no exact spelling exists
    /// (§5.9). `None` = PathCase keeps its own choice.
    fn case_winner(&self, _parent: &Path, _fold: &str) -> Option<String> { None }
}

/// A stand-in for an `io::ErrorKind::CrossesDevices` that stable Rust spells this way.
pub fn crosses_devices() -> io::Error {
    io::Error::new(io::ErrorKind::CrossesDevices, "the paths are in different stores")
}

pub const QUIET: Duration = Duration::from_secs(120);
```

`src/file.rs`:

```rust
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::meta::{FileType, Meta};

/// A remote store's open file.
pub trait RemoteFile: Read + Write + Seek + Send + fmt::Debug {
    fn metadata(&self) -> io::Result<Meta>;
    fn sync_all(&self) -> io::Result<()>;
    fn set_len(&self, len: u64) -> io::Result<()>;
}

#[derive(Debug)]
pub enum File { Std(std::fs::File), Remote(Box<dyn RemoteFile>) }

impl File {
    pub fn open(p: impl AsRef<Path>) -> io::Result<File> { crate::fs::open(p.as_ref()) }
    pub fn create(p: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().write(true).create(true).truncate(true).open(p)
    }
    pub fn create_new(p: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create_new(true).open(p)
    }
    pub fn options() -> OpenOptions { OpenOptions::new() }
    pub fn metadata(&self) -> io::Result<Meta> {
        match self { File::Std(f) => f.metadata().map(Meta::Std), File::Remote(f) => f.metadata() }
    }
    pub fn sync_all(&self) -> io::Result<()> {
        match self { File::Std(f) => f.sync_all(), File::Remote(f) => f.sync_all() }
    }
    pub fn sync_data(&self) -> io::Result<()> {
        match self { File::Std(f) => f.sync_data(), File::Remote(f) => f.sync_all() }
    }
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        match self { File::Std(f) => f.set_len(len), File::Remote(f) => f.set_len(len) }
    }
    pub fn set_times(&self, times: std::fs::FileTimes) -> io::Result<()> {
        match self { File::Std(f) => f.set_times(times), File::Remote(_) => Ok(()) }
    }
    pub fn set_modified(&self, t: std::time::SystemTime) -> io::Result<()> {
        match self { File::Std(f) => f.set_modified(t), File::Remote(_) => Ok(()) }
    }
    pub fn into_std(self) -> Option<std::fs::File> {
        match self { File::Std(f) => Some(f), File::Remote(_) => None }
    }
    pub fn as_std(&self) -> Option<&std::fs::File> {
        match self { File::Std(f) => Some(f), File::Remote(_) => None }
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self { File::Std(f) => f.read(buf), File::Remote(f) => f.read(buf) }
    }
}
impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self { File::Std(f) => f.write(buf), File::Remote(f) => f.write(buf) }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self { File::Std(f) => f.flush(), File::Remote(f) => f.flush() }
    }
}
impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self { File::Std(f) => f.seek(pos), File::Remote(f) => f.seek(pos) }
    }
}

/// `std::fs::OpenOptions`, store-routed.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
    pub mode: Option<u32>,
}

impl OpenOptions {
    pub fn new() -> Self { Self::default() }
    pub fn read(&mut self, v: bool) -> &mut Self { self.read = v; self }
    pub fn write(&mut self, v: bool) -> &mut Self { self.write = v; self }
    pub fn append(&mut self, v: bool) -> &mut Self { self.append = v; self }
    pub fn truncate(&mut self, v: bool) -> &mut Self { self.truncate = v; self }
    pub fn create(&mut self, v: bool) -> &mut Self { self.create = v; self }
    pub fn create_new(&mut self, v: bool) -> &mut Self { self.create_new = v; self }
    /// unix permission bits for a created file (`OpenOptionsExt::mode`); ignored elsewhere.
    pub fn mode(&mut self, m: u32) -> &mut Self { self.mode = Some(m); self }
    pub fn open(&self, p: impl AsRef<Path>) -> io::Result<File> { crate::fs::open_with(p.as_ref(), self) }

    /// The equivalent `std::fs::OpenOptions`.
    pub fn to_std(&self) -> std::fs::OpenOptions {
        let mut o = std::fs::OpenOptions::new();
        o.read(self.read).write(self.write).append(self.append).truncate(self.truncate)
            .create(self.create).create_new(self.create_new);
        #[cfg(unix)]
        if let Some(m) = self.mode { use std::os::unix::fs::OpenOptionsExt; o.mode(m); }
        o
    }
}

/// A remote store's directory entry.
#[derive(Debug, Clone)]
pub struct RemoteEntry { pub path: PathBuf, pub name: OsString, pub meta: Meta }

#[derive(Debug)]
pub enum DirEntry { Std(std::fs::DirEntry), Remote(RemoteEntry) }

impl DirEntry {
    pub fn path(&self) -> PathBuf { match self { DirEntry::Std(e) => e.path(), DirEntry::Remote(e) => e.path.clone() } }
    pub fn file_name(&self) -> OsString { match self { DirEntry::Std(e) => e.file_name(), DirEntry::Remote(e) => e.name.clone() } }
    pub fn file_type(&self) -> io::Result<FileType> {
        match self { DirEntry::Std(e) => e.file_type().map(FileType::from_std), DirEntry::Remote(e) => Ok(e.meta.file_type()) }
    }
    /// Lazy for std entries: one `lstat`-like call, exactly as `std::fs::DirEntry::metadata`.
    pub fn metadata(&self) -> io::Result<Meta> {
        match self { DirEntry::Std(e) => e.metadata().map(Meta::Std), DirEntry::Remote(e) => Ok(e.meta.clone()) }
    }
}

pub enum ReadDir {
    Std(std::fs::ReadDir),
    Remote(std::vec::IntoIter<io::Result<DirEntry>>),
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ReadDir::Std(r) => r.next().map(|e| e.map(DirEntry::Std)),
            ReadDir::Remote(r) => r.next(),
        }
    }
}
```

`src/mount.rs`:

```rust
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;

use crate::store::LibraryStore;

static MOUNTS: RwLock<Vec<(u64, Arc<dyn LibraryStore>)>> = RwLock::new(Vec::new());
static COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The store mounted while this lives.
#[must_use = "dropping the guard unmounts the store"]
pub struct MountGuard(u64);

impl Drop for MountGuard {
    fn drop(&mut self) {
        let mut m = MOUNTS.write();
        m.retain(|(id, _)| *id != self.0);
        COUNT.store(m.len(), Ordering::Release);
    }
}

/// Mount `store` at `store.root()`.
pub fn mount(store: Arc<dyn LibraryStore>) -> MountGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut m = MOUNTS.write();
    m.push((id, store));
    COUNT.store(m.len(), Ordering::Release);
    MountGuard(id)
}

/// The store whose root is the longest prefix of `p` (lexical, case-exact).
pub fn store_for(p: &Path) -> Option<Arc<dyn LibraryStore>> {
    if COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let m = MOUNTS.read();
    m.iter()
        .filter(|(_, s)| p.starts_with(s.root()))
        .max_by_key(|(_, s)| s.root().as_os_str().len())
        .map(|(_, s)| Arc::clone(s))
}
```

`src/fs.rs`:

```rust
//! A `std::fs` mirror routed through the mount table: a path under a mounted store
//! goes to that store, any other path to `std::fs` directly.
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub use crate::file::{DirEntry, File, OpenOptions, ReadDir};
use crate::meta::Meta;
use crate::mount::store_for;
use crate::store::{crosses_devices, Published, WriteIntent};

pub fn metadata(p: impl AsRef<Path>) -> io::Result<Meta> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.metadata(p), None => std::fs::metadata(p).map(Meta::Std) }
}
pub fn symlink_metadata(p: impl AsRef<Path>) -> io::Result<Meta> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.symlink_metadata(p), None => std::fs::symlink_metadata(p).map(Meta::Std) }
}
pub fn read_dir(p: impl AsRef<Path>) -> io::Result<ReadDir> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.read_dir(p), None => std::fs::read_dir(p).map(ReadDir::Std) }
}
pub fn canonicalize(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.canonicalize(p), None => std::fs::canonicalize(p) }
}
pub fn read_link(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.read_link(p), None => std::fs::read_link(p) }
}
/// `Path::exists` (follows symlinks; errors are `false`).
pub fn exists(p: impl AsRef<Path>) -> bool { metadata(p).is_ok() }
/// `Path::try_exists`.
pub fn try_exists(p: impl AsRef<Path>) -> io::Result<bool> {
    match metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
/// `Path::is_file`.
pub fn is_file(p: impl AsRef<Path>) -> bool { metadata(p).is_ok_and(|m| m.is_file()) }
/// `Path::is_dir`.
pub fn is_dir(p: impl AsRef<Path>) -> bool { metadata(p).is_ok_and(|m| m.is_dir()) }

pub(crate) fn open(p: &Path) -> io::Result<File> {
    match store_for(p) { Some(s) => s.open(p), None => std::fs::File::open(p).map(File::Std) }
}
pub(crate) fn open_with(p: &Path, o: &OpenOptions) -> io::Result<File> {
    match store_for(p) { Some(s) => s.open_with(p, o), None => o.to_std().open(p).map(File::Std) }
}
pub fn read(p: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let p = p.as_ref();
    match store_for(p) {
        None => std::fs::read(p),
        Some(s) => { let mut v = Vec::new(); s.open(p)?.read_to_end(&mut v)?; Ok(v) }
    }
}
pub fn read_to_string(p: impl AsRef<Path>) -> io::Result<String> {
    let p = p.as_ref();
    match store_for(p) {
        None => std::fs::read_to_string(p),
        Some(s) => { let mut v = String::new(); s.open(p)?.read_to_string(&mut v)?; Ok(v) }
    }
}
/// `std::fs::write`. On a remote store this is one Sync publish (§3.4).
pub fn write(p: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) {
        None => std::fs::write(p, data),
        Some(s) => {
            let mut o = OpenOptions::new();
            o.write(true).create(true).truncate(true);
            let mut f = s.open_with(p, &o)?;
            f.write_all(data.as_ref())?;
            f.flush()
        }
    }
}
pub fn create_dir(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.create_dir(p), None => std::fs::create_dir(p) }
}
pub fn create_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.create_dir_all(p), None => std::fs::create_dir_all(p) }
}
pub fn remove_file(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.remove_file(p), None => std::fs::remove_file(p) }
}
pub fn remove_dir(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.remove_dir(p), None => std::fs::remove_dir(p) }
}
pub fn remove_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    match store_for(p) { Some(s) => s.remove_dir_all(p), None => std::fs::remove_dir_all(p) }
}
/// `std::fs::rename` (§3.4): within one store, `rename`; from an unmounted path into a
/// store, `publish` with the default intent; across stores, `CrossesDevices`.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    match (store_for(from), store_for(to)) {
        (None, None) => std::fs::rename(from, to),
        (Some(a), Some(b)) if std::ptr::addr_eq(std::sync::Arc::as_ptr(&a), std::sync::Arc::as_ptr(&b)) => a.rename(from, to),
        (None, Some(b)) => b.publish(from, to, &WriteIntent::default_publish()).map(|_| ()),
        _ => Err(crosses_devices()),
    }
}
/// `std::fs::copy`, routed by the destination's store.
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let (from, to) = (from.as_ref(), to.as_ref());
    match store_for(to) {
        Some(s) => s.copy(from, to),
        None => match store_for(from) {
            None => std::fs::copy(from, to),
            Some(s) => {
                let mut src = s.open(from)?;
                let mut dst = std::fs::File::create(to)?;
                io::copy(&mut src, &mut dst)
            }
        },
    }
}
/// Commit a staged file (`staged`, created through this shim) as `dest` (§3.3 D1).
/// Unmounted: a plain rename.
pub fn publish(staged: impl AsRef<Path>, dest: impl AsRef<Path>, w: &WriteIntent) -> io::Result<Published> {
    let (staged, dest) = (staged.as_ref(), dest.as_ref());
    match store_for(dest) {
        Some(s) => s.publish(staged, dest, w),
        None => std::fs::rename(staged, dest).map(|_| Published::default()),
    }
}
```

`src/local.rs`:

```rust
use std::io;
use std::path::{Path, PathBuf};

use crate::file::{File, OpenOptions, ReadDir};
use crate::meta::{Meta, Version};
use crate::store::*;

/// Today's on-disk library: every call forwards to `std::fs` (spec §4).
pub struct LocalStore { root: PathBuf }

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self { Self { root: root.into() } }
}

impl LibraryStore for LocalStore {
    fn root(&self) -> &Path { &self.root }
    fn caps(&self) -> Caps {
        Caps { remote: false, case_sensitive: false, symlinks: true, local_paths: true,
               atomic_replace: true, server_copy: true, trash_on_delete: false }
    }
    fn metadata(&self, p: &Path) -> io::Result<Meta> { std::fs::metadata(p).map(Meta::Std) }
    fn symlink_metadata(&self, p: &Path) -> io::Result<Meta> { std::fs::symlink_metadata(p).map(Meta::Std) }
    fn read_dir(&self, p: &Path) -> io::Result<ReadDir> { std::fs::read_dir(p).map(ReadDir::Std) }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> { std::fs::canonicalize(p) }
    fn read_link(&self, p: &Path) -> io::Result<PathBuf> { std::fs::read_link(p) }
    fn open(&self, p: &Path) -> io::Result<File> { std::fs::File::open(p).map(File::Std) }
    fn local_path(&self, p: &Path, _expect: Option<&Version>) -> io::Result<LocalPath> { Ok(LocalPath::InPlace(p.to_path_buf())) }
    fn open_with(&self, p: &Path, o: &OpenOptions) -> io::Result<File> { o.to_std().open(p).map(File::Std) }
    fn publish(&self, staged: &Path, dest: &Path, _w: &WriteIntent) -> io::Result<Published> {
        std::fs::rename(staged, dest).map(|_| Published::default())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> { std::fs::rename(from, to) }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> { std::fs::copy(from, to) }
    fn remove_file(&self, p: &Path) -> io::Result<()> { std::fs::remove_file(p) }
    fn remove_dir(&self, p: &Path) -> io::Result<()> { std::fs::remove_dir(p) }
    fn remove_dir_all(&self, p: &Path) -> io::Result<()> { std::fs::remove_dir_all(p) }
    fn create_dir(&self, p: &Path) -> io::Result<()> { std::fs::create_dir(p) }
    fn create_dir_all(&self, p: &Path) -> io::Result<()> { std::fs::create_dir_all(p) }
    fn watch(&self, sink: WatchSink) -> io::Result<WatchGuard> { crate::watch::watch_local(&self.root, sink) }
}
```

`watch_local` lives in `src/watch.rs`: with feature `watch`, it is the notify body moved from `bunko-server/src/library/watcher.rs` (Task 3); without it, it returns `io::ErrorKind::Unsupported`. For Task 1 create the stub:

```rust
use std::io;
use std::path::Path;
use crate::store::{WatchGuard, WatchSink};

#[cfg(not(feature = "watch"))]
pub fn watch_local(_root: &Path, _sink: WatchSink) -> io::Result<WatchGuard> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "built without the watch feature"))
}
```

`src/lib.rs`:

```rust
//! The library storage seam (0.8 spec §3): every library filesystem access in
//! bunko-library, bunko-dav and bunko-server goes through [`fs`] (sync) or `afs`
//! (tokio), which route a path under a mounted [`LibraryStore`] to that store and
//! anything else straight to `std::fs`.
pub mod file;
pub mod fs;
pub mod local;
pub mod meta;
pub mod mount;
pub mod store;
pub mod watch;
#[cfg(feature = "tokio")]
pub mod afs;

pub use file::{DirEntry, File, OpenOptions, ReadDir, RemoteEntry, RemoteFile};
pub use local::LocalStore;
pub use meta::{FileType, Meta, RemoteMeta, Version};
pub use mount::{mount, store_for, MountGuard};
pub use store::*;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p bunko-store` — Expected: PASS (7 tests).
Run: `cargo clippy -p bunko-store --all-targets -- -D warnings` — Expected: clean.

- [ ] **Step 6: Commit** — `git add crates/bunko-store Cargo.toml Cargo.lock && git commit -m "bunko-store: the LibraryStore seam and LocalStore"`

---

### Task 2: the tokio shim (`afs`)

**Files:** Create `crates/bunko-store/src/afs.rs`; test in `crates/bunko-store/tests/afs.rs`.

**Interfaces:** Produces `bunko_store::afs::{metadata, create_dir_all, rename, remove_file, File, OpenOptions}`, where `afs::File::open(p).await` returns `afs::File` with `into_std(self) -> Option<tokio::fs::File>`, and `afs::OpenOptions` mirrors `tokio::fs::OpenOptions` (`read/write/create/create_new/truncate/append/mode`, `open(p).await -> io::Result<tokio::fs::File>`, routed: unmounted or LocalStore → `tokio::fs`; another store → `spawn_blocking` of the sync store call, wrapped in `tokio::fs::File::from_std` only when the store returns `File::Std`, else `io::ErrorKind::Unsupported` until M2's stream branch).

- [ ] **Step 1: failing test** (`tests/afs.rs`)

```rust
use bunko_store::afs;

#[tokio::test]
async fn afs_passthrough() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("a/b");
    afs::create_dir_all(&dir).await.unwrap();
    let p = dir.join("f");
    let mut f = afs::OpenOptions::new().write(true).create_new(true).open(&p).await.unwrap();
    use tokio::io::AsyncWriteExt;
    f.write_all(b"hello").await.unwrap();
    f.sync_all().await.unwrap();
    drop(f);
    assert_eq!(afs::metadata(&p).await.unwrap().len(), 5);
    let q = dir.join("g");
    afs::rename(&p, &q).await.unwrap();
    let file = afs::File::open(&q).await.unwrap().into_std().unwrap();
    assert_eq!(file.metadata().await.unwrap().len(), 5);
    afs::remove_file(&q).await.unwrap();
    assert!(afs::metadata(&q).await.is_err());
}
```

- [ ] **Step 2:** `cargo test -p bunko-store --features tokio` → FAIL (no `afs`).

- [ ] **Step 3: implement** `src/afs.rs`:

```rust
//! The `tokio::fs` mirror. Unmounted or local paths call `tokio::fs`; a path in a
//! remote store runs the sync store call on the blocking pool.
use std::io;
use std::path::{Path, PathBuf};

use crate::meta::Meta;
use crate::mount::store_for;

fn local(p: &Path) -> bool { store_for(p).is_none_or(|s| !s.caps().remote) }

pub async fn metadata(p: impl AsRef<Path>) -> io::Result<Meta> {
    let p = p.as_ref();
    if local(p) { return tokio::fs::metadata(p).await.map(Meta::Std); }
    let p = p.to_path_buf();
    tokio::task::spawn_blocking(move || crate::fs::metadata(&p)).await.map_err(io::Error::other)?
}
pub async fn create_dir_all(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    if local(p) { return tokio::fs::create_dir_all(p).await; }
    let p = p.to_path_buf();
    tokio::task::spawn_blocking(move || crate::fs::create_dir_all(&p)).await.map_err(io::Error::other)?
}
pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    if local(from) && local(to) { return tokio::fs::rename(from, to).await; }
    let (from, to): (PathBuf, PathBuf) = (from.into(), to.into());
    tokio::task::spawn_blocking(move || crate::fs::rename(&from, &to)).await.map_err(io::Error::other)?
}
pub async fn remove_file(p: impl AsRef<Path>) -> io::Result<()> {
    let p = p.as_ref();
    if local(p) { return tokio::fs::remove_file(p).await; }
    let p = p.to_path_buf();
    tokio::task::spawn_blocking(move || crate::fs::remove_file(&p)).await.map_err(io::Error::other)?
}

/// An opened file: tokio's for local files; M2 adds a remote stream variant.
pub enum File { Tokio(tokio::fs::File) }

impl File {
    pub async fn open(p: impl AsRef<Path>) -> io::Result<File> {
        let p = p.as_ref();
        if local(p) { return tokio::fs::File::open(p).await.map(File::Tokio); }
        let p = p.to_path_buf();
        let f = tokio::task::spawn_blocking(move || crate::fs::File::open(&p)).await.map_err(io::Error::other)??;
        match f.into_std() {
            Some(std) => Ok(File::Tokio(tokio::fs::File::from_std(std))),
            None => Err(io::Error::new(io::ErrorKind::Unsupported, "remote stream reads arrive in M2")),
        }
    }
    pub fn into_std(self) -> Option<tokio::fs::File> { match self { File::Tokio(f) => Some(f) } }
}

#[derive(Debug, Clone, Default)]
pub struct OpenOptions(crate::file::OpenOptions);

impl OpenOptions {
    pub fn new() -> Self { Self::default() }
    pub fn read(&mut self, v: bool) -> &mut Self { self.0.read(v); self }
    pub fn write(&mut self, v: bool) -> &mut Self { self.0.write(v); self }
    pub fn append(&mut self, v: bool) -> &mut Self { self.0.append(v); self }
    pub fn truncate(&mut self, v: bool) -> &mut Self { self.0.truncate(v); self }
    pub fn create(&mut self, v: bool) -> &mut Self { self.0.create(v); self }
    pub fn create_new(&mut self, v: bool) -> &mut Self { self.0.create_new(v); self }
    pub fn mode(&mut self, m: u32) -> &mut Self { self.0.mode(m); self }
    pub async fn open(&self, p: impl AsRef<Path>) -> io::Result<tokio::fs::File> {
        let p = p.as_ref();
        if local(p) { return tokio::fs::OpenOptions::from(self.0.to_std()).open(p).await; }
        let (p, o) = (p.to_path_buf(), self.0.clone());
        let f = tokio::task::spawn_blocking(move || o.open(&p)).await.map_err(io::Error::other)??;
        f.into_std().map(tokio::fs::File::from_std)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "remote staging files are local files"))
    }
}
```

- [ ] **Step 4:** `cargo test -p bunko-store --features tokio` → PASS.
- [ ] **Step 5: Commit** — `bunko-store: the tokio shim`.

---

### Task 3: move the watcher into `LocalStore::watch`

**Files:** Create `crates/bunko-store/src/watch.rs` body (feature `watch`); modify `crates/bunko-server/src/library/watcher.rs` (keep `translate`, `vanished_is_dir`, `is_relevant_change` filtering and its tests in the server; re-export `WatchEvent` from `bunko_store`), `crates/bunko-server/Cargo.toml` (`bunko-store = { workspace = true, features = ["watch", "tokio"] }`).

**Interfaces:** `bunko_store::watch::watch_local(root, sink) -> io::Result<WatchGuard>` delivers *raw* `notify::Event`s translated by a translator the server supplies? — No: to keep `is_relevant_change` and `translate` in the server (spec §3.8 item 6), `LocalStore::watch` takes the server's sink of `WatchEvent`s and `watch_local` needs the translator. Resolve by giving `LocalStore` an optional translator set at construction:

```rust
pub type Translate = std::sync::Arc<dyn Fn(&notify::Event) -> Vec<WatchEvent> + Send + Sync>;
impl LocalStore { pub fn with_translator(root: impl Into<PathBuf>, t: Translate) -> Self }
```

`watch_local` (feature `watch`):

```rust
pub fn watch_local(root: &Path, translate: Option<&Translate>, sink: WatchSink) -> io::Result<WatchGuard> {
    use notify::{RecursiveMode, Watcher};
    std::fs::create_dir_all(root)?;
    let t = translate.cloned();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(event) => if let Some(t) = &t { for change in t(&event) { sink(change); } },
        Err(error) => {
            tracing::warn!(%error, "[FS-WATCHER] watch error");
            if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) { sink(WatchEvent::Rescan); }
        }
    }).map_err(io::Error::other)?;
    watcher.watch(root, RecursiveMode::Recursive).map_err(io::Error::other)?;
    tracing::info!("[FS-WATCHER] Watching {}", root.display());
    Ok(WatchGuard(Box::new(watcher)))
}
```

`LibraryWatcher::start(root, on_event)` in the server becomes: build `LocalStore::with_translator(root, Arc::new(translate))`, call `.watch(Arc::new(on_event))`, keep the `WatchGuard`. Same log line, same `create_dir_all`, same errors (now `io::Error`; callers that matched `notify::Error` change to `io::Error`).

- [ ] **Step 1:** Run the existing watcher tests first: `cargo test -p bunko-server watcher` → PASS (baseline).
- [ ] **Step 2:** Move the code as above; `WatchEvent` in the server becomes `pub use bunko_store::WatchEvent;`.
- [ ] **Step 3:** `cargo test -p bunko-server watcher` and `cargo test -p bunko-server --test library_watch` (if present; else the library runtime tests) → PASS.
- [ ] **Step 4: Commit** — `watcher: LocalStore::watch owns the notify backend`.

---

### Task 4: bunko-library through the shim

**Files:** `crates/bunko-library/Cargo.toml` (+ `bunko-store.workspace = true`), every file with library fs access: `fsutil.rs`, `index.rs`, `compiler.rs`, `sidecar.rs`, `manifest.rs`, `service.rs`, `archive/mod.rs`, `archive/zipdir.rs`, plus `crates/bunko-library/clippy.toml`.

**Mapping rules (apply mechanically; the inventory `docs/design/0.8-research/inv-library.md` lists every site):**

| Today | Becomes |
|---|---|
| `use std::fs` / `std::fs::X(...)` | `use bunko_store::fs` / `bunko_store::fs::X(...)` |
| `std::fs::File`, `OpenOptions` | `bunko_store::fs::File`, `bunko_store::fs::OpenOptions` (`.mode(0o666)` replaces the `OpenOptionsExt` block) |
| `&std::fs::Metadata`, `Metadata` in signatures | `&bunko_store::Meta` |
| `path.exists()` / `.is_file()` / `.is_dir()` / `.metadata()` / `.symlink_metadata()` / `.read_dir()` / `.canonicalize()` / `.try_exists()` | `fs::exists(path)` / `fs::is_file(path)` / `fs::is_dir(path)` / `fs::metadata(path)` / `fs::symlink_metadata(path)` / `fs::read_dir(path)` / `fs::canonicalize(path)` / `fs::try_exists(path)` |
| `entry.file_type()?.is_dir()` on a `std::fs::DirEntry` | unchanged text; the entry is now `bunko_store::DirEntry` whose `FileType` has the same methods |
| `MetadataExt::mtime()/mtime_nsec()` | `meta.unix_mtime()` |
| tmp + `fs::rename` publish (`fsutil.rs:137`) | `fs::publish(&temp, path, &WriteIntent{mode: Replace{base}, origin: Compiler, durability: Deferred})` |

Special sites:
- `fsutil::py_mtime(meta: &Meta)`: `match meta.unix_mtime() { Some((s, ns)) => s as f64 + ns as f64 * 1e-9, None => <today's non-unix body using meta.modified()> }`. `py_mtime_int`, `size_of`, `stat -> Option<Meta>` follow.
- `fsutil::atomic_write_bytes(path, data)` gains `atomic_write_bytes_with(path, data, base: Option<Version>)`; `atomic_write_bytes` calls it with `None`. `service.rs::write_if_changed` captures `fs::metadata(path).ok().and_then(|m| m.version().cloned())` of the compared file and passes it as `base` (spec §3.8 item 5; on LocalStore it is always `None`).
- `compile_volume`'s `sidecar_meta: &Meta`.
- `archive/zipdir.rs` opens through `fs::File::open` (Read + Seek); `read_central_directory` is already generic over `Read + Seek` (`zipdir.rs:330`).

- [ ] **Step 1:** Add `crates/bunko-library/clippy.toml`:

```toml
disallowed-methods = [
  { path = "std::fs::metadata", reason = "use bunko_store::fs (0.8 §3)" },
  { path = "std::fs::symlink_metadata", reason = "use bunko_store::fs" },
  { path = "std::fs::read_dir", reason = "use bunko_store::fs" },
  { path = "std::fs::read", reason = "use bunko_store::fs" },
  { path = "std::fs::read_to_string", reason = "use bunko_store::fs" },
  { path = "std::fs::write", reason = "use bunko_store::fs" },
  { path = "std::fs::create_dir", reason = "use bunko_store::fs" },
  { path = "std::fs::create_dir_all", reason = "use bunko_store::fs" },
  { path = "std::fs::remove_file", reason = "use bunko_store::fs" },
  { path = "std::fs::remove_dir", reason = "use bunko_store::fs" },
  { path = "std::fs::remove_dir_all", reason = "use bunko_store::fs" },
  { path = "std::fs::rename", reason = "use bunko_store::fs" },
  { path = "std::fs::copy", reason = "use bunko_store::fs" },
  { path = "std::fs::canonicalize", reason = "use bunko_store::fs" },
  { path = "std::fs::read_link", reason = "use bunko_store::fs" },
  { path = "std::fs::File::open", reason = "use bunko_store::fs::File" },
  { path = "std::fs::File::create", reason = "use bunko_store::fs::File" },
  { path = "std::fs::File::create_new", reason = "use bunko_store::fs::File" },
  { path = "std::fs::OpenOptions::open", reason = "use bunko_store::fs::OpenOptions" },
  { path = "std::path::Path::exists", reason = "use bunko_store::fs::exists" },
  { path = "std::path::Path::try_exists", reason = "use bunko_store::fs::try_exists" },
  { path = "std::path::Path::is_file", reason = "use bunko_store::fs::is_file" },
  { path = "std::path::Path::is_dir", reason = "use bunko_store::fs::is_dir" },
  { path = "std::path::Path::metadata", reason = "use bunko_store::fs::metadata" },
  { path = "std::path::Path::symlink_metadata", reason = "use bunko_store::fs::symlink_metadata" },
  { path = "std::path::Path::read_dir", reason = "use bunko_store::fs::read_dir" },
  { path = "std::path::Path::canonicalize", reason = "use bunko_store::fs::canonicalize" },
  { path = "std::path::Path::read_link", reason = "use bunko_store::fs::read_link" },
  { path = "tokio::fs::metadata", reason = "use bunko_store::afs" },
  { path = "tokio::fs::create_dir_all", reason = "use bunko_store::afs" },
  { path = "tokio::fs::rename", reason = "use bunko_store::afs" },
  { path = "tokio::fs::remove_file", reason = "use bunko_store::afs" },
  { path = "tokio::fs::File::open", reason = "use bunko_store::afs::File" },
  { path = "tokio::fs::File::create", reason = "use bunko_store::afs::OpenOptions" },
  { path = "tokio::fs::OpenOptions::open", reason = "use bunko_store::afs::OpenOptions" },
]
```

- [ ] **Step 2:** `cargo clippy -p bunko-library --all-targets -- -D warnings` → expected FAIL listing every site (this is the to-do list).
- [ ] **Step 3:** Apply the mapping to every listed `src/` site. Test files (`tests/*.rs`, `#[cfg(test)] mod tests`) get `#![allow(clippy::disallowed_methods)]` / `#[allow(clippy::disallowed_methods)]`: fixtures write with std on purpose.
- [ ] **Step 4:** `cargo clippy -p bunko-library --all-targets -- -D warnings` → clean; `cargo test -p bunko-library` → PASS (golden + warm cache included).
- [ ] **Step 5: Commit** — `bunko-library: library fs access through bunko-store`.

---

### Task 5: bunko-dav through the shim

**Files:** `crates/bunko-dav/Cargo.toml` (+ `bunko-store = { workspace = true, features = ["tokio"] }`), `clippy.toml` (same list as Task 4), `src/{path_case,resource,paths,propfind,cache,upload,sidecars,lib}.rs`, `src/methods/{mod,read,put,write,lock}.rs`.

Same mapping table as Task 4, plus:
- `path_case::same_file(a, b)`: `match (fs::metadata(a), fs::metadata(b)) { (Ok(x), Ok(y)) => match (x.dev().zip(x.ino()), y.dev().zip(y.ino())) { (Some(p), Some(q)) => p == q, _ => <today's non-unix fallback> }, _ => false }` (keep today's non-unix branch verbatim).
- `resource.rs:20-38` `Stat::from_meta(&Meta)`; the `MetadataExt` use becomes `Meta::unix_mtime()`/`dev()`/`ino()`.
- `upload.rs`: `tokio::fs::*` → `afs::*`; the commit rename (`:382`) becomes `spawn_blocking(fs::publish(temp, dest, &WriteIntent{ mode: <Replace{base: If-Match cver} or CreateNew for If-None-Match: *>, origin: Dav{user}, durability: Sync }))`. With LocalStore the intent is ignored; build `Mode::Replace{base: None}` today where the request carries no version information.
- `methods/write.rs:593-605` copy + `set_times` + rename: `fs::copy`, `fs::File` `set_times`, `fs::rename`; `:538,634` renames via `fs::rename`; `:86,119,122` removals via `fs`.
- `methods/read.rs:221`: `afs::File::open(phys).await?.into_std()` (always `Some` for local stores in M1).
- `put.rs:193,244`: `afs::metadata`.
- `lib.rs:124` root `canonicalize` via `fs::canonicalize`.

- [ ] **Step 1:** add `clippy.toml`; run `cargo clippy -p bunko-dav --all-targets -- -D warnings` → FAIL list.
- [ ] **Step 2:** apply the mapping; allow-list test files.
- [ ] **Step 3:** `cargo clippy -p bunko-dav --all-targets -- -D warnings` clean; `cargo test -p bunko-dav` PASS (webdav_ops, path_case, uploads, move_copy_lock, propfind_cache).
- [ ] **Step 4: Commit** — `bunko-dav: library fs access through bunko-store`.

---

### Task 6: bunko-server through the shim, mount at startup

**Files:** `crates/bunko-server/Cargo.toml`, `clippy.toml` (same list), every `src/` site the clippy run lists (library/, thumbs/, ocr/, admin/, app.rs, davhooks.rs, backend.rs, ops/…). Exempt (keep std, mark `#[allow(clippy::disallowed_methods)]` with a `// unmounted: <reason>` comment) only paths that are never under the library root: `.processing`, spool, users, logs, models, DB, config — the inventory `inv-server-rest.md`/`inv-server-ocr.md` marks them.

Special sites:
- `ocr/collect.rs`: `move_into_place(src, dst, intent: &WriteIntent)` uses `fs::publish` for the rename path; install passes `{CreateNew, Ocr{claim}, Deferred}`; `upgrade.rs:576` passes `{Replace{base: None}, Upgrade, Sync}`. The EXDEV fallback (`collect.rs:206-222`) stays: `fs::publish` from an unmounted path into a LocalStore is `std::fs::rename` and returns the same error.
- `ocr/types.rs:98` `mtime_ns(meta: &Meta)` uses `unix_mtime()` (and today's non-unix branch).
- `thumbs/mod.rs:148-149` `.webp.tmp` publish: `{CreateNew, Thumbs, Deferred}`; `.nocover` stays an `fs::File::create` (local-only marker per D14 later).
- `library/runtime.rs` (or `app.rs`, wherever the library root is known at startup): `let _library_mount = bunko_store::mount(Arc::new(LocalStore::with_translator(library_root, Arc::new(translate))));` held for the server's lifetime (store it in the app state struct that lives as long as the server). The watcher (Task 3) uses the mounted store's `watch`.
- `admin/status.rs:10` statvfs stays std (it measures the disk, not the library store) — mark exempt for M1; RemoteStore's `capacity()` lands in M2.

- [ ] **Step 1:** add `clippy.toml`; run clippy → FAIL list.
- [ ] **Step 2:** apply mapping and special sites; allow-list tests.
- [ ] **Step 3:** `cargo clippy --workspace --all-targets --locked -- -D warnings` clean (also `-p mokuro-bunko --no-default-features`).
- [ ] **Step 4:** `cargo test -p bunko-server` PASS (ocr_sched, ocr_upgrade*, library_compile, dav_path_case, golden).
- [ ] **Step 5: Commit** — `bunko-server: library fs access through bunko-store; LocalStore mounted at startup`.

---

### Task 7: M1 verification

- [ ] **Step 1:** `cargo test --workspace --locked` → PASS. Record the summary line.
- [ ] **Step 2:** differential suites locally (CI skips them): `MOKURO_DIFF_REF=~/.cache/mokuro-bunko-demo/ref052 cargo test -p bunko-server --test differential --test differential_053 -- --nocapture` (check the test files' headers for the exact env var names). Expected PASS; attach the log to the PR.
- [ ] **Step 3:** bench: PROPFIND Depth 1 over 2,000 entries and `build_volume_manifest`, 0.7.3 (`git stash`-free: build `main` in a separate worktree) vs this branch, 10 runs each; within 3 %.
- [ ] **Step 4:** `grep -rn 'std::fs\|tokio::fs' crates/bunko-{library,dav,server}/src | grep -v 'allow(clippy::disallowed_methods)'` reviewed: every remaining hit is an exempt unmounted path with its comment.
- [ ] **Step 5:** CHANGELOG `[Unreleased]` entry: "Internal: library storage goes through a storage seam (no behaviour change)."
- [ ] **Step 6: Commit** and open the M1 PR.
