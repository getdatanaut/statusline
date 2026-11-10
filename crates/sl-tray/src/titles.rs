//! A session's title, as Claude Code shows it in its own session list.
//!
//! Claude Code names a session from what it is about and appends the name to
//! the transcript as `{"type":"ai-title","aiTitle":"..."}`, again as the
//! session goes on; the newest one is the title. Those lines can be megabytes
//! apart in a long transcript, so the newest is found once by reading back
//! from the end until one turns up, and after that only what was appended is
//! read.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// How much is read at a time when looking back for the newest title.
const CHUNK: u64 = 1024 * 1024;
const MARKER: &str = "\"type\":\"ai-title\"";

/// Remembers each transcript's title and how far it has been read, so a poll
/// costs a `stat` unless the transcript grew, and then only the growth.
#[derive(Default)]
pub struct TitleReader {
    /// By path: bytes read (always up to a line end), and the title so far.
    known: HashMap<PathBuf, (u64, Option<String>)>,
}

impl TitleReader {
    pub fn read(&mut self, path: &Path) -> Option<String> {
        let len = std::fs::metadata(path).ok()?.len();
        let (read, title) = match self.known.get(path) {
            Some((read, title)) if *read == len => return title.clone(),
            // Grown: only the new lines can hold a newer title.
            Some((read, title)) if *read < len => {
                let (newer, read) = scan_forward(path, *read, len)?;
                (read, newer.or_else(|| title.clone()))
            }
            // New to us, or rewritten shorter: look back from the end.
            _ => (complete_len(path, len)?, scan_backward(path, len)?),
        };
        self.known.insert(path.to_owned(), (read, title.clone()));
        title
    }

    /// Forget transcripts no longer looked at.
    pub fn retain(&mut self, paths: &[&Path]) {
        self.known.retain(|path, _| paths.contains(&path.as_path()));
    }
}

/// The newest title in `[from, to)`, and where the last complete line ends: a
/// line still being written is left for the next read.
fn scan_forward(path: &Path, from: u64, to: u64) -> Option<(Option<String>, u64)> {
    let bytes = read_range(path, from, to)?;
    let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    Some((newest_title(&bytes[..complete]), from + complete as u64))
}

/// Where the last complete line of the first `len` bytes ends.
fn complete_len(path: &Path, len: u64) -> Option<u64> {
    let tail = read_range(path, len.saturating_sub(CHUNK), len)?;
    let start = len.saturating_sub(CHUNK);
    Some(
        tail.iter()
            .rposition(|b| *b == b'\n')
            .map_or(start, |i| start + i as u64 + 1),
    )
}

/// The newest title in the first `len` bytes, reading back a chunk at a time.
#[expect(
    clippy::option_option,
    reason = "`None` is a read that failed, `Some(None)` a file with no title in it"
)]
fn scan_backward(path: &Path, len: u64) -> Option<Option<String>> {
    let mut end = len;
    // The start of a line cut by the previous chunk's start, carried back.
    let mut carry: Vec<u8> = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let mut bytes = read_range(path, start, end)?;
        bytes.extend_from_slice(&carry);
        // Unless this is the start of the file, the first line may be cut.
        let first = if start > 0 {
            bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |i| i + 1)
        } else {
            0
        };
        if let Some(title) = newest_title(&bytes[first..]) {
            return Some(Some(title));
        }
        carry = bytes[..first].to_vec();
        end = start;
    }
    Some(None)
}

fn read_range(path: &Path, from: u64, to: u64) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes =
        Vec::with_capacity(usize::try_from(to.saturating_sub(from)).unwrap_or(usize::MAX));
    file.take(to.saturating_sub(from))
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

/// The last title among complete `lines`.
pub fn newest_title(lines: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(lines);
    text.lines()
        .rev()
        .filter(|line| line.contains(MARKER))
        .find_map(|line| {
            let record: Value = serde_json::from_str(line).ok()?;
            let title = record.get("aiTitle")?.as_str()?.trim();
            (!title.is_empty()).then(|| title.to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn title(text: &str) -> String {
        format!("{{\"type\":\"ai-title\",\"aiTitle\":\"{text}\",\"sessionId\":\"s\"}}\n")
    }

    fn filler(bytes: usize) -> String {
        let line = format!("{{\"type\":\"user\",\"text\":\"{}\"}}\n", "x".repeat(200));
        line.repeat(bytes / line.len() + 1)
    }

    fn transcript(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("t.jsonl");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// A scratch directory, removed when the guard is dropped.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let dir = guard.path().to_path_buf();
        (guard, dir)
    }

    #[test]
    fn the_newest_title_wins() {
        let lines = format!("{}{}{}", title("First"), filler(500), title("Second"));
        assert_eq!(newest_title(lines.as_bytes()).as_deref(), Some("Second"));
        assert_eq!(newest_title(filler(500).as_bytes()), None);
    }

    #[test]
    fn a_title_megabytes_from_the_end_is_found_across_chunks() {
        let (_guard, dir) = scratch();
        let path = transcript(
            &dir,
            &format!(
                "{}{}{}",
                title("Old"),
                title("Far back"),
                filler(3 * usize::try_from(CHUNK).unwrap_or(usize::MAX))
            ),
        );
        assert_eq!(
            TitleReader::default().read(&path).as_deref(),
            Some("Far back")
        );
        let path = transcript(
            &dir,
            &filler(2 * usize::try_from(CHUNK).unwrap_or(usize::MAX)),
        );
        assert_eq!(TitleReader::default().read(&path), None);
    }

    #[test]
    fn after_the_first_read_only_appended_lines_are_read() {
        let (_guard, dir) = scratch();
        let path = transcript(&dir, &format!("{}{}", title("Before"), filler(1000)));
        let mut reader = TitleReader::default();
        assert_eq!(reader.read(&path).as_deref(), Some("Before"));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(filler(1000).as_bytes()).unwrap();
        assert_eq!(
            reader.read(&path).as_deref(),
            Some("Before"),
            "no newer title: the old one stands"
        );

        // A title line still being written is not taken until it is whole.
        let line = title("After");
        file.write_all(&line.as_bytes()[..10]).unwrap();
        assert_eq!(reader.read(&path).as_deref(), Some("Before"));
        file.write_all(&line.as_bytes()[10..]).unwrap();
        assert_eq!(reader.read(&path).as_deref(), Some("After"));
    }
}
