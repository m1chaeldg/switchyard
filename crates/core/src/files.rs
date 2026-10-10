//! File operations behind the Files explorer: copying between file systems (local ↔ SFTP),
//! recursive delete, and reading / saving text files with a conflict check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use switchyard_remote::fs::file_name;
use switchyard_remote::{FileEntry, FsError, FtpFs, RemoteFs, S3Fs, SftpFs};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::bus::{OnConflict, SaveError, TextFile, TransferError};

/// Largest file the editor opens.
pub const MAX_EDIT_BYTES: u64 = 5 * 1024 * 1024;
const CHUNK: usize = 256 * 1024;

/// Transfers running at once; the rest wait their turn.
pub const PARALLEL_TRANSFERS: usize = 4;
/// Suffix of a file being written; renamed into place when complete.
pub const PART_SUFFIX: &str = ".swypart";

const RUN: u8 = 0;
const CANCEL: u8 = 1;
const PAUSE: u8 = 2;

/// A transfer's control flag, flipped by Pause and Cancel.
pub(crate) type Control = Arc<AtomicU8>;

/// Open SFTP file systems (one per Host), FTP connections and the transfer queue.
pub(crate) struct Files {
    pub(crate) sftp: tokio::sync::Mutex<HashMap<String, Arc<SftpFs>>>,
    /// FTP / FTPS file systems by file connection id.
    pub(crate) ftp: tokio::sync::Mutex<HashMap<String, Arc<FtpFs>>>,
    /// S3 file systems by file connection id.
    pub(crate) s3: tokio::sync::Mutex<HashMap<String, Arc<S3Fs>>>,
    controls: Mutex<HashMap<u64, Control>>,
    pub(crate) slots: Arc<tokio::sync::Semaphore>,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            sftp: tokio::sync::Mutex::default(),
            ftp: tokio::sync::Mutex::default(),
            s3: tokio::sync::Mutex::default(),
            controls: Mutex::default(),
            slots: Arc::new(tokio::sync::Semaphore::new(PARALLEL_TRANSFERS)),
        }
    }
}

impl Files {
    fn controls(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Control>> {
        self.controls.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn start(&self, id: u64) -> Control {
        let flag = Arc::new(AtomicU8::new(RUN));
        self.controls().insert(id, flag.clone());
        flag
    }

    pub(crate) fn finish(&self, id: u64) {
        self.controls().remove(&id);
    }

    /// Stop and delete the partial file.
    pub(crate) fn cancel(&self, id: u64) {
        if let Some(f) = self.controls().get(&id) {
            f.store(CANCEL, Ordering::SeqCst);
        }
    }

    /// Stop and keep the partial file for a later resume.
    pub(crate) fn pause(&self, id: u64) {
        if let Some(f) = self.controls().get(&id) {
            f.store(PAUSE, Ordering::SeqCst);
        }
    }
}

/// `dir/name` with the separator the target expects (SFTP paths are POSIX).
fn child(dir: &Path, name: &str, posix: bool) -> PathBuf {
    if posix {
        let d = dir.to_string_lossy().replace('\\', "/");
        PathBuf::from(if d.ends_with('/') {
            format!("{d}{name}")
        } else {
            format!("{d}/{name}")
        })
    } else {
        dir.join(name)
    }
}

/// A folder path typed by the user: `~` and `~/…` start at `home`; on POSIX targets
/// `.`, `..`, repeated and trailing slashes are folded away (lexically, like `cd`).
pub(crate) fn expand_path(path: &Path, home: &Path, posix: bool) -> PathBuf {
    let raw = path.to_string_lossy();
    let raw = raw.trim();
    let sep = if posix {
        '/'
    } else {
        std::path::MAIN_SEPARATOR
    };
    let expanded = if raw == "~" {
        home.to_string_lossy().into_owned()
    } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        let h = home.to_string_lossy();
        format!("{}{sep}{rest}", h.trim_end_matches(['/', '\\']))
    } else {
        raw.to_owned()
    };
    if !posix {
        return PathBuf::from(expanded);
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in expanded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    PathBuf::from(format!("/{}", parts.join("/")))
}

fn part_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(PART_SUFFIX);
    PathBuf::from(s)
}

/// `report (1).pdf`, `report (2).pdf`, …
fn numbered(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

async fn exists(fs: &dyn RemoteFs, path: &Path) -> bool {
    fs.stat(path).await.is_ok()
}

/// Total bytes under `path`.
async fn size_of(fs: &dyn RemoteFs, path: &Path, entry: &FileEntry, posix: bool) -> u64 {
    if !entry.is_dir() {
        return entry.size;
    }
    let mut total = 0;
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(list) = fs.list(&dir).await else {
            continue;
        };
        for e in list {
            if e.is_dir() {
                stack.push(child(&dir, &e.name, posix));
            } else {
                total += e.size;
            }
        }
    }
    total
}

struct Copy<'a> {
    src: &'a dyn RemoteFs,
    dst: &'a dyn RemoteFs,
    src_posix: bool,
    dst_posix: bool,
    control: &'a AtomicU8,
    done: u64,
    report: &'a (dyn Fn(u64) + Send + Sync),
    /// Skip files already copied in full by an earlier attempt.
    resuming: bool,
}

impl Copy<'_> {
    fn stopped(&self) -> Option<TransferError> {
        match self.control.load(Ordering::SeqCst) {
            CANCEL => Some(TransferError::Cancelled),
            PAUSE => Some(TransferError::Paused),
            _ => None,
        }
    }

    /// Copy one file through `<to>.swypart`, continuing a partial one, then rename it.
    async fn file(&mut self, from: &Path, to: &Path, size: u64) -> Result<(), TransferError> {
        let fail = |e: FsError| TransferError::Failed(e.to_string());
        let io = |e: std::io::Error| TransferError::Failed(e.to_string());
        // A complete copy from an earlier attempt (folder resume): skip it.
        if let Ok(t) = self.dst.stat(to).await
            && !t.is_dir()
            && t.size == size
            && self.resuming
        {
            self.done += size;
            (self.report)(self.done);
            return Ok(());
        }
        let part = part_of(to);
        // Only a resume continues a partial file: a fresh transfer of a changed source
        // must not keep stale bytes.
        let offset = match self.dst.stat(&part).await {
            Ok(p) if self.resuming && p.size <= size => p.size,
            _ => 0,
        };
        self.done += offset;
        (self.report)(self.done);
        let mut r = self.src.open_read_from(from, offset).await.map_err(fail)?;
        let mut w = self
            .dst
            .open_write_from(&part, offset)
            .await
            .map_err(fail)?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if let Some(stop) = self.stopped() {
                let _ = w.flush().await;
                drop(w);
                if stop == TransferError::Cancelled {
                    // Do not leave half a file behind.
                    let _ = self.dst.delete(&part).await;
                }
                return Err(stop);
            }
            let n = r.read(&mut buf).await.map_err(io)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n]).await.map_err(io)?;
            self.done += n as u64;
            (self.report)(self.done);
        }
        w.shutdown().await.map_err(io)?;
        drop(w);
        // SFTP rename does not replace: remove the old file first.
        if exists(self.dst, to).await {
            self.dst.delete(to).await.map_err(fail)?;
        }
        self.dst.rename(&part, to).await.map_err(fail)?;
        Ok(())
    }

    async fn tree(
        &mut self,
        from: &Path,
        to: &Path,
        entry: &FileEntry,
    ) -> Result<(), TransferError> {
        if !entry.is_dir() {
            return self.file(from, to, entry.size).await;
        }
        if !exists(self.dst, to).await {
            self.dst
                .mkdir(to)
                .await
                .map_err(|e| TransferError::Failed(e.to_string()))?;
        }
        let list = self
            .src
            .list(from)
            .await
            .map_err(|e| TransferError::Failed(e.to_string()))?;
        for e in list {
            // Links that are not directories are copied as the file they point to.
            let (f, t) = (
                child(from, &e.name, self.src_posix),
                child(to, &e.name, self.dst_posix),
            );
            Box::pin(self.tree(&f, &t, &e)).await?;
        }
        Ok(())
    }
}

/// Copy `path` (file or folder) from `src` into the folder `dir` on `dst`.
///
/// Files are written as `<name>.swypart` and renamed when complete. Pausing or failing
/// keeps the partial file; `resume` continues from it (and skips files already copied),
/// so an interrupted transfer — even one from a previous run of the app — picks up at
/// its last byte. Cancelling deletes it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn transfer(
    src: &dyn RemoteFs,
    src_posix: bool,
    path: &Path,
    dst: &dyn RemoteFs,
    dst_posix: bool,
    dir: &Path,
    on_conflict: OnConflict,
    resume: bool,
    control: &AtomicU8,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<PathBuf, TransferError> {
    let entry = src
        .stat(path)
        .await
        .map_err(|e| TransferError::Failed(e.to_string()))?;
    let name = file_name(path);
    let mut target = child(dir, &name, dst_posix);
    if !resume
        && on_conflict == OnConflict::Ask
        && !entry.is_dir()
        && let Ok(part) = dst.stat(&part_of(&target)).await
        && part.size > 0
        && part.size <= entry.size
    {
        return Err(TransferError::Partial(part.size));
    }
    if !resume && exists(dst, &target).await {
        match on_conflict {
            OnConflict::Ask => return Err(TransferError::Exists(name)),
            OnConflict::Replace => {}
            OnConflict::KeepBoth => {
                let mut n = 1;
                while exists(dst, &target).await {
                    target = child(dir, &numbered(&name, n), dst_posix);
                    n += 1;
                }
            }
        }
    }
    let total = size_of(src, path, &entry, src_posix).await;
    progress(0, Some(total));
    let last = std::sync::atomic::AtomicU64::new(0);
    let report = |done: u64| {
        // Every ~1 MB, so the bus is not flooded.
        if done.saturating_sub(last.load(Ordering::Relaxed)) >= 1 << 20 {
            last.store(done, Ordering::Relaxed);
            progress(done, Some(total));
        }
    };
    let mut copy = Copy {
        src,
        dst,
        src_posix,
        dst_posix,
        control,
        done: 0,
        report: &report,
        resuming: resume,
    };
    let r = copy.tree(path, &target, &entry).await;
    progress(copy.done, Some(total));
    r.map(|()| target)
}

/// Delete a file, or a folder and everything in it.
pub(crate) async fn delete_tree(
    fs: &dyn RemoteFs,
    path: &Path,
    posix: bool,
) -> Result<(), FsError> {
    let entry = fs.stat(path).await;
    if let Ok(e) = &entry
        && e.is_dir()
    {
        // `stat` follows links: never recurse into a linked folder, just remove the link.
        let is_link = fs
            .list(path.parent().unwrap_or(Path::new("/")))
            .await
            .ok()
            .and_then(|l| l.into_iter().find(|x| x.name == file_name(path)))
            .is_some_and(|x| matches!(x.kind, switchyard_remote::EntryKind::Symlink(_)));
        if !is_link {
            for c in fs.list(path).await? {
                Box::pin(delete_tree(fs, &child(path, &c.name, posix), posix)).await?;
            }
        }
    }
    fs.delete(path).await
}

/// Read a text file for the editor.
pub(crate) async fn read_text(fs: &dyn RemoteFs, path: &Path) -> Result<TextFile, String> {
    let meta = fs.stat(path).await.map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err(format!("{} is a folder", meta.name));
    }
    let bytes = fs
        .read_file(path, MAX_EDIT_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        return Err(format!("{} looks like a binary file", meta.name));
    }
    let content =
        String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", meta.name))?;
    Ok(TextFile {
        content,
        modified_ms: meta.modified_ms,
    })
}

/// Save a text file unless it changed since `expect` (SFTP times are whole seconds).
pub(crate) async fn write_text(
    fs: &dyn RemoteFs,
    path: &Path,
    content: &str,
    expect: Option<i64>,
    force: bool,
) -> Result<Option<i64>, SaveError> {
    if !force {
        match fs.stat(path).await {
            Ok(now) if now.modified_ms != expect => {
                return Err(SaveError::Conflict(now.modified_ms));
            }
            Ok(_) => {}
            // Deleted meanwhile: saving recreates it.
            Err(_) => {}
        }
    }
    fs.write_file(path, content.as_bytes())
        .await
        .map_err(|e| SaveError::Failed(e.to_string()))?;
    Ok(fs.stat(path).await.ok().and_then(|m| m.modified_ms))
}

/// Statistics from a folder sync operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Number of files scanned and compared.
    pub files_checked: usize,
    /// Number of files copied.
    pub files_transferred: usize,
    /// Total bytes copied.
    pub bytes_transferred: u64,
}

/// Helper to scan a folder recursively, returning relative paths and entries.
async fn collect_file_tree(
    fs: &dyn RemoteFs,
    posix: bool,
    base: &Path,
    rel: &Path,
) -> Result<Vec<(PathBuf, FileEntry)>, FsError> {
    let full = if rel.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        child(base, &rel.to_string_lossy().replace('\\', "/"), posix)
    };

    let entries = fs.list(&full).await?;
    let mut out = Vec::new();
    for entry in entries {
        let child_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(&entry.name)
        } else if posix {
            PathBuf::from(format!("{}/{}", rel.to_string_lossy().replace('\\', "/"), entry.name))
        } else {
            rel.join(&entry.name)
        };

        if entry.is_dir() {
            let mut sub = Box::pin(collect_file_tree(fs, posix, base, &child_rel)).await?;
            out.append(&mut sub);
        } else {
            out.push((child_rel, entry));
        }
    }
    Ok(out)
}

/// Synchronize a folder from `src` to `dst` directionally, only copying missing or newer/different files.
pub(crate) async fn sync_folder(
    src: &dyn RemoteFs,
    src_posix: bool,
    src_dir: &Path,
    dst: &dyn RemoteFs,
    dst_posix: bool,
    dst_dir: &Path,
    control: &AtomicU8,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<SyncReport, TransferError> {
    let fail = |e: FsError| TransferError::Failed(e.to_string());
    let src_files = collect_file_tree(src, src_posix, src_dir, Path::new("")).await.map_err(fail)?;

    let mut report = SyncReport {
        files_checked: src_files.len(),
        files_transferred: 0,
        bytes_transferred: 0,
    };

    for (rel_path, src_entry) in src_files {
        match control.load(Ordering::SeqCst) {
            CANCEL => return Err(TransferError::Cancelled),
            PAUSE => return Err(TransferError::Paused),
            _ => {}
        }

        let rel_str = rel_path.to_string_lossy().replace('\\', "/");
        let dst_target = child(dst_dir, &rel_str, dst_posix);

        // Check if destination file exists and whether it needs copying
        let needs_copy = match dst.stat(&dst_target).await {
            Ok(dst_entry) => {
                if dst_entry.is_dir() {
                    true
                } else if dst_entry.size != src_entry.size {
                    true
                } else {
                    match (src_entry.modified_ms, dst_entry.modified_ms) {
                        (Some(sm), Some(dm)) => sm > dm,
                        _ => false,
                    }
                }
            }
            Err(_) => true,
        };

        if needs_copy {
            // Ensure parent directory exists on dst
            if let Some(parent) = rel_path.parent() {
                let parent_str = parent.to_string_lossy().replace('\\', "/");
                if !parent_str.is_empty() {
                    let parent_target = child(dst_dir, &parent_str, dst_posix);
                    if !exists(dst, &parent_target).await {
                        let _ = dst.mkdir(&parent_target).await;
                    }
                }
            }

            let src_target = child(src_dir, &rel_str, src_posix);
            let mut copy = Copy {
                src,
                dst,
                src_posix,
                dst_posix,
                control,
                done: report.bytes_transferred,
                report: &|done| progress(done, None),
                resuming: false,
            };

            copy.file(&src_target, &dst_target, src_entry.size).await?;
            report.files_transferred += 1;
            report.bytes_transferred += src_entry.size;
            progress(report.bytes_transferred, None);
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use switchyard_remote::LocalFs;

    #[test]
    fn typed_paths() {
        let home = Path::new("/home/swy");
        let e = |p: &str| expand_path(Path::new(p), home, true);
        assert_eq!(e("~"), PathBuf::from("/home/swy"));
        assert_eq!(e("~/app/"), PathBuf::from("/home/swy/app"));
        assert_eq!(
            e("/srv//www/./site/../logs/"),
            PathBuf::from("/srv/www/logs")
        );
        assert_eq!(e("/.."), PathBuf::from("/"));
        assert_eq!(e("  /etc "), PathBuf::from("/etc"));
    }

    #[test]
    fn names() {
        assert_eq!(numbered("report.pdf", 1), "report (1).pdf");
        assert_eq!(numbered("Makefile", 2), "Makefile (2)");
        assert_eq!(numbered(".bashrc", 1), ".bashrc (1)");
        assert_eq!(
            child(Path::new("/srv/app"), "x", true),
            PathBuf::from("/srv/app/x")
        );
    }

    fn run() -> AtomicU8 {
        AtomicU8::new(RUN)
    }

    #[tokio::test]
    async fn copies_folders_with_conflict_policies_and_cancel() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"aaa").unwrap();
        std::fs::write(src.join("sub/b.txt"), vec![7u8; 3_000_000]).unwrap();
        let out = t.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let fs = LocalFs;
        let seen = Mutex::new(Vec::new());
        let progress = |d: u64, t: Option<u64>| seen.lock().unwrap().push((d, t));
        let go = |path: PathBuf, dir: PathBuf, c: OnConflict, ctl: AtomicU8| {
            let progress = &progress;
            async move {
                let fs = LocalFs;
                transfer(
                    &fs, false, &path, &fs, false, &dir, c, false, &ctl, progress,
                )
                .await
            }
        };

        let to = go(src.clone(), out.clone(), OnConflict::Ask, run())
            .await
            .unwrap();
        assert_eq!(to, out.join("src"));
        assert_eq!(
            std::fs::read(out.join("src/sub/b.txt")).unwrap().len(),
            3_000_000
        );
        assert!(
            !out.join("src/sub/b.txt.swypart").exists(),
            "renamed into place"
        );
        assert_eq!(
            seen.lock().unwrap().last(),
            Some(&(3_000_003, Some(3_000_003)))
        );

        let again = go(src.clone(), out.clone(), OnConflict::Ask, run()).await;
        assert_eq!(again, Err(TransferError::Exists("src".into())));
        let a = src.join("a.txt");
        assert_eq!(
            go(a.clone(), out.clone(), OnConflict::KeepBoth, run())
                .await
                .unwrap(),
            out.join("a.txt")
        );
        assert_eq!(
            go(a.clone(), out.clone(), OnConflict::KeepBoth, run())
                .await
                .unwrap(),
            out.join("a (1).txt")
        );
        // Replace swaps the file in.
        std::fs::write(&a, b"new").unwrap();
        go(a.clone(), out.clone(), OnConflict::Replace, run())
            .await
            .unwrap();
        assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"new");

        let r = go(
            src.join("sub/b.txt"),
            out.join("src"),
            OnConflict::Replace,
            AtomicU8::new(CANCEL),
        )
        .await;
        assert_eq!(r, Err(TransferError::Cancelled));
        assert!(
            !out.join("src/b.txt.swypart").exists(),
            "partial file removed"
        );

        delete_tree(&fs, &out.join("src"), false).await.unwrap();
        assert!(!out.join("src").exists());
    }

    /// A transfer killed part-way (state on disk only) resumes from its last byte.
    #[tokio::test]
    async fn resumes_from_the_last_byte() {
        let t = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..5_000_000u32).map(|i| (i % 253) as u8).collect();
        let src = t.path().join("big.bin");
        std::fs::write(&src, &data).unwrap();
        let out = t.path().join("out");
        std::fs::create_dir(&out).unwrap();
        // What a killed process leaves behind: the first 2 MB in the partial file.
        std::fs::write(out.join("big.bin.swypart"), &data[..2_000_000]).unwrap();
        let fs = LocalFs;
        let seen = Mutex::new(Vec::new());
        let progress = |d: u64, _t: Option<u64>| seen.lock().unwrap().push(d);
        transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Ask,
            true,
            &run(),
            &progress,
        )
        .await
        .unwrap();
        assert!(
            std::fs::read(out.join("big.bin")).unwrap() == data,
            "identical"
        );
        assert!(!out.join("big.bin.swypart").exists());
        // Reported 0, then jumped to the resume point: nothing before it was read again.
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen[0], 0);
            assert!(seen[1] >= 2_000_000, "{:?}", &seen[..3]);
        }

        // Pause keeps the part; resume finishes it.
        std::fs::remove_file(out.join("big.bin")).unwrap();
        let paused = transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Ask,
            false,
            &AtomicU8::new(PAUSE),
            &|_, _| {},
        )
        .await;
        assert_eq!(paused, Err(TransferError::Paused));
        assert!(out.join("big.bin.swypart").exists(), "partial file kept");
        transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Ask,
            true,
            &run(),
            &|_, _| {},
        )
        .await
        .unwrap();
        assert!(std::fs::read(out.join("big.bin")).unwrap() == data);
    }

    /// A fresh transfer that finds an interrupted one asks before discarding it.
    #[tokio::test]
    async fn leftover_partial_is_offered_for_resume() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("db.dump");
        std::fs::write(&src, vec![9u8; 1000]).unwrap();
        let out = t.path().join("out");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("db.dump.swypart"), vec![9u8; 400]).unwrap();
        let fs = LocalFs;
        let r = transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Ask,
            false,
            &run(),
            &|_, _| {},
        )
        .await;
        assert_eq!(r, Err(TransferError::Partial(400)));
        // Start over: Replace ignores (truncates) the partial file.
        transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Replace,
            false,
            &run(),
            &|_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(out.join("db.dump")).unwrap().len(), 1000);
    }

    /// Resuming a folder skips files that were already complete.
    #[tokio::test]
    async fn folder_resume_skips_finished_files() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("site");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("done.txt"), b"12345").unwrap();
        std::fs::write(src.join("half.txt"), b"abcdefgh").unwrap();
        let out = t.path().join("out");
        std::fs::create_dir_all(out.join("site")).unwrap();
        std::fs::write(out.join("site/done.txt"), b"12345").unwrap();
        std::fs::write(out.join("site/half.txt.swypart"), b"abcd").unwrap();
        let fs = LocalFs;
        let ok = transfer(
            &fs,
            false,
            &src,
            &fs,
            false,
            &out,
            OnConflict::Ask,
            true,
            &run(),
            &|_, _| {},
        )
        .await;
        assert_eq!(ok, Ok(out.join("site")));
        assert_eq!(
            std::fs::read(out.join("site/half.txt")).unwrap(),
            b"abcdefgh"
        );
    }

    #[tokio::test]
    async fn save_detects_changes_made_elsewhere() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("app.conf");
        std::fs::write(&f, "port=1\n").unwrap();
        let fs = LocalFs;
        let opened = read_text(&fs, &f).await.unwrap();
        assert_eq!(opened.content, "port=1\n");
        let saved = write_text(&fs, &f, "port=2\n", opened.modified_ms, false)
            .await
            .unwrap();
        // Someone else edits it (a different mtime).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, "port=3\n").unwrap();
        let r = write_text(&fs, &f, "port=4\n", saved, false).await;
        assert!(matches!(r, Err(SaveError::Conflict(_))), "{r:?}");
        write_text(&fs, &f, "port=4\n", saved, true).await.unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "port=4\n");

        std::fs::write(t.path().join("bin"), [0u8, 1, 2]).unwrap();
        assert!(
            read_text(&fs, &t.path().join("bin"))
                .await
                .unwrap_err()
                .contains("binary")
        );
    }

    #[tokio::test]
    async fn sync_folder_directional_logic() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("source");
        let dst = t.path().join("dest");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("nested/b.txt"), b"world 123").unwrap();

        let fs = LocalFs;
        let control = TransferControl::default();
        let report = sync_folder(
            &fs,
            false,
            &src,
            &fs,
            false,
            &dst,
            &control,
            &|_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(report.files_copied, 2);
        assert_eq!(report.files_skipped, 0);
        assert_eq!(report.bytes_transferred, 14);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("nested/b.txt")).unwrap(), b"world 123");

        // Sync again without changes - should skip both files
        let report2 = sync_folder(
            &fs,
            false,
            &src,
            &fs,
            false,
            &dst,
            &control,
            &|_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(report2.files_copied, 0);
        assert_eq!(report2.files_skipped, 2);

        // Update one file in source
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(src.join("a.txt"), b"hello updated!").unwrap();

        let report3 = sync_folder(
            &fs,
            false,
            &src,
            &fs,
            false,
            &dst,
            &control,
            &|_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(report3.files_copied, 1);
        assert_eq!(report3.files_skipped, 1);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello updated!");
    }
}
