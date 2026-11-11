//! The mutation methods of [`RemoteStore`] (spec §3.4, §5.7): staging files,
//! `publish`, moves, deletes, folders, `copy`, and the cloud queue-file writer
//! (§9.1, D10). The ops themselves run in `outbox.rs`.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bunko_store::{
    Durability, File, LibraryStore, Mode, OpId, OpenOptions, Origin, Published, WriteIntent,
    conflict, unavailable,
};

use crate::backend::Backend;
use crate::file::StagingFile;
use crate::index::RelPath;
use crate::names::{fold, is_reserved_name};
use crate::outbox::{BARRIER, Op, OpKind, OpState};
use crate::state::{ConflictRow, random_id};
use crate::status::{MARKER_NAME, QUEUE_DOC_MAX_BYTES, cloud_queue_doc, queue_doc_changed};
use crate::store::{Core, RemoteStore};

/// What a remove may remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Remove {
    File,
    EmptyDir,
    Tree,
}

/// The queue file's name in the merged root.
pub const QUEUE_FILE: &str = ".mokuro-queue.json";

/// A Deferred write of the outbox at or under `rel` (a case merge's steps aside: they
/// run in their group).
fn deferred_under(o: &Op, rel: &RelPath) -> bool {
    o.durability == Durability::Deferred
        && o.group.is_none()
        && o.dest.segments().starts_with(rel.segments())
}

/// The worker is running such a write (`ops` locked: the worker claims under it, and
/// `running` is the op it claimed, 0 for none).
fn running_under(ops: &BTreeMap<u64, Op>, running: u64, rel: &RelPath) -> bool {
    ops.get(&running)
        .is_some_and(|o| deferred_under(o, rel) && o.state.live())
}

fn parent_of(rel: &RelPath) -> RelPath {
    let s = rel.segments();
    RelPath::from_segments(s[..s.len().saturating_sub(1)].iter().cloned()).unwrap_or_default()
}

fn kind_err(kind: io::ErrorKind, p: &Path, what: &str) -> io::Error {
    io::Error::new(kind, format!("{}: {what}", p.display()))
}

fn denied(p: &Path, what: &str) -> io::Error {
    kind_err(io::ErrorKind::PermissionDenied, p, what)
}

/// A staging file under a temporary name (`.<name>.upload-<r>.tmp`,
/// `.<name>.compile-<r>.tmp`, `<name>.json.tmp`, ...): its writer publishes or removes
/// it before it returns, so after a restart nothing will ever publish it.
fn is_temp_name(rel: &RelPath) -> bool {
    rel.segments()
        .last()
        .is_some_and(|n| fold(n).ends_with(".tmp"))
}

/// Reload the staging map from `local_row`; sweep staging files nothing names, and
/// the orphans of writes a crash or kill cut short (temporary names, §5.10).
pub(crate) fn load_staging<B: Backend>(core: &Core<B>) {
    let dir = core.dir.join("staging");
    let mut map = core.staging.lock();
    for (path, file) in core.state.local_rows("staging") {
        let local = dir.join(&file);
        let plain = Path::new(&file).file_name() == Some(std::ffi::OsStr::new(&file));
        match RelPath::parse(&path) {
            Some(rel) if plain && local.is_file() && !is_temp_name(&rel) => {
                map.insert(rel, local);
            }
            Some(rel) if plain && local.is_file() => {
                tracing::info!(path = %rel, "staging: removing an unpublished temporary file");
                core.state.local_del(&path);
            }
            _ => core.state.local_del(&path),
        }
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for f in rd.flatten() {
            if !map.values().any(|l| *l == f.path()) {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
}

impl<B: Backend> Core<B> {
    /// Stage a Deferred Put of the local file `src` at `rel` (no network): the
    /// bytes move into the outbox, the op is STAGED, the index shows it.
    pub(crate) fn stage_put(&self, src: &Path, rel: RelPath, w: &WriteIntent) -> io::Result<Op> {
        let id = self.next_seq();
        let (name, size, sha) = self.take_bytes(src, id)?;
        // The folder the write is for: a replaced folder is a conflict (outbox.rs).
        let parent = self.parent_pid_of(&rel);
        let mut op = Op::new(id, OpKind::Put, rel, w.origin.clone(), w.durability);
        op.mode = w.mode.clone();
        op.parent_pid = parent;
        op.staged = Some(name);
        op.size = size;
        op.sha256 = Some(sha);
        self.save_op(&op)?;
        op.state = OpState::Staged;
        self.save_op(&op)?;
        Ok(op)
    }

    /// Write the cloud queue file now: a coalesced, fenced Deferred Status op.
    pub(crate) fn write_queue_doc(&self, doc: serde_json::Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(&doc).map_err(io::Error::other)?;
        let tmp = self
            .dir
            .join("staging")
            .join(format!("queue-{}", random_id()));
        std::fs::write(&tmp, &bytes)?;
        let rel = RelPath::from_segments([QUEUE_FILE]).expect("a safe name");
        let w = WriteIntent::new(
            Mode::Replace { base: None },
            Origin::Status,
            Durability::Deferred,
        );
        let op = self.stage_put(&tmp, rel, &w)?;
        self.stage_deferred(op)?;
        let mut q = self.queue.lock();
        q.last = Some(doc);
        q.last_write = Some(std::time::Instant::now());
        Ok(())
    }
}

impl<B: Backend> RemoteStore<B> {
    pub(crate) fn staging_file(&self, p: &Path) -> Option<PathBuf> {
        let rel = self.rel(p).ok()?;
        self.core.staging.lock().get(&rel).cloned()
    }

    /// Who may change what (authorization of every mutation through the shim):
    /// nothing may create, move or remove a name Bunko reserves for its transient
    /// files, or the lease marker (only the lease writes it, directly); the cloud
    /// queue file is the [`Origin::Status`] writer's alone. `origin` is `None` for
    /// mutations without an intent (rename, remove, folders, `open_with`). Names
    /// compare folded, as a case-insensitive provider resolves them. Paths outside
    /// the store root never get here (`rel` refuses them).
    pub(crate) fn check_target(
        &self,
        rel: &RelPath,
        p: &Path,
        origin: Option<&Origin>,
    ) -> io::Result<()> {
        if rel.segments().iter().any(|s| is_reserved_name(s)) {
            return Err(denied(
                p,
                "a name Bunko reserves for its own transient files",
            ));
        }
        if let [name] = rel.segments() {
            let name = fold(name);
            if name == MARKER_NAME {
                return Err(denied(p, "the lease marker is written by the lease only"));
            }
            if name == QUEUE_FILE && origin != Some(&Origin::Status) {
                return Err(denied(
                    p,
                    "the queue file is written by Bunko's status writer only",
                ));
            }
        }
        Ok(())
    }

    /// A local file outside the store may be published or copied into it only from
    /// the folders Bunko stages files in ([`RemoteStoreConfig::local_sources`]): a
    /// regular file (no symlink, no extra hard link), owned by the user that owns
    /// the store's folder, and not in the cloud state folder (credentials, the state
    /// and outbox of any store) unless it is this store's own `staging/`.
    ///
    /// [`RemoteStoreConfig::local_sources`]: crate::RemoteStoreConfig::local_sources
    pub(crate) fn check_local_source(&self, src: &Path) -> io::Result<()> {
        let meta = std::fs::symlink_metadata(src)?;
        if !meta.file_type().is_file() {
            return Err(denied(
                src,
                "not a regular file (symlinks are not followed)",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() > 1 {
                return Err(denied(src, "a hard-linked file"));
            }
            if let Ok(d) = std::fs::metadata(&self.cfg.dir)
                && d.uid() != meta.uid()
            {
                return Err(denied(src, "owned by another user"));
            }
        }
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let real = std::fs::canonicalize(src)?;
        if real.starts_with(canon(&self.cfg.dir.join("staging"))) {
            return Ok(());
        }
        let private = self
            .cfg
            .dir
            .parent()
            .filter(|c| c.file_name().is_some_and(|n| n == "cloud"))
            .unwrap_or(&self.cfg.dir);
        if real.starts_with(canon(private)) {
            return Err(denied(src, "inside Bunko's cloud state folder"));
        }
        if self
            .cfg
            .local_sources
            .iter()
            .any(|r| real.starts_with(canon(r)))
        {
            return Ok(());
        }
        Err(denied(src, "outside the folders Bunko stages files in"))
    }

    fn alive(&self) -> io::Result<()> {
        if self.crashed() {
            return Err(io::Error::other("the store stopped at a failpoint"));
        }
        Ok(())
    }

    /// `open` of a pending Deferred write (its staged bytes) or of a staging file.
    pub(crate) fn open_local(&self, p: &Path) -> io::Result<Option<File>> {
        if let Ok(e) = self.lookup(p) {
            if let Some(op) = e.pending_op.as_deref().and_then(|id| self.pending(id))
                && let Some(path) = self.core.staged_path(&op)
            {
                let f = std::fs::File::open(path)?;
                return Ok(Some(File::Remote(Box::new(crate::file::PendingFile::new(
                    f,
                    self.meta(&e),
                )))));
            }
            return Ok(None);
        }
        match self.staging_file(p) {
            Some(local) => std::fs::File::open(local).map(|f| Some(File::Std(f))),
            None => Ok(None),
        }
    }

    /// The local file behind a pending Deferred write or a staging file.
    pub(crate) fn local_bytes(&self, p: &Path) -> io::Result<Option<PathBuf>> {
        if let Ok(e) = self.lookup(p) {
            return Ok(e
                .pending_op
                .as_deref()
                .and_then(|id| self.pending(id))
                .and_then(|op| self.core.staged_path(&op)));
        }
        Ok(self.staging_file(p))
    }

    fn parent_dir_exists(&self, rel: &RelPath, p: &Path) -> io::Result<()> {
        match self.core.index.lock().lookup(&parent_of(rel)) {
            Some(e) if e.is_dir => Ok(()),
            Some(_) => Err(kind_err(
                io::ErrorKind::NotADirectory,
                p,
                "the parent is a file",
            )),
            None => Err(kind_err(
                io::ErrorKind::NotFound,
                p,
                "the parent folder does not exist",
            )),
        }
    }

    /// `open_with`: reads are `open`; anything that writes gets a local staging file.
    pub(crate) fn open_with_impl(&self, p: &Path, o: &OpenOptions) -> io::Result<File> {
        if !(o.write || o.append || o.create || o.create_new || o.truncate) {
            return self.open(p);
        }
        self.alive()?;
        let rel = self.rel(p)?;
        if rel.is_root() {
            return Err(kind_err(io::ErrorKind::IsADirectory, p, "the library root"));
        }
        self.check_target(&rel, p, None)?;
        self.parent_dir_exists(&rel, p)?;
        let indexed = self.core.index.lock().lookup(&rel);
        if indexed.as_ref().is_some_and(|e| e.is_dir) {
            return Err(kind_err(io::ErrorKind::IsADirectory, p, "a folder"));
        }
        let staged = self.core.staging.lock().get(&rel).cloned();
        let exists = indexed.is_some() || staged.is_some();
        if o.create_new && exists {
            return Err(kind_err(io::ErrorKind::AlreadyExists, p, "exists"));
        }
        if !(exists || o.create || o.create_new) {
            return Err(kind_err(io::ErrorKind::NotFound, p, "no such file"));
        }
        let (local, fresh) = match staged {
            Some(l) => (l, false),
            None => {
                let name = random_id();
                (self.core.dir.join("staging").join(name), true)
            }
        };
        if fresh && indexed.is_some() && !o.truncate {
            // Opened to modify: start from the current bytes.
            let mut src = self.open(p)?;
            src.will_read(0, u64::MAX);
            let mut dst = std::fs::File::create(&local)?;
            io::copy(&mut src, &mut dst)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .append(o.append)
            .truncate(o.truncate && !o.append)
            .open(&local)?;
        let fname = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.core
            .state
            .local_put(&rel.to_string(), "staging", &fname);
        self.core.staging.lock().insert(rel, local.clone());
        Ok(File::Remote(Box::new(StagingFile {
            file,
            path: local,
            dest: p.to_path_buf(),
        })))
    }

    /// `publish(staged, dest, intent)` (spec D1, §5.7).
    pub(crate) fn publish_impl(
        &self,
        staged: &Path,
        dest: &Path,
        w: &WriteIntent,
    ) -> io::Result<Published> {
        self.alive()?;
        let rel = self.rel(dest)?;
        if rel.is_root() {
            return Err(kind_err(
                io::ErrorKind::InvalidInput,
                dest,
                "the library root",
            ));
        }
        self.check_target(&rel, dest, Some(&w.origin))?;
        self.parent_dir_exists(&rel, dest)?;
        let existing = self.core.index.lock().lookup(&rel);
        if existing.as_ref().is_some_and(|e| e.is_dir) {
            return Err(kind_err(io::ErrorKind::IsADirectory, dest, "a folder"));
        }
        // The bytes: a staging file of this store, or a local file elsewhere.
        let staging_rel = staged
            .starts_with(&self.cfg.root)
            .then(|| self.rel(staged))
            .transpose()?;
        let src =
            match &staging_rel {
                Some(srel) => {
                    self.core.staging.lock().get(srel).cloned().ok_or_else(|| {
                        kind_err(io::ErrorKind::NotFound, staged, "no staged file")
                    })?
                }
                None => staged.to_path_buf(),
            };
        if !src.is_file() {
            return Err(kind_err(io::ErrorKind::NotFound, staged, "no such file"));
        }
        if staging_rel.is_none() {
            self.check_local_source(&src)?;
        }
        let forget_staging = |s: &Self| {
            if let Some(srel) = &staging_rel {
                s.core.staging.lock().remove(srel);
                s.core.state.local_del(&srel.to_string());
            }
        };
        // A create over a name the index already shows: the origin decides now.
        if w.mode == Mode::CreateNew && existing.is_some() {
            match &w.origin {
                Origin::Ocr { .. } => {
                    self.core.state.conflict_add(
                        ConflictRow::Discarded,
                        &rel.to_string(),
                        existing.as_ref().and_then(|e| e.winner_pid.as_deref()),
                        "discarded: a sidecar appeared in the cloud",
                    );
                    self.core.hooks.ocr_discarded(dest);
                    let _ = std::fs::remove_file(&src);
                    forget_staging(self);
                    return Ok(Published::default());
                }
                Origin::Thumbs => {
                    let _ = std::fs::remove_file(&src);
                    forget_staging(self);
                    return Ok(Published::default());
                }
                _ => return Err(kind_err(io::ErrorKind::AlreadyExists, dest, "exists")),
            }
        }
        // PREPARED: a Compiler base must be own or a submission (D8). `catalog.json`
        // is derived: a foreign version is overwritten (§9.3; its facts went in at
        // first adoption).
        if w.origin == Origin::Compiler
            && let Mode::Replace { base: Some(b) } = &w.mode
            && !b.as_str().starts_with("pending:")
            && !crate::submissions::is_catalog_json(&rel)
        {
            let pid = existing.as_ref().and_then(|e| e.winner_pid.clone());
            let own = pid.is_some_and(|p| self.core.state.is_own(&p, b.as_str()));
            // A captured submission (merged, being merged, or refused) counts as
            // merged: the compile publishes on top of it (§9.3).
            let merged = self
                .core
                .state
                .submission_base_ok(&rel.to_string(), b.as_str());
            if !own && !merged {
                return Err(conflict(format!(
                    "{rel}: the compile base is neither Bunko's own version nor a submitted one"
                )));
            }
        }
        // §5.11: above `outbox_max_gb` a Deferred write is refused (507), so an
        // outage cannot fill the disk; the caller keeps its bytes.
        if w.durability == Durability::Deferred {
            let size = std::fs::metadata(&src)?.len();
            let queued = self.core.outbox_bytes();
            if queued.saturating_add(size) > self.core.outbox_max {
                return Err(kind_err(
                    io::ErrorKind::StorageFull,
                    dest,
                    &format!("the outbox is full ({queued} bytes wait for the provider)"),
                ));
            }
        }
        let op = self.core.stage_put(&src, rel, w)?;
        forget_staging(self);
        match w.durability {
            Durability::Deferred => {
                let op = self.core.stage_deferred(op)?;
                self.core.fail_at("STAGED")?;
                Ok(Published {
                    version: None,
                    sha256: op.sha256,
                    op: Some(OpId(op.id)),
                })
            }
            Durability::Sync => {
                self.core.fail_at("STAGED")?;
                let core = self.core.clone();
                self.block_on("publish", async move { core.run_sync(op).await })
            }
        }
    }

    /// `rename` (spec §3.4): a staging file is published; an entry moves by id.
    pub(crate) fn rename_impl(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.alive()?;
        let (frel, trel) = (self.rel(from)?, self.rel(to)?);
        self.check_target(&frel, from, None)?;
        self.check_target(&trel, to, None)?;
        if frel == trel {
            return Ok(());
        }
        if self.core.staging.lock().contains_key(&frel) {
            return self
                .publish_impl(from, to, &WriteIntent::default_publish())
                .map(|_| ());
        }
        if frel.is_root() || trel.is_root() {
            return Err(kind_err(
                io::ErrorKind::InvalidInput,
                from,
                "the library root",
            ));
        }
        if trel.segments().starts_with(frel.segments()) {
            return Err(kind_err(
                io::ErrorKind::InvalidInput,
                to,
                "a folder cannot move into itself",
            ));
        }
        let deadline = std::time::Instant::now() + BARRIER;
        let src = loop {
            // A write of the source the worker has on its way lands (or is back for a
            // later try) before the source is looked at: an op on its way cannot move.
            self.settle_under(&frel)?;
            let src = self.lookup(from)?;
            self.parent_dir_exists(&trel, to)?;
            // A pending Deferred create that has not left yet: move the op, not a node.
            let pending = match (&src.winner_pid, src.pending_op.as_deref()) {
                (None, Some(id)) => id.parse::<u64>().ok(),
                _ => None,
            };
            let Some(id) = pending else {
                break src;
            };
            if self.core.index.lock().lookup(&trel).is_some() {
                return Err(kind_err(io::ErrorKind::AlreadyExists, to, "exists"));
            }
            // Out of the op map while it moves (the worker claims under that lock): it
            // is sent to the new name, or not yet. One claimed meanwhile is waited out.
            let taken = {
                let mut ops = self.core.ops.lock();
                (self.core.running.load(Ordering::Acquire) != id).then(|| match ops.get(&id) {
                    Some(o) if o.state.live() => ops.remove(&id),
                    _ => None,
                })
            };
            let Some(taken) = taken else {
                self.settle(deadline)?;
                continue;
            };
            let Some(mut op) = taken else {
                break src;
            };
            let cleared = self.core.index.lock().clear_pending(&op.op_id());
            let ch = match cleared {
                Ok(ch) => ch,
                Err(e) => {
                    self.core.ops.lock().insert(op.id, op);
                    return Err(io::Error::other(e));
                }
            };
            self.core.emit_changes(&ch);
            op.dest = trel;
            if let Err(e) = self.core.save_op(&op) {
                // Not lost for this run: it is sent to the new name all the same.
                self.core.ops.lock().insert(op.id, op);
                return Err(e);
            }
            self.core.stage_deferred(op)?;
            return Ok(());
        };
        let Some(pid) = src.winner_pid.clone() else {
            return Err(kind_err(io::ErrorKind::NotFound, from, "no such file"));
        };
        let target = self.core.index.lock().lookup(&trel);
        if let Some(d) = target.filter(|d| d.eid != src.eid) {
            if d.is_dir && !src.is_dir {
                return Err(kind_err(io::ErrorKind::IsADirectory, to, "a folder"));
            }
            if !d.is_dir && src.is_dir {
                return Err(kind_err(io::ErrorKind::NotADirectory, to, "a file"));
            }
            // Replacing the destination: it goes to the trash first (one group, §3.4).
            self.remove_impl(
                to,
                if d.is_dir {
                    Remove::EmptyDir
                } else {
                    Remove::File
                },
            )?;
        }
        let id = self.core.next_seq();
        let mut op = Op::new(id, OpKind::Move, trel, Origin::Admin, Durability::Sync);
        op.src = Some(frel);
        op.target_pid = Some(pid);
        op.state = OpState::Staged;
        self.core.save_op(&op)?;
        self.core.fail_at("STAGED")?;
        let core = self.core.clone();
        self.block_on("rename", async move { core.run_sync(op).await })
            .map(|_| ())
    }

    /// `remove_file` / `remove_dir` / `remove_dir_all`: the provider trash, or a
    /// permanent delete of a file version Bunko wrote (D16).
    pub(crate) fn remove_impl(&self, p: &Path, how: Remove) -> io::Result<()> {
        self.alive()?;
        let rel = self.rel(p)?;
        self.check_target(&rel, p, None)?;
        let staged = self.core.staging.lock().get(&rel).cloned();
        let indexed = self.core.index.lock().lookup(&rel);
        let Some(e) = indexed else {
            if let Some(local) = staged
                && how == Remove::File
            {
                self.core.staging.lock().remove(&rel);
                self.core.state.local_del(&rel.to_string());
                return std::fs::remove_file(local);
            }
            return Err(kind_err(
                io::ErrorKind::NotFound,
                p,
                "no such file or directory",
            ));
        };
        if rel.is_root() {
            return Err(kind_err(
                io::ErrorKind::PermissionDenied,
                p,
                "the library root",
            ));
        }
        let fits = |e: &crate::index::Entry| -> io::Result<()> {
            match how {
                Remove::File if e.is_dir => {
                    Err(kind_err(io::ErrorKind::IsADirectory, p, "a folder"))
                }
                Remove::EmptyDir | Remove::Tree if !e.is_dir => {
                    Err(kind_err(io::ErrorKind::NotADirectory, p, "a file"))
                }
                Remove::EmptyDir if !self.core.index.lock().children(e.eid).is_empty() => {
                    Err(kind_err(io::ErrorKind::DirectoryNotEmpty, p, "not empty"))
                }
                _ => Ok(()),
            }
        };
        fits(&e)?;
        if let Some(local) = staged {
            self.core.staging.lock().remove(&rel);
            self.core.state.local_del(&rel.to_string());
            let _ = std::fs::remove_file(local);
        }
        // Pending Deferred writes at or under the path will never land. One the worker
        // is running there may be on its way: it is waited out, so the entry is looked
        // at again as it stands now (a write may have landed since the lookup above).
        self.cancel_deferred_under(&rel)?;
        let Some(e) = self.core.index.lock().lookup(&rel) else {
            return Ok(()); // a local-only pending entry: cancelled above
        };
        fits(&e)?;
        let Some(pid) = e.winner_pid.clone() else {
            // Still pending: an upload whose outcome is in doubt, settled at its next run.
            return Ok(());
        };
        let own = !e.is_dir
            && e.cver
                .as_ref()
                .is_some_and(|c| self.core.state.is_own(&pid, c));
        let kind = if own { OpKind::Delete } else { OpKind::Trash };
        let id = self.core.next_seq();
        let mut op = Op::new(id, kind, rel, Origin::Admin, Durability::Sync);
        op.target_pid = Some(pid);
        op.state = OpState::Staged;
        self.core.save_op(&op)?;
        self.core.fail_at("STAGED")?;
        let core = self.core.clone();
        self.block_on("remove", async move { core.run_sync(op).await })
            .map(|_| ())
    }

    /// Cancel the Deferred writes at or under `rel` that have not left: STAGED, HELD,
    /// or back from a PREFLIGHT that failed (it never sent a byte). Each is taken out
    /// of the op map under the lock the worker claims an op under, so it is never sent
    /// after this. The one the worker is running there may be on its way to the
    /// provider and cannot be called back: its attempt is waited out
    /// ([`RemoteStore::settle`]) and the rest looked at again (it may have landed, or
    /// be back for a later try).
    fn cancel_deferred_under(&self, rel: &RelPath) -> io::Result<()> {
        let deadline = std::time::Instant::now() + BARRIER;
        loop {
            let (doomed, busy) = {
                let mut ops = self.core.ops.lock();
                let running = self.core.running.load(Ordering::Acquire);
                let busy = running_under(&ops, running, rel);
                let ids: Vec<u64> = ops
                    .values()
                    .filter(|o| {
                        o.id != running
                            && deferred_under(o, rel)
                            && matches!(
                                o.state,
                                OpState::Staged | OpState::Held | OpState::Preflight
                            )
                    })
                    .map(|o| o.id)
                    .collect();
                let doomed: Vec<Op> = ids.iter().filter_map(|id| ops.remove(id)).collect();
                (doomed, busy)
            };
            for mut o in doomed {
                // Deleting the pending file itself is intended; anything under a deleted
                // folder is a lost write.
                if o.dest != *rel {
                    self.core.state.conflict_add(
                        ConflictRow::LostWrite,
                        &o.dest.to_string(),
                        None,
                        "its destination was deleted before it was uploaded",
                    );
                }
                self.core.finish(&mut o, OpState::Superseded);
            }
            if !busy {
                return Ok(());
            }
            self.settle(deadline)?;
        }
    }

    /// Wait until the worker's run of a Deferred write at or under `rel` (if any) is
    /// over: it landed, or is back for a later try.
    fn settle_under(&self, rel: &RelPath) -> io::Result<()> {
        let deadline = std::time::Instant::now() + BARRIER;
        loop {
            let busy = {
                let ops = self.core.ops.lock();
                running_under(&ops, self.core.running.load(Ordering::Acquire), rel)
            };
            if !busy {
                return Ok(());
            }
            self.settle(deadline)?;
        }
    }

    /// Wait out the run the worker is making (it holds the op lock while it runs an
    /// op), at most until `deadline`, as a Sync move or delete waits for in-flight ops
    /// (§5.7 Barrier): then 503. The failpoint `settle` is passed first.
    fn settle(&self, deadline: std::time::Instant) -> io::Result<()> {
        self.core.fail_at("settle")?;
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let core = self.core.clone();
        let settled = self.block_on("settle", async move {
            tokio::time::timeout(left, core.exec.lock()).await.is_ok()
        });
        if !settled {
            return Err(unavailable(
                "a write of the path on its way to the provider did not finish in time",
            ));
        }
        Ok(())
    }

    /// `create_dir` / `create_dir_all`: Sync, idempotent, network only for missing
    /// segments.
    pub(crate) fn create_dir_impl(&self, p: &Path, all: bool) -> io::Result<()> {
        self.alive()?;
        let rel = self.rel(p)?;
        self.check_target(&rel, p, None)?;
        let segs = rel.segments().to_vec();
        if segs.is_empty() {
            return if all {
                Ok(())
            } else {
                Err(kind_err(
                    io::ErrorKind::AlreadyExists,
                    p,
                    "the library root",
                ))
            };
        }
        for k in 1..=segs.len() {
            let prefix =
                RelPath::from_segments(segs[..k].iter().cloned()).expect("checked segments");
            match self.core.index.lock().lookup(&prefix) {
                Some(e) if e.is_dir => {
                    if k == segs.len() && !all {
                        return Err(kind_err(io::ErrorKind::AlreadyExists, p, "exists"));
                    }
                    continue;
                }
                Some(_) => {
                    return Err(kind_err(
                        io::ErrorKind::AlreadyExists,
                        p,
                        "a file is in the way",
                    ));
                }
                None if !all && k < segs.len() => {
                    return Err(kind_err(
                        io::ErrorKind::NotFound,
                        p,
                        "the parent folder does not exist",
                    ));
                }
                None => {}
            }
            let id = self.core.next_seq();
            let mut op = Op::new(id, OpKind::Mkdir, prefix, Origin::Admin, Durability::Sync);
            op.state = OpState::Staged;
            self.core.save_op(&op)?;
            self.core.fail_at("STAGED")?;
            let core = self.core.clone();
            self.block_on("create_dir", async move { core.run_sync(op).await })?;
        }
        Ok(())
    }

    /// `copy(from, to)` into the store: `to` becomes a local staging file with
    /// `from`'s bytes, so the rename that follows publishes it (spec §3.4).
    pub(crate) fn copy_impl(&self, from: &Path, to: &Path) -> io::Result<u64> {
        let mut src: Box<dyn Read> = if from.starts_with(&self.cfg.root) {
            let mut f = self.open(from)?;
            f.will_read(0, u64::MAX);
            Box::new(f)
        } else {
            self.check_local_source(from)?;
            Box::new(std::fs::File::open(from)?)
        };
        let mut o = OpenOptions::new();
        o.write(true).create(true).truncate(true);
        let mut dst = self.open_with_impl(to, &o)?;
        io::copy(&mut src, &mut dst)
    }

    /// Publish the cloud copy of the queue file (§9.1, D10) built from the server's
    /// `build_document` output: written only when it changed (apart from
    /// `generated_at`), at most once per provider floor (a later change waits for
    /// the floor), as a coalesced, fenced Deferred Status op. `true` when a write
    /// was queued or scheduled.
    pub fn publish_queue_doc(&self, server_doc: serde_json::Value) -> io::Result<bool> {
        self.alive()?;
        let doc = cloud_queue_doc(
            &server_doc,
            &self.core.instance_id,
            &self.core.done.lock(),
            QUEUE_DOC_MAX_BYTES,
        );
        let mut q = self.core.queue.lock();
        let unchanged = q
            .last
            .as_ref()
            .is_some_and(|prev| !queue_doc_changed(prev, &doc));
        if unchanged {
            q.pending = None;
            return Ok(false);
        }
        let floor = self.core.queue_floor;
        if let Some(t) = q.last_write
            && t.elapsed() < floor
        {
            q.pending = Some(doc);
            if !q.timer {
                q.timer = true;
                let wait = floor - t.elapsed();
                let core = Arc::downgrade(&self.core);
                self.handle().spawn(async move {
                    tokio::time::sleep(wait).await;
                    let Some(core) = core.upgrade() else { return };
                    let doc = {
                        let mut q = core.queue.lock();
                        q.timer = false;
                        q.pending.take()
                    };
                    if let Some(doc) = doc
                        && let Err(e) = core.write_queue_doc(doc)
                    {
                        tracing::warn!("queue file: {e}");
                    }
                });
            }
            return Ok(true);
        }
        drop(q);
        self.core.write_queue_doc(doc)?;
        Ok(true)
    }

    /// The cloud queue file is written at most this often.
    pub fn queue_floor(&self) -> Duration {
        self.core.queue_floor
    }
}
