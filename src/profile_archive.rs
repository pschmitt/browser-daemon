//! Bounded profile imports. Validate the immutable upload before unpacking it:
//! tar extension headers otherwise allocate before the caller sees an entry.
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

const EXPANDED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const ENTRIES: usize = 100_000;
const METADATA_BYTES: u64 = 1024 * 1024;
const IMPORT_TIME: Duration = Duration::from_secs(600);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Cancel {
    flag: Arc<AtomicBool>,
}
impl Drop for Cancel {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

struct Budget<R> {
    inner: R,
    left: u64,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}
impl<R: Read> Read for Budget<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Relaxed) || Instant::now() >= self.deadline {
            return Err(invalid("import cancelled or timed out"));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf
            .len()
            .min(self.left.saturating_add(1) as usize)
            .min(64 * 1024);
        let n = self.inner.read(&mut buf[..len])?;
        if n as u64 > self.left {
            return Err(invalid("expanded archive is too large"));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

fn reader<'a>(
    file: &'a std::fs::File,
    limit: u64,
    deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> Budget<flate2::read::MultiGzDecoder<Budget<&'a std::fs::File>>> {
    // Check the deadline while consuming compressed data as well, including
    // streams of empty gzip members that produce no decompressed output.
    Budget {
        inner: flate2::read::MultiGzDecoder::new(Budget {
            inner: file,
            left: super::MAX_UPLOAD_BYTES as u64,
            deadline,
            cancelled: cancelled.clone(),
        }),
        left: limit,
        deadline,
        cancelled: cancelled.clone(),
    }
}

fn unpack(
    mut file: std::fs::File,
    dir: &Path,
    limit: u64,
    entries: usize,
    deadline: Instant,
    cancelled: &Arc<AtomicBool>,
) -> io::Result<()> {
    // Raw iteration exposes GNU/PAX metadata before tar buffers it. Refuse
    // sparse encodings so a tiny archive cannot declare enormous sparse files.
    let mut archive = tar::Archive::new(reader(&file, limit, deadline, cancelled));
    for (index, entry) in archive.entries()?.raw(true).enumerate() {
        if index >= entries {
            return Err(invalid("too many archive entries"));
        }
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_sparse() {
            return Err(invalid("sparse profile files are unsupported"));
        }
        if kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions()
        {
            if entry.size() > METADATA_BYTES {
                return Err(invalid("archive metadata is too large"));
            }
            let mut metadata = Vec::new();
            entry.read_to_end(&mut metadata)?;
            if metadata
                .windows(b"GNU.sparse.".len())
                .any(|s| s == b"GNU.sparse.")
            {
                return Err(invalid("sparse profile files are unsupported"));
            }
        } else if !kind.is_file() && !kind.is_dir() && !kind.is_symlink() && !kind.is_hard_link() {
            return Err(invalid("unsupported profile archive entry"));
        }
    }
    // Check the gzip checksum, trailing members, and the entire expanded size.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    file.rewind()?;
    let mut archive = tar::Archive::new(reader(&file, limit, deadline, cancelled));
    let mut remaining = limit;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= entries {
            return Err(invalid("too many archive entries"));
        }
        let mut entry = entry?;
        remaining = remaining
            .checked_sub(entry.size())
            .ok_or_else(|| invalid("expanded profile is too large"))?;
        if !entry.unpack_in(dir)? {
            return Err(invalid("archive path escapes the profile"));
        }
    }
    Ok(())
}

// A cancelled HTTP task must not leave an extraction running against a directory
// its caller is deleting. The reader observes cancellation; this guard removes
// any files created in the short interval before it does.
struct WorkerCleanup {
    dir: PathBuf,
    cancelled: Arc<AtomicBool>,
}
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        if self.cancelled.load(Ordering::Relaxed) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

pub(super) async fn extract(body: axum::body::Body, dir: &Path) -> io::Result<u64> {
    let deadline = Instant::now() + IMPORT_TIME;
    // Spool at most the existing compressed upload limit. The unlinked file
    // survives only as long as this request or its extraction worker needs it.
    let mut file = tokio::fs::File::from_std(tempfile::tempfile_in(dir)?);
    let upload = async {
        let mut chunks = Box::pin(super::capped_chunks(body, super::MAX_UPLOAD_BYTES));
        let mut received = 0u64;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            received += chunk.len() as u64;
        }
        file.flush().await?;
        if received == 0 {
            return Err(invalid("empty archive"));
        }
        Ok(received)
    };
    let received = tokio::time::timeout(IMPORT_TIME, upload)
        .await
        .map_err(|_| invalid("import timed out"))??;
    let mut file = file.into_std().await;
    file.rewind()?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = Cancel {
        flag: cancelled.clone(),
    };
    let cleanup = WorkerCleanup {
        dir: dir.to_owned(),
        cancelled: cancelled.clone(),
    };
    tokio::task::spawn_blocking(move || {
        unpack(
            file,
            &cleanup.dir,
            EXPANDED_BYTES,
            ENTRIES,
            deadline,
            &cancelled,
        )
    })
    .await
    .map_err(io::Error::other)??;
    // Marking a completed worker cancelled cannot remove a successful import.
    drop(cancel);
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(files: &[(&str, &[u8])]) -> std::fs::File {
        let file = tempfile::tempfile().unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        let mut file = builder.into_inner().unwrap().finish().unwrap();
        file.rewind().unwrap();
        file
    }

    fn run(
        file: std::fs::File,
        dir: &Path,
        limit: u64,
        entries: usize,
        deadline: Instant,
    ) -> io::Result<()> {
        unpack(
            file,
            dir,
            limit,
            entries,
            deadline,
            &Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn compressed_bombs_and_excess_entries_are_rejected_before_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let bomb = vec![0; 128 * 1024];
        let err = run(
            archive(&[("bomb", &bomb)]),
            dir.path(),
            16 * 1024,
            10,
            Instant::now() + IMPORT_TIME,
        )
        .unwrap_err();
        assert!(err.to_string().contains("too large"));
        assert!(!dir.path().join("bomb").exists());
        let err = run(
            archive(&[("one", b""), ("two", b"")]),
            dir.path(),
            16 * 1024,
            1,
            Instant::now() + IMPORT_TIME,
        )
        .unwrap_err();
        assert!(err.to_string().contains("too many"));
        assert!(!dir.path().join("one").exists());
    }

    #[test]
    fn expired_and_cancelled_readers_stop() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            archive(&[("file", b"value")]),
            dir.path(),
            16 * 1024,
            10,
            Instant::now(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        let err = unpack(
            archive(&[("file", b"value")]),
            dir.path(),
            16 * 1024,
            10,
            Instant::now() + IMPORT_TIME,
            &Arc::new(AtomicBool::new(true)),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn ordinary_long_paths_still_unpack() {
        let dir = tempfile::tempdir().unwrap();
        let path = format!("Default/{}", "a".repeat(140));
        run(
            archive(&[(&path, b"value")]),
            dir.path(),
            16 * 1024,
            10,
            Instant::now() + IMPORT_TIME,
        )
        .unwrap();
        assert_eq!(std::fs::read(dir.path().join(path)).unwrap(), b"value");
    }

    #[test]
    fn sparse_files_are_rejected() {
        let mut file = tempfile::tempfile().unwrap();
        {
            let mut encoder = flate2::write::GzEncoder::new(&mut file, flate2::Compression::fast());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::new(b'S'));
            header.set_size(0);
            header
                .as_gnu_mut()
                .unwrap()
                .set_real_size(EXPANDED_BYTES + 1);
            header.set_cksum();
            encoder.write_all(header.as_bytes()).unwrap();
            encoder.finish().unwrap();
        }
        file.rewind().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            file,
            dir.path(),
            EXPANDED_BYTES,
            ENTRIES,
            Instant::now() + IMPORT_TIME,
        )
        .unwrap_err();
        assert!(err.to_string().contains("sparse"));
    }

    #[test]
    fn oversized_extension_headers_are_rejected_without_buffering() {
        let mut file = tempfile::tempfile().unwrap();
        {
            let mut encoder = flate2::write::GzEncoder::new(&mut file, flate2::Compression::fast());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::GNULongName);
            header.set_size(METADATA_BYTES + 1);
            header.set_cksum();
            encoder.write_all(header.as_bytes()).unwrap();
            encoder.finish().unwrap();
        }
        file.rewind().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            file,
            dir.path(),
            EXPANDED_BYTES,
            ENTRIES,
            Instant::now() + IMPORT_TIME,
        )
        .unwrap_err();
        assert!(err.to_string().contains("metadata is too large"));
    }
}
