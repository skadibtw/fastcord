//! Linux speech through speech-dispatcher, the desktop's speech service (the
//! one screen readers use), spoken to in its SSIP text protocol over the
//! user's Unix socket. Like libspeechd, a missing daemon is started once with
//! `speech-dispatcher --spawn`; without it speech is
//! [`SpeechError::Unsupported`]. Stop cancels only this client's messages
//! (`CANCEL self`), never a screen reader's.
//!
//! One reader thread per connection turns the daemon's replies and its
//! end/cancel events into messages for the speech thread; nothing polls.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use super::{
    Arc, Cmd, MAX_RATE, MIN_RATE, Mutex, Shared, SpeechError, Utterance, Voice, next_utterance,
};

/// How long the daemon may take to answer one command.
const REPLY: Duration = Duration::from_secs(5);
/// How long a spawned daemon may take to open its socket.
const SPAWN_WAIT: Duration = Duration::from_secs(3);
/// Lines of one reply kept (a voice list is the longest).
const MAX_REPLY_LINES: usize = 2048;

/// The daemon's socket: `SPEECHD_ADDRESS` (`unix_socket:<path>`) or the
/// per-user default.
fn socket_path() -> Option<PathBuf> {
    if let Some(address) = std::env::var_os("SPEECHD_ADDRESS") {
        return address
            .to_str()?
            .strip_prefix("unix_socket:")
            .map(PathBuf::from);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("speech-dispatcher/speechd.sock"))
}

fn connect() -> Result<UnixStream, SpeechError> {
    let path = socket_path().ok_or(SpeechError::Unsupported)?;
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    let spawned = Command::new("speech-dispatcher")
        .arg("--spawn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if spawned.is_err() {
        // Not installed.
        return Err(SpeechError::Unsupported);
    }
    let step = Duration::from_millis(100);
    for _ in 0..SPAWN_WAIT.as_millis() / step.as_millis() {
        std::thread::sleep(step);
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
    }
    Err(SpeechError::Failed)
}

/// One complete answer: its code and the text of its lines.
#[derive(Debug)]
struct Reply {
    code: u16,
    lines: Vec<String>,
}

/// Reads replies and events until the connection closes. Events 702 (end)
/// and 703 (cancel) report the finished message's ID; replies go to the
/// command waiting for them.
fn read(stream: UnixStream, replies: mpsc::Sender<Reply>, events: mpsc::Sender<Cmd>) {
    let mut lines = Vec::new();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let line = line.trim_end_matches('\r');
        let (Some(code), Some(separator)) = (
            line.get(..3).and_then(|code| code.parse::<u16>().ok()),
            line.as_bytes().get(3),
        ) else {
            break;
        };
        if lines.len() < MAX_REPLY_LINES {
            lines.push(line.get(4..).unwrap_or_default().to_owned());
        }
        if *separator == b'-' {
            continue;
        }
        let reply = Reply {
            code,
            lines: std::mem::take(&mut lines),
        };
        if (700..800).contains(&code) {
            if matches!(code, 702 | 703)
                && let Some(id) = reply.lines.first().and_then(|id| id.parse().ok())
            {
                let _ = events.send(Cmd::Ended(id));
            }
        } else if replies.send(reply).is_err() {
            break;
        }
    }
    let _ = events.send(Cmd::Lost);
}

/// Text as SSIP data: CRLF lines, a leading dot doubled, ended by a lone dot.
fn data(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for line in text.lines() {
        let line: String = line.chars().filter(|c| !c.is_control()).collect();
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

/// A percent of the normal rate as SSIP's -100..=100 (100% is 0).
fn ssip_rate(percent: u16) -> i32 {
    let percent = i32::from(percent.clamp(MIN_RATE, MAX_RATE));
    if percent <= 100 {
        (percent - 100) * 100 / (100 - i32::from(MIN_RATE))
    } else {
        (percent - 100) * 100 / (i32::from(MAX_RATE) - 100)
    }
}

/// One connection to the daemon.
struct Session {
    writer: UnixStream,
    replies: mpsc::Receiver<Reply>,
}

impl Session {
    fn open(events: mpsc::Sender<Cmd>) -> Result<Self, SpeechError> {
        Self::with(connect()?, events)
    }

    #[cfg(test)]
    fn open_at(path: &std::path::Path, events: mpsc::Sender<Cmd>) -> Result<Self, SpeechError> {
        Self::with(
            UnixStream::connect(path).map_err(|_| SpeechError::Failed)?,
            events,
        )
    }

    fn with(stream: UnixStream, events: mpsc::Sender<Cmd>) -> Result<Self, SpeechError> {
        let reader = stream.try_clone().map_err(|_| SpeechError::Failed)?;
        let (replies_tx, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("fastcord-speechd".to_owned())
            .spawn(move || read(reader, replies_tx, events))
            .map_err(|_| SpeechError::Failed)?;
        let mut session = Self {
            writer: stream,
            replies,
        };
        session.command("SET self CLIENT_NAME user:fastcord:main")?;
        session.command("SET self NOTIFICATION end on")?;
        session.command("SET self NOTIFICATION cancel on")?;
        Ok(session)
    }

    fn send(&mut self, bytes: &str) -> Result<Reply, SpeechError> {
        self.writer
            .write_all(bytes.as_bytes())
            .map_err(|_| SpeechError::Failed)?;
        let reply = self
            .replies
            .recv_timeout(REPLY)
            .map_err(|_| SpeechError::Failed)?;
        // 2xx accepted (230 waits for data); 3xx and 4xx are refusals.
        if (200..300).contains(&reply.code) {
            Ok(reply)
        } else {
            Err(SpeechError::Failed)
        }
    }

    fn command(&mut self, line: &str) -> Result<Reply, SpeechError> {
        self.send(&format!("{line}\r\n"))
    }

    /// Queues `utterance` and returns the daemon's message ID.
    fn speak(&mut self, utterance: &Utterance) -> Result<u64, SpeechError> {
        match utterance.voice.as_deref() {
            // A listed name cannot contain a line break; refuse one that does.
            Some(voice) if !voice.chars().any(char::is_control) => {
                let _ = self.command(&format!("SET self SYNTHESIS_VOICE {voice}"));
            }
            // The default voice of the default synthesizer.
            _ => {
                let _ = self.command("SET self VOICE_TYPE MALE1");
            }
        }
        self.command(&format!("SET self RATE {}", ssip_rate(utterance.rate)))?;
        self.command("SPEAK")?;
        let queued = self.send(&data(&utterance.text))?;
        queued
            .lines
            .first()
            .and_then(|id| id.parse().ok())
            .ok_or(SpeechError::Failed)
    }

    fn cancel(&mut self) {
        let _ = self.command("CANCEL self");
    }

    fn voices(&mut self) -> Result<Vec<Voice>, SpeechError> {
        let reply = self.command("LIST SYNTHESIS_VOICES")?;
        Ok(reply
            .lines
            .iter()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let name = fields.next()?.trim();
                let language = fields.next().unwrap_or_default().trim();
                (!name.is_empty()).then(|| Voice {
                    id: name.to_owned(),
                    name: name.to_owned(),
                    language: language.to_owned(),
                })
            })
            .collect())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Ends the reader thread too.
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}

pub fn voices() -> Result<Vec<Voice>, SpeechError> {
    let (events, _ignored) = mpsc::channel();
    Session::open(events)?.voices()
}

pub fn run(
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::Sender<Cmd>,
    rx: mpsc::Receiver<Cmd>,
    ready: mpsc::Sender<Result<(), SpeechError>>,
) {
    let mut session = match Session::open(tx.clone()) {
        Ok(session) => Some(session),
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    serve(&shared, &rx, &mut session, || Session::open(tx.clone()));
}

/// The speech thread's loop: one message at a time, the next when the daemon
/// reports the current one ended or was cancelled.
fn serve(
    shared: &Mutex<Shared>,
    rx: &mpsc::Receiver<Cmd>,
    session: &mut Option<Session>,
    reopen: impl Fn() -> Result<Session, SpeechError>,
) {
    let mut current: Option<u64> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Shutdown => break,
            Cmd::Stop => {
                if current.take().is_some()
                    && let Some(session) = session
                {
                    session.cancel();
                }
            }
            Cmd::Ended(id) if current == Some(id) => {
                current = None;
                shared.lock().busy = false;
            }
            Cmd::Ended(_) => continue,
            Cmd::Lost => {
                // Reconnect when there is something to say.
                *session = None;
                current = None;
                shared.lock().busy = false;
            }
            Cmd::Wake => {}
        }
        if current.is_some() {
            continue;
        }
        while let Some(utterance) = next_utterance(shared) {
            if session.is_none() {
                *session = reopen().ok();
            }
            let Some(open) = session.as_mut() else {
                shared.lock().busy = false;
                continue;
            };
            match open.speak(&utterance) {
                // A Stop that raced with the request cleared `busy`; honor it.
                Ok(id) if shared.lock().busy => {
                    current = Some(id);
                    break;
                }
                Ok(_) => {
                    open.cancel();
                    break;
                }
                Err(_) => shared.lock().busy = false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn data_escapes_dots_and_ends_with_a_lone_dot() {
        assert_eq!(data("hi"), "hi\r\n.\r\n");
        assert_eq!(data(".\nnext\r\n.x"), "..\r\nnext\r\n..x\r\n.\r\n");
        assert_eq!(data("a\u{7}b"), "ab\r\n.\r\n");
    }

    #[test]
    fn rates_map_onto_ssip() {
        assert_eq!(ssip_rate(100), 0);
        assert_eq!(ssip_rate(MIN_RATE), -100);
        assert_eq!(ssip_rate(MAX_RATE), 100);
        assert_eq!(ssip_rate(200), 50);
        assert_eq!(ssip_rate(10), -100);
    }

    /// A stand-in speech-dispatcher: answers the setup, reads one message
    /// and reports it spoken, lists two voices, and records what it got.
    fn daemon(listener: UnixListener, got: mpsc::Sender<String>) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let got = got.clone();
            std::thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let command = line.trim_end().to_owned();
                    line.clear();
                    let answer = if command == "SPEAK" {
                        writer.write_all(b"230 OK RECEIVING DATA\r\n").unwrap();
                        let mut text = String::new();
                        loop {
                            reader.read_line(&mut line).unwrap();
                            if line == ".\r\n" {
                                line.clear();
                                break;
                            }
                            text.push_str(&line);
                            line.clear();
                        }
                        got.send(text).unwrap();
                        "225-21\r\n225 OK MESSAGE QUEUED\r\n702-21\r\n702-1\r\n702 END\r\n"
                            .to_owned()
                    } else if command == "LIST SYNTHESIS_VOICES" {
                        "249-Anna\tde\tnone\r\n249-Bob\ten-US\tnone\r\n249 OK VOICE LIST SENT\r\n"
                            .to_owned()
                    } else {
                        got.send(command).unwrap();
                        "200 OK\r\n".to_owned()
                    };
                    writer.write_all(answer.as_bytes()).unwrap();
                }
            });
        }
    }

    #[test]
    fn a_message_is_spoken_and_its_end_starts_the_next() {
        let dir = std::env::temp_dir().join(format!("fastcord-ssip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("speechd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (got_tx, got) = mpsc::channel();
        std::thread::spawn(move || daemon(listener, got_tx));

        let (tx, rx) = mpsc::channel();
        let mut session = Session::open_at(&path, tx.clone()).ok();
        assert!(session.is_some());
        let voices = session.as_mut().unwrap().voices().unwrap();
        assert_eq!(voices[1].name, "Bob");
        assert_eq!(voices[1].language, "en-US");

        let shared = Arc::new(Mutex::new(Shared::default()));
        for text in ["one", ".two"] {
            shared
                .lock()
                .queue
                .push(Utterance {
                    text: text.to_owned(),
                    voice: Some("Anna".to_owned()),
                    rate: 150,
                })
                .unwrap();
        }
        let thread_shared = Arc::clone(&shared);
        let speaker = std::thread::spawn(move || {
            serve(&thread_shared, &rx, &mut session, || {
                Err(SpeechError::Failed)
            });
        });
        tx.send(Cmd::Wake).unwrap();
        let mut seen = Vec::new();
        while seen
            .iter()
            .filter(|s: &&String| s.ends_with("\r\n"))
            .count()
            < 2
        {
            seen.push(got.recv_timeout(REPLY).expect("the daemon heard both"));
        }
        assert!(seen.contains(&"SET self CLIENT_NAME user:fastcord:main".to_owned()));
        assert!(seen.contains(&"SET self SYNTHESIS_VOICE Anna".to_owned()));
        assert!(seen.contains(&"SET self RATE 25".to_owned()));
        let spoken: Vec<_> = seen.iter().filter(|s| s.ends_with("\r\n")).collect();
        assert_eq!(spoken, ["one\r\n", "..two\r\n"]);
        // The second end event leaves the speaker idle.
        let deadline = std::time::Instant::now() + REPLY;
        while shared.lock().busy && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!shared.lock().busy && shared.lock().queue.0.is_empty());
        tx.send(Cmd::Shutdown).unwrap();
        speaker.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
