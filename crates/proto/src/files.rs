//! Files on the clipboard, over the native protocol (version 4), the same in both
//! directions.
//!
//! - The side that copied files sends their list ([`crate::FileEntry`]: folders walked,
//!   paths relative to the copied items, `/`-separated) as `Files` with a fresh id.
//! - The other side reads them, when it needs them, with `FileRead` (one range of one
//!   file) and gets `FileData` back; [`Download`] drives that and writes to disk.
//! - Ranges are small ([`CHUNK`]) so the control channel keeps carrying input between them.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use crate::FileEntry;

/// Bytes per read: small enough that input queued behind one is not noticeably late.
pub const CHUNK: u32 = 256 << 10;
/// Entries offered at most (folders walked).
pub const MAX_ENTRIES: usize = 50_000;

/// Our copied files and folders, flattened.
#[derive(Debug, Default)]
pub struct Offer {
    pub entries: Vec<FileEntry>,
    /// The local path behind each entry.
    paths: Vec<PathBuf>,
}

impl Offer {
    pub fn new(items: &[PathBuf]) -> io::Result<Self> {
        let mut offer = Self::default();
        for item in items {
            let name = item
                .file_name()
                .ok_or_else(|| io::Error::other(format!("{} has no name", item.display())))?
                .to_string_lossy()
                .into_owned();
            offer.add(item, name)?;
        }
        Ok(offer)
    }

    fn add(&mut self, path: &Path, rel: String) -> io::Result<()> {
        if self.entries.len() >= MAX_ENTRIES {
            return Err(io::Error::other(format!("more than {MAX_ENTRIES} files")));
        }
        let meta = std::fs::metadata(path)?;
        self.entries.push(FileEntry {
            path: rel.clone(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            dir: meta.is_dir(),
        });
        self.paths.push(path.to_owned());
        if meta.is_dir() {
            let mut children: Vec<_> = std::fs::read_dir(path)?.filter_map(Result::ok).collect();
            children.sort_by_key(std::fs::DirEntry::file_name);
            for child in children {
                let name = child.file_name().to_string_lossy().into_owned();
                self.add(&child.path(), format!("{rel}/{name}"))?;
            }
        }
        Ok(())
    }

    /// Total bytes of the files.
    pub fn bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// Up to `len` bytes (at most [`CHUNK`]) of entry `index` from `offset`.
    pub fn read(&self, index: u32, offset: u64, len: u32) -> io::Result<Vec<u8>> {
        let (Some(entry), Some(path)) = (
            self.entries.get(index as usize),
            self.paths.get(index as usize),
        ) else {
            return Err(io::Error::other("no such file"));
        };
        if entry.dir {
            return Err(io::Error::other("a folder has no contents"));
        }
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len.min(CHUNK) as usize];
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        buf.truncate(n);
        Ok(buf)
    }
}

/// `rel` as a path under `dir`, refusing anything that would leave it (absolute paths,
/// `..`, drive prefixes) or is empty.
pub fn safe_join(dir: &Path, rel: &str) -> io::Result<PathBuf> {
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err(io::Error::other(format!("unsafe path {rel:?}")));
    }
    let mut out = dir.to_owned();
    let mut parts = 0;
    for part in rel.split('/') {
        let p = Path::new(part);
        match p.components().next() {
            None => continue,
            Some(Component::Normal(_)) if p.components().count() == 1 => {
                out.push(part);
                parts += 1;
            }
            _ => return Err(io::Error::other(format!("unsafe path {rel:?}"))),
        }
        // Windows would read these as a stream or a device.
        if part.contains(':') || part.contains('\\') {
            return Err(io::Error::other(format!("unsafe path {rel:?}")));
        }
    }
    if parts == 0 {
        return Err(io::Error::other("empty path"));
    }
    Ok(out)
}

/// Fetches the other side's files into a folder, range by range.
#[derive(Debug)]
pub struct Download {
    pub id: u32,
    dir: PathBuf,
    entries: Vec<FileEntry>,
    /// The file being fetched, and how much of it has arrived.
    current: usize,
    offset: u64,
    file: Option<File>,
    /// A read is out and not answered yet.
    waiting: bool,
    pub bytes: u64,
}

impl Download {
    /// Checks every path and creates the folders up front.
    pub fn new(dir: &Path, id: u32, entries: Vec<FileEntry>) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        for e in &entries {
            let path = safe_join(dir, &e.path)?;
            if e.dir {
                std::fs::create_dir_all(&path)?;
            }
        }
        Ok(Self {
            id,
            dir: dir.to_owned(),
            entries,
            current: 0,
            offset: 0,
            file: None,
            waiting: false,
            bytes: 0,
        })
    }

    /// The next read to ask for (`index`, `offset`, `len`); `None` when everything is here
    /// or a read is already out.
    pub fn next_request(&mut self) -> io::Result<Option<(u32, u64, u32)>> {
        if self.waiting {
            return Ok(None);
        }
        while let Some(e) = self.entries.get(self.current) {
            if e.dir {
                self.current += 1;
                continue;
            }
            if self.file.is_none() {
                let path = safe_join(&self.dir, &e.path)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                self.file = Some(File::create(&path)?);
                self.offset = 0;
            }
            if self.offset >= e.size {
                self.file = None;
                self.current += 1;
                continue;
            }
            let len = (e.size - self.offset).min(u64::from(CHUNK)) as u32;
            self.waiting = true;
            return Ok(Some((self.current as u32, self.offset, len)));
        }
        Ok(None)
    }

    /// All files have arrived.
    pub fn done(&self) -> bool {
        !self.waiting && self.current >= self.entries.len()
    }

    /// The answer to the read that is out.
    pub fn on_data(&mut self, index: u32, offset: u64, data: &[u8]) -> io::Result<()> {
        if !self.waiting || index as usize != self.current || offset != self.offset {
            return Err(io::Error::other("an answer to a read that was not asked for"));
        }
        let size = self.entries[self.current].size;
        if data.is_empty() && self.offset < size {
            return Err(io::Error::other("the file got shorter while it was read"));
        }
        let take = data.len().min((size - self.offset) as usize);
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("no file open"))?
            .write_all(&data[..take])?;
        self.offset += take as u64;
        self.bytes += take as u64;
        self.waiting = false;
        Ok(())
    }

    /// The copied items themselves (the top level of the list), in order.
    pub fn top_paths(&self) -> Vec<PathBuf> {
        let mut seen: Vec<&str> = Vec::new();
        for e in &self.entries {
            let top = e.path.split('/').find(|p| !p.is_empty()).unwrap_or("");
            if !top.is_empty() && !seen.contains(&top) {
                seen.push(top);
            }
        }
        seen.into_iter().map(|t| self.dir.join(t)).collect()
    }
}

/// A fresh folder under `base` for one paste: `glidedesk-<seconds>-<n>`.
pub fn new_folder(base: &Path) -> io::Result<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for n in 0..1000 {
        let dir = base.join(format!("glidedesk-{secs}-{n}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                std::fs::create_dir_all(base)?;
                if n > 0 {
                    return Err(e);
                }
            }
        }
    }
    Err(io::Error::other("no free folder name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "glidedesk-files-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let d = Path::new("/base");
        assert_eq!(safe_join(d, "a/b.txt").unwrap(), Path::new("/base/a/b.txt"));
        for bad in ["../x", "a/../../x", "/etc/passwd", "", "a/c:x", "a\\..\\x"] {
            assert!(safe_join(d, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_folder_round_trips() {
        let src = temp("src");
        std::fs::create_dir_all(src.join("folder/sub")).unwrap();
        let big: Vec<u8> = (0..(CHUNK as usize * 2 + 17)).map(|i| i as u8).collect();
        std::fs::write(src.join("folder/sub/big.bin"), &big).unwrap();
        std::fs::write(src.join("folder/empty"), b"").unwrap();
        std::fs::write(src.join("one.txt"), b"hello").unwrap();
        let offer = Offer::new(&[src.join("folder"), src.join("one.txt")]).unwrap();
        assert_eq!(offer.bytes(), big.len() as u64 + 5);
        assert!(offer.entries.iter().any(|e| e.path == "folder/sub/big.bin"));

        let dst = temp("dst");
        let mut d = Download::new(&dst, 7, offer.entries.clone()).unwrap();
        let mut reads = 0;
        while let Some((index, offset, len)) = d.next_request().unwrap() {
            // Only one read at a time.
            assert!(d.next_request().unwrap().is_none());
            let data = offer.read(index, offset, len).unwrap();
            d.on_data(index, offset, &data).unwrap();
            reads += 1;
        }
        assert!(d.done());
        assert_eq!(reads, 4); // three ranges of the big file, one of one.txt
        assert_eq!(std::fs::read(dst.join("folder/sub/big.bin")).unwrap(), big);
        assert_eq!(std::fs::read(dst.join("folder/empty")).unwrap(), b"");
        assert_eq!(std::fs::read(dst.join("one.txt")).unwrap(), b"hello");
        assert_eq!(d.top_paths(), vec![dst.join("folder"), dst.join("one.txt")]);
        // An answer nobody asked for is refused.
        assert!(d.on_data(0, 0, b"x").is_err());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
