//! Files on the clipboard, as RDP moves them ([MS-RDPECLIP] 1.3.2.2.3; what `mstsc` does).
//!
//! - Our files to the client: the copied paths become a list of file descriptors (folders
//!   walked, names relative to the copied items) that IronRDP offers as
//!   `FileGroupDescriptorW`; the client then reads each file by ranges
//!   (`FileContentsRequest`), answered from disk.
//! - The client's files to us: its descriptor list is mirrored under a fresh folder in the
//!   cache, and each file is fetched range by range, straight to disk; the session gets the
//!   top-level paths (`text/uri-list` in GNOME) once everything has arrived.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor,
};

/// Bytes asked for per request (`mstsc` asks for up to a few hundred kB at a time).
pub const CHUNK: u32 = 1 << 20;
/// Entries walked at most when offering a folder.
const MAX_ENTRIES: usize = 50_000;

/// Our copied files and folders, flattened for the client.
#[derive(Default)]
pub struct Offer {
    pub descriptors: Vec<FileDescriptor>,
    /// The local file behind each descriptor (folders included, never read).
    pub paths: Vec<PathBuf>,
}

impl Offer {
    pub fn new(items: &[PathBuf]) -> Result<Self> {
        let mut offer = Self::default();
        for item in items {
            let name = item
                .file_name()
                .with_context(|| format!("{} has no name", item.display()))?
                .to_string_lossy()
                .into_owned();
            offer.add(item, None, name)?;
        }
        Ok(offer)
    }

    fn add(&mut self, path: &Path, parent: Option<&str>, name: String) -> Result<()> {
        if self.paths.len() >= MAX_ENTRIES {
            bail!("more than {MAX_ENTRIES} files");
        }
        let meta = std::fs::metadata(path).with_context(|| format!("{}", path.display()))?;
        let mut d = FileDescriptor::new(name.clone())
            .with_last_write_time(meta.modified().map(filetime).unwrap_or(0));
        if let Some(p) = parent {
            d = d.with_relative_path(p.to_owned());
        }
        if meta.is_dir() {
            self.descriptors
                .push(d.with_attributes(ClipboardFileAttributes::DIRECTORY));
            self.paths.push(path.to_owned());
            let inside = match parent {
                Some(p) => format!("{p}\\{name}"),
                None => name,
            };
            let mut children: Vec<_> = std::fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let child_name = child.file_name().to_string_lossy().into_owned();
                self.add(&child.path(), Some(&inside), child_name)?;
            }
        } else {
            self.descriptors.push(
                d.with_attributes(ClipboardFileAttributes::ARCHIVE)
                    .with_file_size(meta.len()),
            );
            self.paths.push(path.to_owned());
        }
        Ok(())
    }

    /// Answers one of the client's reads (blocking file IO).
    pub fn answer(&self, req: &FileContentsRequest) -> FileContentsResponse<'static> {
        let error = FileContentsResponse::new_error(req.stream_id);
        let Some(path) = usize::try_from(req.index)
            .ok()
            .and_then(|i| self.paths.get(i))
        else {
            return error;
        };
        let r = (|| -> Result<FileContentsResponse<'static>> {
            if req.flags.contains(FileContentsFlags::SIZE) {
                let len = std::fs::metadata(path)?.len();
                return Ok(FileContentsResponse::new_size_response(req.stream_id, len));
            }
            let mut f = std::fs::File::open(path)?;
            f.seek(SeekFrom::Start(req.position))?;
            let mut buf = Vec::with_capacity(req.requested_size.min(CHUNK * 8) as usize);
            f.take(u64::from(req.requested_size))
                .read_to_end(&mut buf)?;
            Ok(FileContentsResponse::new_data_response(req.stream_id, buf))
        })();
        r.unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = format!("{e:#}"), "Reading a copied file failed");
            error
        })
    }
}

/// The client's files on their way to a local folder.
pub struct Download {
    /// Top-level items (what the user copied).
    pub top: Vec<PathBuf>,
    /// Files still to fetch: (index in the client's list, size, local path).
    queue: std::collections::VecDeque<(i32, u64, PathBuf)>,
    current: Option<(i32, u64, std::fs::File)>,
    position: u64,
    clip_data_id: Option<u32>,
    stream_id: u32,
    pub bytes: u64,
}

impl Download {
    /// Mirrors the folders under `dir` and plans the file fetches.
    pub fn new(dir: &Path, files: &[FileDescriptor], clip_data_id: Option<u32>) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut top = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        for (index, d) in files.iter().enumerate() {
            let mut path = dir.to_owned();
            let rel = d.relative_path.as_deref().unwrap_or("");
            for part in rel
                .split(['\\', '/'])
                .chain(std::iter::once(d.name.as_str()))
            {
                if part.is_empty() {
                    continue;
                }
                path.push(safe_component(part)?);
            }
            if rel.is_empty() {
                top.push(path.clone());
            }
            let is_dir = d
                .attributes
                .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY));
            if is_dir {
                std::fs::create_dir_all(&path)?;
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                queue.push_back((index as i32, d.file_size.unwrap_or(0), path));
            }
        }
        Ok(Self {
            top,
            queue,
            current: None,
            position: 0,
            clip_data_id,
            stream_id: 0,
            bytes: 0,
        })
    }

    /// The next read to ask the client for; `None` when every file is complete.
    pub fn next_request(&mut self) -> Result<Option<FileContentsRequest>> {
        loop {
            if let Some((index, size, _)) = &self.current {
                if self.position < *size {
                    self.stream_id = self.stream_id.wrapping_add(1).max(1);
                    return Ok(Some(FileContentsRequest {
                        stream_id: self.stream_id,
                        index: *index,
                        flags: FileContentsFlags::RANGE,
                        position: self.position,
                        requested_size: (*size - self.position).min(u64::from(CHUNK)) as u32,
                        data_id: self.clip_data_id,
                    }));
                }
                self.current = None;
            }
            let Some((index, size, path)) = self.queue.pop_front() else {
                return Ok(None);
            };
            let file = std::fs::File::create(&path)
                .with_context(|| format!("creating {}", path.display()))?;
            self.current = Some((index, size, file));
            self.position = 0;
        }
    }

    /// The client's answer to the last request.
    pub fn on_response(&mut self, stream_id: u32, data: Option<&[u8]>) -> Result<()> {
        if stream_id != self.stream_id {
            bail!(
                "answer for stream {stream_id}, waiting for {}",
                self.stream_id
            );
        }
        let (Some(data), Some((_, size, file))) = (data, self.current.as_mut()) else {
            bail!("the client could not send the file");
        };
        if data.is_empty() || self.position + data.len() as u64 > *size {
            bail!(
                "the client sent {} bytes at {} of {size}",
                data.len(),
                self.position
            );
        }
        file.write_all(data)?;
        self.position += data.len() as u64;
        self.bytes += data.len() as u64;
        Ok(())
    }
}

/// One path component from the client, refused if it could leave the folder.
fn safe_component(part: &str) -> Result<&str> {
    let p = Path::new(part);
    let mut comps = p.components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) if !part.contains(':') => {}
        _ => bail!("refusing the file name {part:?}"),
    }
    if cfg!(windows) && ironrdp_cliprdr::is_windows_device_name(part) {
        bail!("refusing the device name {part:?}");
    }
    Ok(part)
}

/// Windows FILETIME: 100 ns ticks since 1601.
fn filetime(t: SystemTime) -> u64 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() + EPOCH_DIFF) * 10_000_000 + u64::from(d.subsec_nanos() / 100)
}

/// A fresh folder for the client's files; older ones are removed.
pub fn new_download_dir() -> Result<PathBuf> {
    let base = cache_dir().join("clipboard");
    if let Ok(entries) = std::fs::read_dir(&base) {
        for e in entries.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let dir = base.join(stamp.to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn cache_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    base.unwrap_or_else(std::env::temp_dir).join("glidedesk")
}

/// `file://` URI of an absolute path.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Local paths in a `text/uri-list` or `x-special/gnome-copied-files` text.
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("file://"))
        .map(|rest| {
            // file://host/path: only local files (no host or localhost).
            let path = rest.strip_prefix("localhost").unwrap_or(rest);
            PathBuf::from(percent_decode(path))
        })
        .filter(|p| p.is_absolute())
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gd-files-{tag}-{}",
            glidedesk_proto::random::<4>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Our files offered, then fetched back the way a client does, into another folder.
    #[test]
    fn files_go_round_trip() {
        let (src, dst) = (temp("src"), temp("dst"));
        std::fs::create_dir_all(src.join("folder/inner")).unwrap();
        std::fs::write(src.join("folder/a.txt"), b"hello").unwrap();
        std::fs::write(src.join("folder/inner/empty"), b"").unwrap();
        let big: Vec<u8> = (0..(CHUNK as usize * 2 + 123)).map(|i| i as u8).collect();
        std::fs::write(src.join("big bin"), &big).unwrap();

        let offer = Offer::new(&[src.join("folder"), src.join("big bin")]).unwrap();
        let names: Vec<_> = offer
            .descriptors
            .iter()
            .map(|d| format!("{}|{}", d.relative_path.as_deref().unwrap_or(""), d.name))
            .collect();
        assert_eq!(
            names,
            [
                "|folder",
                "folder|a.txt",
                "folder|inner",
                "folder\\inner|empty",
                "|big bin"
            ]
        );

        let mut down = Download::new(&dst, &offer.descriptors, Some(7)).unwrap();
        let mut requests = 0;
        while let Some(req) = down.next_request().unwrap() {
            assert_eq!(req.data_id, Some(7));
            let resp = offer.answer(&req);
            assert!(!resp.is_error());
            down.on_response(resp.stream_id(), Some(resp.data()))
                .unwrap();
            requests += 1;
        }
        assert_eq!(requests, 1 + 3);
        assert_eq!(down.top, vec![dst.join("folder"), dst.join("big bin")]);
        assert_eq!(std::fs::read(dst.join("folder/a.txt")).unwrap(), b"hello");
        assert!(dst.join("folder/inner/empty").is_file());
        assert_eq!(std::fs::read(dst.join("big bin")).unwrap(), big);
        std::fs::remove_dir_all(src).ok();
        std::fs::remove_dir_all(dst).ok();
    }

    #[test]
    fn names_cannot_leave_the_folder() {
        let dst = temp("evil");
        for (rel, name) in [
            (Some(".."), "x"),
            (None, "../x"),
            (Some("a\\..\\.."), "x"),
            (None, "C:x"),
        ] {
            let mut d = FileDescriptor::new(name).with_file_size(1);
            if let Some(r) = rel {
                d = d.with_relative_path(r);
            }
            assert!(Download::new(&dst, &[d], None).is_err(), "{rel:?} {name}");
        }
        std::fs::remove_dir_all(dst).ok();
    }

    #[cfg(unix)]
    #[test]
    fn uris() {
        let p = Path::new("/home/me/My file ñ.txt");
        let uri = file_uri(p);
        assert_eq!(uri, "file:///home/me/My%20file%20%C3%B1.txt");
        let list = format!("copy\n{uri}\nfile://localhost/tmp/a\nhttp://x/y");
        assert_eq!(
            parse_uri_list(&list),
            vec![p.to_path_buf(), PathBuf::from("/tmp/a")]
        );
    }
}
