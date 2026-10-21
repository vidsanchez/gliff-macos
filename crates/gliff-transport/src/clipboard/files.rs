//! Files on the clipboard: listing what a local `text/uri-list` points at for
//! an offer, and spooling a peer's files to disk when something pastes them.

use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gliff_proto::clipboard::{
    safe_relative_path, top_level, validate_files, FilesError, MAX_FILES_TOTAL, MAX_FILE_ENTRIES,
};
use gliff_proto::{ClipboardFile, ClipboardItem};

use super::progress::Meter;
use super::{ChunkSink, TransferError, Transfers, WriteSink};

/// The files behind a local offer: the entries sent to the peer and, at the
/// same index, the local path each one is served from.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LocalFiles {
    pub entries: Vec<ClipboardFile>,
    pub paths: Vec<PathBuf>,
}

impl LocalFiles {
    pub fn path_for(&self, item: &ClipboardItem) -> Option<&Path> {
        match item {
            ClipboardItem::File(i) => self.paths.get(*i as usize).map(PathBuf::as_path),
            ClipboardItem::Mime(_) => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error(transparent)]
    Files(#[from] FilesError),
    #[error("{0}: {1}")]
    Io(PathBuf, io::Error),
    #[error("{0} has no file name")]
    NoName(PathBuf),
}

/// Walk the copied paths. Directories are listed recursively; symlinks inside
/// them are skipped so a loop cannot run the walk forever. Fails when the
/// caps are exceeded, so a huge selection is not offered at all rather than
/// offered in part.
pub fn list_files(roots: &[PathBuf]) -> Result<LocalFiles, ListError> {
    let mut out = LocalFiles::default();
    let mut total = 0u64;
    for root in roots {
        let name = root
            .file_name()
            .ok_or_else(|| ListError::NoName(root.clone()))?;
        let meta = std::fs::metadata(root).map_err(|e| ListError::Io(root.clone(), e))?;
        walk(
            root,
            Path::new(name),
            meta.is_dir(),
            meta.len(),
            &mut out,
            &mut total,
        )?;
    }
    validate_files(&out.entries)?;
    Ok(out)
}

fn walk(
    path: &Path,
    rel: &Path,
    is_dir: bool,
    size: u64,
    out: &mut LocalFiles,
    total: &mut u64,
) -> Result<(), ListError> {
    if out.entries.len() >= MAX_FILE_ENTRIES {
        return Err(FilesError::TooMany(out.entries.len() + 1).into());
    }
    let rel_str = rel
        .to_str()
        .ok_or_else(|| FilesError::BadPath(rel.to_string_lossy().into_owned()))?
        .to_string();
    if is_dir {
        out.entries.push(ClipboardFile {
            path: rel_str,
            size: 0,
            dir: true,
        });
        out.paths.push(path.to_path_buf());
        let dir = std::fs::read_dir(path).map_err(|e| ListError::Io(path.to_path_buf(), e))?;
        for entry in dir {
            let entry = entry.map_err(|e| ListError::Io(path.to_path_buf(), e))?;
            let ty = entry
                .file_type()
                .map_err(|e| ListError::Io(entry.path(), e))?;
            if ty.is_symlink() {
                continue;
            }
            let size = if ty.is_dir() {
                0
            } else {
                entry
                    .metadata()
                    .map_err(|e| ListError::Io(entry.path(), e))?
                    .len()
            };
            walk(
                &entry.path(),
                &rel.join(entry.file_name()),
                ty.is_dir(),
                size,
                out,
                total,
            )?;
        }
    } else {
        *total = total.saturating_add(size);
        if *total > MAX_FILES_TOTAL {
            return Err(FilesError::TooLarge(*total).into());
        }
        out.entries.push(ClipboardFile {
            path: rel_str,
            size,
            dir: false,
        });
        out.paths.push(path.to_path_buf());
    }
    Ok(())
}

/// A directory that holds one offer's files on the pasting side. Removed when
/// dropped, i.e. when the peer's offer is replaced or the session ends.
pub use super::spool::Spool;

/// How long a replaced offer's spool stays on disk, so an application that
/// was just handed a URI list into it can still open the files.
pub const SPOOL_GRACE: Duration = Duration::from_secs(300);

/// Keep a spool alive for [`SPOOL_GRACE`] after its offer is replaced. The
/// end of the session drops the task, and with it the spool, earlier.
pub fn retire(spool: Rc<Spool>) {
    tokio::task::spawn_local(async move {
        tokio::time::sleep(SPOOL_GRACE).await;
        drop(spool);
    });
}

/// Stream every entry of the peer's offer into `spool` and return the
/// absolute paths of the top-level entries, in offer order, for the URI list.
/// Entries are validated first; any bad path fails the whole paste.
pub async fn fetch_files(
    transfers: &Transfers,
    files: &[ClipboardFile],
    serial: u32,
    spool: &Spool,
    meter: &Meter,
) -> Result<Vec<PathBuf>, TransferError> {
    validate_files(files).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    for (i, f) in files.iter().enumerate() {
        let rel = safe_relative_path(&f.path).expect("validated");
        let dest = spool.dir().join(rel);
        if f.dir {
            tokio::fs::create_dir_all(&dest).await?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::File::create(&dest).await?;
        let got = transfers
            .fetch(
                ClipboardItem::File(i as u32),
                serial,
                meter.wrap(WriteSink(file)),
                Some(f.size),
            )
            .await?;
        if got != f.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: got {got} of {} bytes", f.path, f.size),
            )
            .into());
        }
    }
    Ok(top_level(files)
        .map(|f| {
            spool
                .dir()
                .join(safe_relative_path(&f.path).expect("validated"))
        })
        .collect())
}

/// Serve one of our offered files to the peer.
pub async fn open_source(path: &Path) -> io::Result<super::ReadSource<tokio::fs::File>> {
    Ok(super::ReadSource(tokio::fs::File::open(path).await?))
}

/// Write a whole in-memory body through a sink (a paste target), used for the
/// regenerated URI list.
pub async fn write_body<S: ChunkSink>(mut sink: S, body: String) -> io::Result<()> {
    sink.write(body.into_bytes().into()).await?;
    sink.finish().await
}
