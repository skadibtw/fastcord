//! A short, secret-free log of voice events (connection status, inbound
//! packets that failed decryption, playback underruns, device errors), so a
//! dropout can be explained afterwards. The last [`MAX_LINES`] lines stay in
//! memory for the audio dialog, and every line is appended to
//! `fastcord/logs/voice.log` beside the audio settings, rotated to
//! `voice.log.1` past [`MAX_FILE_BYTES`]. Lines carry event kinds and counts
//! only: never tokens, keys, endpoints or message content.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LINES: usize = 200;
const MAX_FILE_BYTES: u64 = 1 << 20;

static LINES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Records one event, stamped with local time.
pub fn note(line: &str) {
    let stamped = format!("{} {line}", stamp());
    if let Ok(mut lines) = LINES.lock() {
        if lines.len() == MAX_LINES {
            lines.pop_front();
        }
        lines.push_back(stamped.clone());
    }
    // ponytail: a synchronous append on the voice task; lines are rare
    // (state changes and once-a-second anomalies), move to a writer thread
    // if this ever logs per packet.
    if let Some(path) = file() {
        let _ = append(&path, &stamped);
    }
}

/// The newest `count` lines, oldest first.
pub fn recent(count: usize) -> Vec<String> {
    LINES.lock().map_or_else(
        |_| Vec::new(),
        |lines| lines.iter().rev().take(count).rev().cloned().collect(),
    )
}

/// Where the file log lives, for the audio dialog to name.
pub fn file() -> Option<PathBuf> {
    crate::audio_devices::data_dir().map(|dir| dir.join("logs").join("voice.log"))
}

fn stamp() -> String {
    let now = time::OffsetDateTime::now_utc()
        .to_offset(crate::localtime::offset().unwrap_or(time::UtcOffset::UTC));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_FILE_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_rotates_past_its_bound_and_keeps_appending() {
        let dir = std::env::temp_dir().join(format!("fastcord-voice-log-{}", std::process::id()));
        let path = dir.join("logs").join("voice.log");
        let _ = fs::remove_dir_all(&dir);
        append(&path, "first").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\n");
        fs::write(&path, vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        append(&path, "after rotation").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "after rotation\n");
        assert!(path.with_extension("log.1").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn memory_keeps_only_the_newest_lines() {
        for n in 0..MAX_LINES + 5 {
            if let Ok(mut lines) = LINES.lock() {
                if lines.len() == MAX_LINES {
                    lines.pop_front();
                }
                lines.push_back(format!("line {n}"));
            }
        }
        let last = recent(2);
        assert_eq!(
            last,
            [
                format!("line {}", MAX_LINES + 3),
                format!("line {}", MAX_LINES + 4)
            ]
        );
        assert!(LINES.lock().unwrap().len() <= MAX_LINES);
    }
}
