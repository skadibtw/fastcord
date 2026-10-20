//! Uploading files with a message (`POST /channels/{id}/attachments`, then a
//! `PUT` of each file to the storage URL Discord names, then the message).
//!
//! The pieces are kept apart on purpose:
//!
//! - Discord's REST API (authenticated, rate limited) only ever receives JSON:
//!   the slot request and, in [`send`](crate::send), the message that refers to
//!   the uploaded files.
//! - The file bytes go to the storage URL through [`Uploader`], a separate
//!   client with no default headers, cookies or credentials, `https` only, no
//!   redirects, and a host allowlist. The user token can never reach it.
//! - A file is read in [`CHUNK`]-sized pieces while it is sent, never loaded
//!   whole. If it changes while it is being read the upload fails.
//!
//! Nothing here retries. A failed upload created no message, so retrying it is
//! always safe; only the final message `POST` can be ambiguous, and that is
//! [`SendError`](crate::SendError)'s business.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use fastcord_model::Snowflake;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::{Clock, Method, NetworkFailure, Priority, RestClient, RestError, RestRequest, Route};

/// Files one message can carry.
pub const MAX_UPLOAD_FILES: usize = 10;
/// Sanity ceiling for a selected file. Discord's real limit depends on the
/// account and the server and is enforced by Discord; this only keeps an
/// obviously impossible selection from starting.
pub const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// Longest file name sent.
pub const MAX_FILENAME_CHARS: usize = 255;
/// Bytes read from disk and handed to the network at a time.
pub const CHUNK: usize = 64 * 1024;

/// Hosts that may receive file bytes: Discord's storage and CDN domains.
const UPLOAD_HOST_SUFFIXES: &[&str] = &[
    ".storage.googleapis.com",
    ".discordapp.com",
    ".discordapp.net",
    ".discord.com",
    ".discord.media",
];

/// Why selecting, uploading or attaching a file failed. None of them left a
/// message behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadError {
    /// The file no longer exists or cannot be read.
    Missing,
    /// Not a regular file (a folder, a device).
    NotAFile,
    /// Zero bytes; Discord refuses empty files.
    Empty,
    /// Larger than [`MAX_UPLOAD_BYTES`].
    TooLarge,
    /// More than [`MAX_UPLOAD_FILES`].
    TooMany,
    /// The file's size or modification time changed after it was chosen.
    Changed,
    /// The name is empty or too long.
    BadName,
    /// Discord's answer to the slot request was unusable.
    BadSlots,
    /// The storage URL is not one this client may send a file to.
    UnsafeUrl,
    /// The storage service refused the file (expired, forbidden, too large).
    Rejected(u16),
    /// The slot request failed.
    Rest(RestError),
    /// The transfer failed or was cut.
    Network(NetworkFailure),
}

impl UploadError {
    /// A fixed, secret-free sentence for the user.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => "A file is missing or cannot be read.",
            Self::NotAFile => "Only files can be attached, not folders.",
            Self::Empty => "A file is empty; Discord does not accept empty files.",
            Self::TooLarge => "A file is larger than fastcord attaches.",
            Self::TooMany => "Discord allows at most 10 files per message.",
            Self::Changed => "A file changed after you chose it. Attach it again.",
            Self::BadName => "A file name is empty or too long.",
            Self::BadSlots => "Discord did not prepare the upload as expected.",
            Self::UnsafeUrl => "Discord named an upload address fastcord will not use.",
            Self::Rejected(401 | 403) => {
                "The upload was refused (the upload address expired or is not allowed)."
            }
            Self::Rejected(413) => "The file is larger than your account or this server allows.",
            Self::Rejected(_) => "The upload service refused the file.",
            Self::Rest(RestError::PermissionDenied) => "You cannot attach files in this channel.",
            Self::Rest(RestError::AuthenticationRequired) => {
                "Discord no longer accepts this login."
            }
            Self::Rest(RestError::Http(status)) if status.as_u16() == 413 => {
                "The files are larger than your account or this server allows."
            }
            Self::Rest(RestError::Http(_)) => {
                "Discord refused the upload (the files may be too large for your account or this server)."
            }
            Self::Rest(_) | Self::Network(_) => {
                "The upload was interrupted. Check the connection and retry."
            }
        }
    }
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for UploadError {}

/// A chosen local file, checked when chosen and again before it is sent. The
/// path is the user's own and is left out of `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct UploadFile {
    path: PathBuf,
    filename: String,
    size: u64,
    modified: Option<SystemTime>,
    /// Set for a file this program made (a pasted image): it is deleted when
    /// the last copy of this value is dropped.
    temporary: Option<Arc<TempFile>>,
    /// Set for a recorded voice message: it is sent with the voice-message flag.
    voice: Option<VoiceMeta>,
}

/// What a voice-message attachment carries besides its audio.
#[derive(Clone, PartialEq, Eq)]
pub struct VoiceMeta {
    pub duration_ms: u64,
    /// Base64 of the 0-255 loudness samples.
    pub waveform: String,
}

#[derive(PartialEq, Eq)]
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl fmt::Debug for UploadFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadFile")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl UploadFile {
    /// Checks that `path` is a non-empty regular file that can be attached.
    pub fn inspect(path: &Path) -> Result<Self, UploadError> {
        let metadata = std::fs::metadata(path).map_err(|_| UploadError::Missing)?;
        if !metadata.is_file() {
            return Err(UploadError::NotAFile);
        }
        if metadata.len() == 0 {
            return Err(UploadError::Empty);
        }
        if metadata.len() > MAX_UPLOAD_BYTES {
            return Err(UploadError::TooLarge);
        }
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty() && name.chars().count() <= MAX_FILENAME_CHARS)
            .ok_or(UploadError::BadName)?;
        Ok(Self {
            path: path.to_owned(),
            filename,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            temporary: None,
            voice: None,
        })
    }

    /// [`inspect`](Self::inspect) for a file the program created for this
    /// message. The file is removed once nothing refers to it any more.
    pub fn inspect_temporary(path: &Path) -> Result<Self, UploadError> {
        let guard = Arc::new(TempFile(path.to_owned()));
        let mut file = Self::inspect(path)?;
        file.temporary = Some(guard);
        Ok(file)
    }

    /// Marks this file as a voice message (flag, duration and waveform).
    pub fn into_voice_message(mut self, duration_ms: u64, waveform: &[u8]) -> Self {
        use base64::Engine;
        self.voice = Some(VoiceMeta {
            duration_ms,
            waveform: base64::engine::general_purpose::STANDARD.encode(waveform),
        });
        self
    }

    pub fn is_voice_message(&self) -> bool {
        self.voice.is_some()
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fails unless the file is still the one that was chosen.
    fn unchanged(&self) -> Result<(), UploadError> {
        let metadata = std::fs::metadata(&self.path).map_err(|_| UploadError::Missing)?;
        if !metadata.is_file() {
            return Err(UploadError::NotAFile);
        }
        if metadata.len() != self.size || metadata.modified().ok() != self.modified {
            return Err(UploadError::Changed);
        }
        Ok(())
    }
}

/// Bytes sent so far across all of one message's files; shared with the UI.
#[derive(Debug, Default)]
pub struct UploadProgress {
    sent: AtomicU64,
    total: AtomicU64,
}

impl UploadProgress {
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    fn start(&self, total: u64) {
        self.sent.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    fn add(&self, bytes: u64) {
        self.sent.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// A file Discord has stored, ready to be named by the message.
#[derive(Clone, PartialEq, Eq)]
pub struct UploadedFile {
    pub(crate) id: u32,
    pub(crate) filename: String,
    pub(crate) uploaded_filename: String,
    pub(crate) voice: Option<VoiceMeta>,
}

impl fmt::Debug for UploadedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadedFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct SlotFile<'a> {
    filename: &'a str,
    file_size: u64,
    id: String,
}

#[derive(Serialize)]
struct SlotRequest<'a> {
    files: Vec<SlotFile<'a>>,
}

#[derive(Deserialize)]
struct SlotResponse {
    attachments: Vec<Slot>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SlotId {
    Number(u32),
    Text(String),
}

impl SlotId {
    fn get(&self) -> Option<u32> {
        match self {
            Self::Number(n) => Some(*n),
            Self::Text(s) => s.parse().ok(),
        }
    }
}

/// Where to put one file. The URL is signed: never log it.
#[derive(Deserialize)]
struct Slot {
    id: SlotId,
    upload_url: String,
    upload_filename: String,
}

fn slots_route(channel: Snowflake) -> Result<Route, RestError> {
    Route::new(Method::POST, &format!("/channels/{channel}/attachments"))
}

/// Matches Discord's slots to the files by the index each was requested with.
fn match_slots(files: &[UploadFile], answer: SlotResponse) -> Result<Vec<Slot>, UploadError> {
    if answer.attachments.len() != files.len() {
        return Err(UploadError::BadSlots);
    }
    let mut slots: Vec<Option<Slot>> = files.iter().map(|_| None).collect();
    for slot in answer.attachments {
        let index = slot.id.get().ok_or(UploadError::BadSlots)? as usize;
        match slots.get_mut(index) {
            Some(place @ None) => *place = Some(slot),
            _ => return Err(UploadError::BadSlots),
        }
    }
    slots
        .into_iter()
        .map(|slot| slot.ok_or(UploadError::BadSlots))
        .collect()
}

/// The credential-free client that sends file bytes to storage.
#[derive(Clone)]
pub struct Uploader {
    http: reqwest::Client,
    allow_loopback_http: bool,
}

impl Uploader {
    pub fn new() -> Result<Self, UploadError> {
        Self::build(false)
    }

    /// Plain HTTP to loopback only, for tests against a local server.
    #[cfg(test)]
    fn for_tests() -> Result<Self, UploadError> {
        Self::build(true)
    }

    fn build(local: bool) -> Result<Self, UploadError> {
        let mut builder = reqwest::Client::builder()
            .use_rustls_tls()
            // No default headers, cookies or credentials of any kind, and a
            // redirect would carry the body somewhere nobody validated.
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .https_only(!local);
        if local {
            builder = builder.no_proxy();
        }
        Ok(Self {
            http: builder
                .build()
                .map_err(|_| UploadError::Network(NetworkFailure::Other))?,
            allow_loopback_http: local,
        })
    }

    fn validate(&self, raw: &str) -> Result<reqwest::Url, UploadError> {
        let url = reqwest::Url::parse(raw).map_err(|_| UploadError::UnsafeUrl)?;
        let host = url.host_str().ok_or(UploadError::UnsafeUrl)?;
        let loopback = self.allow_loopback_http && (host == "127.0.0.1" || host == "localhost");
        let allowed_host = loopback
            || UPLOAD_HOST_SUFFIXES
                .iter()
                .any(|suffix| host.to_ascii_lowercase().ends_with(suffix));
        let scheme_ok = url.scheme() == "https" || (loopback && url.scheme() == "http");
        if !scheme_ok || !allowed_host || !url.username().is_empty() || url.password().is_some() {
            return Err(UploadError::UnsafeUrl);
        }
        Ok(url)
    }

    /// Sends `file` to `url` in chunks. The file's bytes are counted into
    /// `progress` as they leave. Fails if the file is not exactly its chosen
    /// size when read to the end.
    async fn put(
        &self,
        url: &str,
        file: &UploadFile,
        progress: Arc<UploadProgress>,
    ) -> Result<(), UploadError> {
        let url = self.validate(url)?;
        file.unchanged()?;
        let handle = tokio::fs::File::open(&file.path)
            .await
            .map_err(|_| UploadError::Missing)?;
        let expected = file.size;
        let state = (handle, 0u64, progress, false);
        let stream = futures_util::stream::unfold(
            state,
            move |(mut handle, read, progress, done)| async move {
                if done {
                    return None;
                }
                let mut buffer = vec![0u8; CHUNK];
                let n = match handle.read(&mut buffer).await {
                    Ok(n) => n,
                    Err(error) => return Some((Err(error), (handle, read, progress, true))),
                };
                let read = read + n as u64;
                if read > expected || (n == 0 && read != expected) {
                    let error = std::io::Error::other("file changed while uploading");
                    return Some((Err(error), (handle, read, progress, true)));
                }
                if n == 0 {
                    return None;
                }
                buffer.truncate(n);
                progress.add(n as u64);
                Some((Ok(Bytes::from(buffer)), (handle, read, progress, false)))
            },
        );
        let response = self
            .http
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, expected)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|error| {
                if error.is_body() || error.is_request() {
                    // The body stream failed: the file changed or vanished.
                    UploadError::Changed
                } else {
                    UploadError::Network(NetworkFailure::from_reqwest(&error))
                }
            })?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(UploadError::Rejected(response.status().as_u16()))
        }
    }
}

impl<C: Clock> RestClient<C> {
    /// Asks Discord where to put `files`, then uploads each one. Returns what
    /// the message must name. Each file is checked again just before it is
    /// read, so one changed after it was chosen fails the upload instead of
    /// sending something the user did not pick.
    pub async fn upload_files(
        &self,
        uploader: &Uploader,
        channel: Snowflake,
        files: &[UploadFile],
        progress: Arc<UploadProgress>,
    ) -> Result<Vec<UploadedFile>, UploadError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        if files.len() > MAX_UPLOAD_FILES {
            return Err(UploadError::TooMany);
        }
        for file in files {
            file.unchanged()?;
        }
        progress.start(files.iter().map(|file| file.size).sum());
        let body = SlotRequest {
            files: files
                .iter()
                .enumerate()
                .map(|(index, file)| SlotFile {
                    filename: &file.filename,
                    file_size: file.size,
                    id: index.to_string(),
                })
                .collect(),
        };
        let request = slots_route(channel)
            .and_then(|route| RestRequest::new(route, Priority::UserWrite).json(&body))
            .map_err(UploadError::Rest)?;
        let response = self.execute(request).await.map_err(UploadError::Rest)?;
        let answer: SlotResponse =
            serde_json::from_slice(response.body()).map_err(|_| UploadError::BadSlots)?;
        let slots = match_slots(files, answer)?;
        let mut uploaded = Vec::with_capacity(files.len());
        for (index, (file, slot)) in files.iter().zip(slots).enumerate() {
            uploader
                .put(&slot.upload_url, file, Arc::clone(&progress))
                .await?;
            uploaded.push(UploadedFile {
                id: index as u32,
                filename: file.filename.clone(),
                uploaded_filename: slot.upload_filename,
                voice: file.voice.clone(),
            });
        }
        Ok(uploaded)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::*;

    fn temp_file(name: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("fastcord-upload-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("photo.png");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        (dir, path)
    }

    /// Serves one request: records the raw head and body, answers `status`.
    async fn serve(
        status: u16,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<(String, Vec<u8>)>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head_end, length) = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break (data.len(), 0);
                }
                data.extend_from_slice(&chunk[..n]);
                if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..at]).to_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (at + 4, length);
                }
            };
            while data.len() < head_end + length {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&chunk[..n]);
            }
            let reply =
                format!("HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = socket.write_all(reply.as_bytes()).await;
            let head = String::from_utf8_lossy(&data[..head_end.min(data.len())]).into_owned();
            (head, data[head_end.min(data.len())..].to_vec())
        });
        (address, task)
    }

    #[test]
    fn inspect_accepts_regular_files_and_refuses_the_rest() {
        let (dir, path) = temp_file("inspect", b"abc");
        let file = UploadFile::inspect(&path).unwrap();
        assert_eq!((file.filename(), file.size()), ("photo.png", 3));
        assert_eq!(UploadFile::inspect(&dir), Err(UploadError::NotAFile));
        assert_eq!(
            UploadFile::inspect(&dir.join("nope.txt")),
            Err(UploadError::Missing)
        );
        let empty = dir.join("empty.txt");
        std::fs::File::create(&empty).unwrap();
        assert_eq!(UploadFile::inspect(&empty), Err(UploadError::Empty));
        // The path never reaches Debug output.
        assert!(!format!("{file:?}").contains("fastcord-upload"));
        std::fs::write(&path, b"abcdef").unwrap();
        assert_eq!(file.unchanged(), Err(UploadError::Changed));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(file.unchanged(), Err(UploadError::Missing));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn slot_request_and_response_have_the_documented_shape() {
        let (dir, path) = temp_file("shape", b"abc");
        let file = UploadFile::inspect(&path).unwrap();
        let body = serde_json::to_value(SlotRequest {
            files: vec![SlotFile {
                filename: file.filename(),
                file_size: file.size(),
                id: "0".to_owned(),
            }],
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({"files": [{"filename": "photo.png", "file_size": 3, "id": "0"}]})
        );
        let answer: SlotResponse = serde_json::from_value(serde_json::json!({
            "attachments": [{"id": 0, "upload_url": "https://x", "upload_filename": "f/photo.png"}]
        }))
        .unwrap();
        assert_eq!(
            match_slots(std::slice::from_ref(&file), answer)
                .unwrap()
                .len(),
            1
        );
        // Wrong count, duplicate and out-of-range ids are refused.
        for bad in [
            serde_json::json!({"attachments": []}),
            serde_json::json!({"attachments": [
                {"id": "0", "upload_url": "u", "upload_filename": "a"},
                {"id": "0", "upload_url": "u", "upload_filename": "b"}]}),
            serde_json::json!({"attachments": [{"id": 7, "upload_url": "u", "upload_filename": "a"}]}),
        ] {
            let answer: SlotResponse = serde_json::from_value(bad).unwrap();
            assert_eq!(
                match_slots(std::slice::from_ref(&file), answer).err(),
                Some(UploadError::BadSlots)
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_discord_storage_over_https_without_credentials_may_receive_bytes() {
        let uploader = Uploader::new().unwrap();
        assert!(
            uploader
                .validate("https://discord-attachments-uploads-prd.storage.googleapis.com/x?sig=1")
                .is_ok()
        );
        for bad in [
            "http://discord-attachments-uploads-prd.storage.googleapis.com/x",
            "https://evil.example/x",
            "https://user:pw@cdn.discordapp.com/x",
            "https://discordapp.com.evil.example/x",
            "https://127.0.0.1/x",
            "ftp://cdn.discordapp.com/x",
            "not a url",
        ] {
            assert_eq!(
                uploader.validate(bad).err(),
                Some(UploadError::UnsafeUrl),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn the_file_streams_without_credentials_and_progress_counts_every_byte() {
        let content: Vec<u8> = (0..(CHUNK * 3 + 17)).map(|n| (n % 251) as u8).collect();
        let (dir, path) = temp_file("stream", &content);
        let file = UploadFile::inspect(&path).unwrap();
        let (address, seen) = serve(200).await;
        let progress = Arc::new(UploadProgress::default());
        Uploader::for_tests()
            .unwrap()
            .put(
                &format!("http://{address}/up?sig=abc"),
                &file,
                Arc::clone(&progress),
            )
            .await
            .unwrap();
        let (head, body) = seen.await.unwrap();
        let head = head.to_lowercase();
        assert!(head.starts_with("put /up?sig=abc http/1.1"));
        assert!(!head.contains("authorization") && !head.contains("cookie"));
        assert_eq!(body, content);
        assert_eq!(progress.sent(), content.len() as u64);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn refusals_and_a_file_that_shrinks_midway_fail_the_upload() {
        let (dir, path) = temp_file("refused", &vec![7u8; CHUNK * 2]);
        let file = UploadFile::inspect(&path).unwrap();
        for status in [401u16, 403, 413] {
            let (address, _seen) = serve(status).await;
            let error = Uploader::for_tests()
                .unwrap()
                .put(&format!("http://{address}/u"), &file, Arc::default())
                .await
                .err();
            assert_eq!(error, Some(UploadError::Rejected(status)));
        }
        // A file shorter than the size announced fails instead of completing.
        let shrunk = UploadFile {
            size: file.size + 10,
            ..file.clone()
        };
        let (address, _seen) = serve(200).await;
        let error = Uploader::for_tests()
            .unwrap()
            .put(&format!("http://{address}/u"), &shrunk, Arc::default())
            .await
            .err();
        assert!(matches!(
            error,
            Some(UploadError::Changed | UploadError::Network(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
