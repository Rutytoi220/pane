//! Remote file explorer: directory listings and chunked downloads.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use pane_proto::{DirEntry, FILE_CHUNK_SIZE, HostMsg, MAX_TEXT_PREVIEW_BYTES};

use crate::outbox::Outbox;

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand `""`, `~` and `~/…` relative to the host user's home directory.
pub fn resolve(path: &str) -> PathBuf {
    let path = path.trim();
    if path.is_empty() || path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        let p = PathBuf::from(path);
        if p.is_absolute() { p } else { home_dir().join(p) }
    }
}

/// List `path`, returning its canonical absolute form and the sorted entries
/// (directories first, then case-insensitive by name).
pub fn list_dir(path: &str) -> Result<(String, Vec<DirEntry>), String> {
    let dir = resolve(path);
    let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
    let read = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let mut entries: Vec<DirEntry> = read
        .filter_map(Result::ok)
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let link_meta = e.metadata().ok(); // does not follow symlinks
            let is_symlink = link_meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
            let meta = if is_symlink { fs::metadata(e.path()).ok().or(link_meta) } else { link_meta };
            DirEntry {
                name,
                is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                is_symlink,
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified_unix: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
            }
        })
        .collect();
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok((dir.to_string_lossy().into_owned(), entries))
}

/// Stream `path` to the client in 64 KiB [`HostMsg::FileChunk`]s on a
/// background thread, using the bounded bulk queue for back-pressure.
pub fn spawn_download(req_id: u64, path: String, outbox: Outbox) {
    std::thread::Builder::new()
        .name(format!("download-{req_id}"))
        .spawn(move || {
            let path = resolve(&path);
            if let Err(message) = stream_file(req_id, &path, &outbox) {
                log!("download {req_id} ({}) failed: {message}", path.display());
                outbox.send(HostMsg::Error { req_id: Some(req_id), message });
            }
        })
        .expect("spawn download thread");
}

fn stream_file(req_id: u64, path: &Path, outbox: &Outbox) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let meta = fs::metadata(path).map_err(err)?;
    if meta.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    let total_size = meta.len();
    let mut file = File::open(path).map_err(err)?;
    log!("download {req_id}: {} ({total_size} bytes)", path.display());

    let mut offset = 0u64;
    let mut buf = vec![0u8; FILE_CHUNK_SIZE];
    loop {
        let n = read_full(&mut file, &mut buf).map_err(err)?;
        // A short read means we hit EOF; a full chunk may or may not be the last.
        let eof = n < buf.len();
        let chunk = HostMsg::FileChunk {
            req_id,
            offset,
            total_size: total_size.max(offset + n as u64),
            data: buf[..n].to_vec(),
            eof,
        };
        if !outbox.send_bulk(chunk) {
            return Ok(()); // client gone
        }
        offset += n as u64;
        if eof {
            return Ok(());
        }
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Read up to [`MAX_TEXT_PREVIEW_BYTES`] (256 KiB) of a UTF-8 text file for
/// quick remote inspection in the client's File Viewer.
pub fn read_text_file(path: &str) -> Result<(String, String, bool), String> {
    let resolved = resolve(path);
    let canonical = resolved.canonicalize().map_err(|e| format!("{}: {e}", resolved.display()))?;
    let meta = fs::metadata(&canonical).map_err(|e| format!("{}: {e}", canonical.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a directory", canonical.display()));
    }
    let mut file = File::open(&canonical).map_err(|e| format!("{}: {e}", canonical.display()))?;
    let mut buf = vec![0u8; MAX_TEXT_PREVIEW_BYTES + 1];
    let n = read_full(&mut file, &mut buf).map_err(|e| format!("{}: {e}", canonical.display()))?;
    let truncated = n > MAX_TEXT_PREVIEW_BYTES;
    buf.truncate(n.min(MAX_TEXT_PREVIEW_BYTES));

    if buf.iter().take(8192).any(|&b| b == 0) {
        return Err(format!("{} appears to be a binary file (use [DL] to download)", canonical.display()));
    }

    // If we truncated mid-character, trim the incomplete trailing UTF-8 sequence.
    let mut valid_len = buf.len();
    if truncated {
        while valid_len > 0 && std::str::from_utf8(&buf[..valid_len]).is_err() && buf.len() - valid_len < 4 {
            valid_len -= 1;
        }
    }
    let content = String::from_utf8_lossy(&buf[..valid_len]).into_owned();
    Ok((canonical.to_string_lossy().into_owned(), content, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_directories_first() {
        let tmp = std::env::temp_dir().join(format!("pane-host-test-{}", std::process::id()));
        fs::create_dir_all(tmp.join("zdir")).unwrap();
        fs::write(tmp.join("Afile.txt"), b"hello").unwrap();
        let (path, entries) = list_dir(tmp.to_str().unwrap()).unwrap();
        assert!(Path::new(&path).is_absolute());
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["zdir", "Afile.txt"]);
        assert_eq!(entries[1].size, 5);
        fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn tilde_resolves_to_home() {
        assert_eq!(resolve("~"), home_dir());
        assert_eq!(resolve(""), home_dir());
        assert_eq!(resolve("~/x"), home_dir().join("x"));
        assert_eq!(resolve("/etc"), PathBuf::from("/etc"));
    }

    #[test]
    fn read_text_file_handles_small_truncated_and_binary() {
        let tmp = std::env::temp_dir().join(format!("pane-host-preview-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        let small = tmp.join("config.yml");
        fs::write(&small, "version: '3'\nservices:\n  web:\n    image: nginx\n").unwrap();
        let (_, text, trunc) = read_text_file(small.to_str().unwrap()).unwrap();
        assert!(!trunc);
        assert!(text.contains("image: nginx"));

        let huge = tmp.join("big.log");
        fs::write(&huge, "A".repeat(MAX_TEXT_PREVIEW_BYTES + 1024)).unwrap();
        let (_, text2, trunc2) = read_text_file(huge.to_str().unwrap()).unwrap();
        assert!(trunc2);
        assert_eq!(text2.len(), MAX_TEXT_PREVIEW_BYTES);

        let bin = tmp.join("data.bin");
        fs::write(&bin, b"\x7fELF\x00\x01\x02").unwrap();
        assert!(read_text_file(bin.to_str().unwrap()).is_err());

        fs::remove_dir_all(tmp).unwrap();
    }
}
