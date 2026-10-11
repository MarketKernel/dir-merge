//! Read-only text source backed either by a memory map (large files) or an owned
//! buffer (small files, UTF-16 files converted to UTF-8, in-memory text).
//!
//! Only a compact line index is kept in memory; the text itself stays in the
//! mapped file and the OS pages in just the parts that are actually read.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use encoding_rs::Encoding;
use memmap2::Mmap;
use parking_lot::RwLock;

/// Files at least this large are memory mapped instead of being read into RAM.
const MMAP_THRESHOLD: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8 { bom: bool },
    Utf16Le,
    Utf16Be,
    /// Any ASCII-compatible legacy encoding (Windows-1251, Shift_JIS, ...).
    /// Stored bytes are kept as-is and decoded line by line on demand.
    Legacy(&'static Encoding),
}

impl TextEncoding {
    pub fn label(&self) -> String {
        match self {
            TextEncoding::Utf8 { bom: false } => "UTF-8".into(),
            TextEncoding::Utf8 { bom: true } => "UTF-8 BOM".into(),
            TextEncoding::Utf16Le => "UTF-16 LE".into(),
            TextEncoding::Utf16Be => "UTF-16 BE".into(),
            TextEncoding::Legacy(enc) => enc.name().into(),
        }
    }

    /// Whether the stored bytes are UTF-8 (true for UTF-16 too, which is converted on load).
    fn stored_utf8(&self) -> bool {
        !matches!(self, TextEncoding::Legacy(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    CrLf,
    Mixed,
    /// No line breaks at all (single line or empty file).
    None,
}

impl Eol {
    pub fn label(&self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
            Eol::Mixed => "Mixed",
            Eol::None => "—",
        }
    }
}

enum Storage {
    Owned(Vec<u8>),
    Mapped(Mmap),
}

impl Storage {
    fn bytes(&self) -> &[u8] {
        match self {
            Storage::Owned(v) => v,
            Storage::Mapped(m) => m,
        }
    }
}

pub struct TextSource {
    pub path: Option<PathBuf>,
    storage: RwLock<Storage>,
    /// Byte offset of each line start, plus a final sentinel (= end of text).
    line_starts: Vec<u64>,
    pub encoding: TextEncoding,
    pub eol: Eol,
    /// The line ending to use for newly written lines.
    pub default_eol: &'static str,
    pub binary: bool,
    pub mapped: bool,
    pub max_line_len: usize,
    pub file_len: u64,
    pub mtime: Option<SystemTime>,
}

impl std::fmt::Debug for TextSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextSource")
            .field("path", &self.path)
            .field("lines", &self.line_count())
            .field("encoding", &self.encoding)
            .finish()
    }
}

impl TextSource {
    pub fn empty() -> Self {
        Self::from_bytes(None, Storage::Owned(Vec::new()), None)
    }

    #[cfg(test)]
    pub fn from_string(text: impl Into<String>) -> Self {
        Self::from_bytes(None, Storage::Owned(text.into().into_bytes()), None)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        if meta.is_dir() {
            return Err(io::Error::other("is a directory"));
        }
        let storage = if meta.len() >= MMAP_THRESHOLD {
            // SAFETY: the file is opened read-only; external truncation while
            // mapped is the usual mmap caveat that every viewer accepts.
            Storage::Mapped(unsafe { Mmap::map(&file)? })
        } else {
            let mut buf = Vec::with_capacity(meta.len() as usize);
            file.read_to_end(&mut buf)?;
            Storage::Owned(buf)
        };
        Ok(Self::from_bytes(
            Some(path.to_path_buf()),
            storage,
            meta.modified().ok(),
        ))
    }

    fn from_bytes(path: Option<PathBuf>, storage: Storage, mtime: Option<SystemTime>) -> Self {
        let file_len = storage.bytes().len() as u64;
        let mapped = matches!(storage, Storage::Mapped(_));
        let (encoding, storage, text_start, binary) = detect_encoding(storage);
        let bytes = storage.bytes();

        let mut line_starts = Vec::with_capacity(bytes.len() / 40 + 2);
        line_starts.push(text_start as u64);
        let mut crlf = 0usize;
        let mut lf = 0usize;
        let mut max_line_len = 0usize;
        let mut prev = text_start;
        for pos in memchr::memchr_iter(b'\n', &bytes[text_start..]) {
            let pos = pos + text_start;
            if pos > 0 && bytes[pos - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
            max_line_len = max_line_len.max(pos - prev);
            prev = pos + 1;
            line_starts.push(pos as u64 + 1);
        }
        if *line_starts.last().unwrap() != bytes.len() as u64 {
            max_line_len = max_line_len.max(bytes.len() - prev);
            line_starts.push(bytes.len() as u64);
        }
        line_starts.shrink_to_fit();

        let eol = match (lf, crlf) {
            (0, 0) => Eol::None,
            (_, 0) => Eol::Lf,
            (0, _) => Eol::CrLf,
            _ => Eol::Mixed,
        };
        let default_eol = if crlf > lf || (lf == 0 && crlf == 0 && cfg!(windows)) {
            "\r\n"
        } else {
            "\n"
        };

        TextSource {
            path,
            storage: RwLock::new(storage),
            line_starts,
            encoding,
            eol,
            default_eol,
            binary,
            mapped,
            max_line_len,
            file_len,
            mtime,
        }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len() - 1
    }

    /// Byte range of a line including its line terminator.
    pub fn line_range(&self, line: usize) -> Range<usize> {
        self.line_starts[line] as usize..self.line_starts[line + 1] as usize
    }

    /// Run `f` with access to the raw stored bytes. Keep the closure short-lived:
    /// it holds a read lock that blocks [`TextSource::detach`].
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        let guard = self.storage.read();
        f(guard.bytes())
    }

    /// Decode raw stored bytes (without line terminator) into text.
    pub fn decode<'a>(&self, raw: &'a [u8]) -> Cow<'a, str> {
        if self.encoding.stored_utf8() {
            String::from_utf8_lossy(raw)
        } else if let TextEncoding::Legacy(enc) = self.encoding {
            enc.decode_without_bom_handling(raw).0
        } else {
            unreachable!()
        }
    }

    /// Text of a line without its terminator.
    pub fn line_text(&self, line: usize) -> String {
        let range = self.line_range(line);
        self.with_bytes(|b| self.decode(strip_eol(&b[range])).into_owned())
    }

    /// Visit lines `range` with their text (terminator stripped) and raw terminator.
    pub fn for_each_line(&self, range: Range<usize>, mut f: impl FnMut(usize, &str, &[u8])) {
        self.with_bytes(|b| {
            for line in range {
                let raw = &b[self.line_range(line)];
                let content = strip_eol(raw);
                let text = self.decode(content);
                f(line, &text, &raw[content.len()..]);
            }
        })
    }

    /// Raw stored bytes of a range of lines (including terminators).
    pub fn raw_lines_range(&self, range: Range<usize>) -> Range<usize> {
        if range.is_empty() {
            return 0..0;
        }
        self.line_starts[range.start] as usize..self.line_starts[range.end] as usize
    }

    pub fn can_copy_raw(&self) -> bool {
        // UTF-16 sources are converted to UTF-8 internally, so raw bytes
        // don't match the on-disk encoding.
        !matches!(self.encoding, TextEncoding::Utf16Le | TextEncoding::Utf16Be)
    }

    /// Replace a memory map by an owned copy of the bytes, releasing the file.
    /// Needed on Windows before the mapped file can be replaced on save.
    pub fn detach(&self) {
        let mut guard = self.storage.write();
        if let Storage::Mapped(m) = &*guard {
            let owned = m.to_vec();
            *guard = Storage::Owned(owned);
        }
    }
}

pub fn strip_eol(raw: &[u8]) -> &[u8] {
    let mut end = raw.len();
    if end > 0 && raw[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && raw[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &raw[..end]
}

/// Returns (encoding, storage with text, byte offset of text start, looks binary).
fn detect_encoding(storage: Storage) -> (TextEncoding, Storage, usize, bool) {
    let bytes = storage.bytes();
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return (TextEncoding::Utf8 { bom: true }, storage, 3, false);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let (enc, te) = if bytes[0] == 0xFF {
            (encoding_rs::UTF_16LE, TextEncoding::Utf16Le)
        } else {
            (encoding_rs::UTF_16BE, TextEncoding::Utf16Be)
        };
        let text = enc.decode_with_bom_removal(bytes).0.into_owned();
        return (te, Storage::Owned(text.into_bytes()), 0, false);
    }
    let head = &bytes[..bytes.len().min(64 * 1024)];
    let binary = memchr::memchr(0, head).is_some();
    if binary || std::str::from_utf8(bytes).is_ok() {
        return (TextEncoding::Utf8 { bom: false }, storage, 0, binary);
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(head, head.len() == bytes.len());
    let enc = det.guess(None, chardetng::Utf8Detection::Allow);
    if enc == encoding_rs::UTF_8 {
        (TextEncoding::Utf8 { bom: false }, storage, 0, false)
    } else {
        (TextEncoding::Legacy(enc), storage, 0, false)
    }
}

/// Encode text into the given encoding.
/// Encode text into the given encoding. Fails with the first character the
/// encoding cannot represent (instead of silently writing HTML entities).
pub fn encode_into(enc: TextEncoding, text: &str, out: &mut Vec<u8>) -> Result<(), char> {
    match enc {
        TextEncoding::Utf8 { .. } => out.extend_from_slice(text.as_bytes()),
        TextEncoding::Utf16Le => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_le_bytes())),
        TextEncoding::Utf16Be => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_be_bytes())),
        TextEncoding::Legacy(e) => {
            let mut encoder = e.new_encoder();
            let cap = encoder.max_buffer_length_from_utf8_without_replacement(text.len()).unwrap_or(text.len() * 4 + 16);
            let mut buf = vec![0u8; cap];
            let (res, _, written) = encoder.encode_from_utf8_without_replacement(text, &mut buf, true);
            match res {
                encoding_rs::EncoderResult::Unmappable(c) => return Err(c),
                _ => out.extend_from_slice(&buf[..written]),
            }
        }
    }
    Ok(())
}

pub fn bom_bytes(enc: TextEncoding) -> &'static [u8] {
    match enc {
        TextEncoding::Utf8 { bom: true } => &[0xEF, 0xBB, 0xBF],
        TextEncoding::Utf16Le => &[0xFF, 0xFE],
        TextEncoding::Utf16Be => &[0xFE, 0xFF],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index() {
        let s = TextSource::from_string("a\r\nbb\nccc");
        assert_eq!(s.line_count(), 3);
        assert_eq!(s.line_text(0), "a");
        assert_eq!(s.line_text(1), "bb");
        assert_eq!(s.line_text(2), "ccc");
        assert_eq!(s.eol, Eol::Mixed);
        assert_eq!(TextSource::from_string("").line_count(), 0);
        assert_eq!(TextSource::from_string("x\n").line_count(), 1);
        assert_eq!(TextSource::from_string("\n").line_count(), 1);
    }

    #[test]
    fn legacy_encoding() {
        let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode("Привет, мир! Как дела? Всё хорошо.\nВторая строка текста\n");
        let s = TextSource::from_bytes(None, Storage::Owned(bytes.into_owned()), None);
        assert!(matches!(s.encoding, TextEncoding::Legacy(_)));
        assert_eq!(s.line_text(1), "Вторая строка текста");
    }

    #[test]
    fn utf16() {
        let mut bytes = vec![0xFF, 0xFE];
        "hi\nthere".encode_utf16().for_each(|u| bytes.extend_from_slice(&u.to_le_bytes()));
        let s = TextSource::from_bytes(None, Storage::Owned(bytes), None);
        assert_eq!(s.encoding, TextEncoding::Utf16Le);
        assert_eq!(s.line_text(1), "there");
    }
}
