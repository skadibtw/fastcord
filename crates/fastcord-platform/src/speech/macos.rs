//! macOS speech through the system `say` tool, which speaks with the voices
//! installed in System Settings, on the device. One `say` process per
//! utterance reads its text from standard input (so no text is taken for an
//! option); Stop kills it, which silences it at once.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use super::{Arc, Cmd, Mutex, Shared, SpeechError, Utterance, Voice, next_utterance};

const SAY: &str = "/usr/bin/say";
/// `say`'s normal rate, in words per minute.
const NORMAL_WPM: u32 = 180;
/// How often a speaking `say` is checked for its end.
const CHECK: Duration = Duration::from_millis(100);

fn begin(utterance: &Utterance) -> std::io::Result<Child> {
    let mut command = Command::new(SAY);
    if let Some(voice) = &utterance.voice {
        command.arg("-v").arg(voice);
    }
    let wpm = NORMAL_WPM * u32::from(utterance.rate) / 100;
    let mut child = command
        .arg("-r")
        .arg(wpm.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // At most 2 KiB: far below a pipe's buffer, so this never blocks.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(utterance.text.as_bytes())?;
    }
    Ok(child)
}

fn end(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// `say -v ?` lines: `Name   xx_YY    # sample sentence`; names may hold spaces.
fn parse_voices(listing: &str) -> Vec<Voice> {
    listing
        .lines()
        .filter_map(|line| {
            let described = line.split('#').next()?.trim_end();
            let (name, language) = described.rsplit_once(char::is_whitespace)?;
            let name = name.trim();
            (!name.is_empty()).then(|| Voice {
                id: name.to_owned(),
                name: name.to_owned(),
                language: language.replace('_', "-"),
            })
        })
        .collect()
}

pub fn voices() -> Result<Vec<Voice>, SpeechError> {
    let output = Command::new(SAY)
        .args(["-v", "?"])
        .stdin(Stdio::null())
        .output()
        .map_err(|_| SpeechError::Unsupported)?;
    if !output.status.success() {
        return Err(SpeechError::Failed);
    }
    Ok(parse_voices(&String::from_utf8_lossy(&output.stdout)))
}

pub fn run(
    shared: Arc<Mutex<Shared>>,
    _tx: mpsc::Sender<Cmd>,
    rx: mpsc::Receiver<Cmd>,
    ready: mpsc::Sender<Result<(), SpeechError>>,
) {
    if !std::path::Path::new(SAY).exists() {
        let _ = ready.send(Err(SpeechError::Unsupported));
        return;
    }
    let _ = ready.send(Ok(()));
    let mut speaking: Option<Child> = None;
    loop {
        // ponytail: a speaking `say` is checked every 100 ms for its end;
        // nothing wakes while idle. A waiter thread per process if that matters.
        let cmd = if speaking.is_some() {
            match rx.recv_timeout(CHECK) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            }
        };
        match cmd {
            Some(Cmd::Shutdown) => break,
            Some(Cmd::Stop) => {
                if let Some(child) = speaking.take() {
                    end(child);
                }
            }
            Some(Cmd::Wake | Cmd::Ended(_) | Cmd::Lost) | None => {}
        }
        if let Some(child) = &mut speaking {
            if matches!(child.try_wait(), Ok(None)) {
                continue;
            }
            speaking = None;
            shared.lock().busy = false;
        }
        while let Some(utterance) = next_utterance(&shared) {
            match begin(&utterance) {
                // A Stop that raced with the start cleared `busy`; honor it.
                Ok(child) if shared.lock().busy => {
                    speaking = Some(child);
                    break;
                }
                Ok(child) => {
                    end(child);
                    break;
                }
                Err(_) => shared.lock().busy = false,
            }
        }
    }
    if let Some(child) = speaking {
        end(child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_listing_keeps_names_with_spaces() {
        let voices = parse_voices(
            "Alex                en_US    # Most people recognize me by my voice.\n\
             Bad News            en_US    # The light you see at the end of the tunnel.\n\
             Amélie              fr_CA    # Bonjour, je m'appelle Amélie.\n\
             \n",
        );
        assert_eq!(voices.len(), 3);
        assert_eq!(voices[1].name, "Bad News");
        assert_eq!(voices[1].language, "en-US");
        assert_eq!(voices[2].id, "Amélie");
    }
}
