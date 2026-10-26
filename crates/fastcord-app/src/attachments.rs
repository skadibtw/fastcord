//! Unauthenticated CDN fetches and byte-bounded attachment caches (SPEC §5.3).
//!
//! Signed URLs are transport-only inputs; cache identity is the account/channel/
//! message/attachment tuple and never contains a URL or its query string.
//!
//! * One separate HTTP client with no credentials of any kind fetches the CDN.
//!   Redirects are followed by hand, each hop re-validated, so nothing but the
//!   plain `GET` (with the signed query kept as the server wrote it) is ever sent.
//!   It carries only the public profile's `User-Agent` and the `Accept` of the
//!   fetch purpose (Chromium's image list for image subresources, `*/*` for a
//!   file download): no cookie, `Authorization`, fingerprint, installation id or
//!   `x-super-properties` (IDENTITY_AUDIT C2).
//! * A 403/404 asks the authenticated API for a fresh URL once and retries once.
//! * Compressed bytes live in an account-scoped disk LRU (256 MiB across all
//!   accounts, atomic writes); decoded thumbnails in a 12 MiB CPU LRU.
//! * At most four fetches and two decodes run at once, all off the UI thread.
//!   Image dimensions are read from the header and checked before any pixel
//!   buffer is allocated; thumbnails are scaled down to the display size.
//! * An explicit download streams straight to the file the user picks, through a
//!   `.part` file that is renamed into place on success. It never enters the
//!   cache and is not bounded by the cache size.
use crate::i18n::tr;
use fastcord_model::{Attachment, Snowflake};
use image::imageops::FilterType;
use image::{ImageDecoder, ImageError, Limits};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub const DISK_CACHE_LIMIT: u64 = 256 * 1024 * 1024;
pub const DECODED_CACHE_LIMIT: usize = 12 * 1024 * 1024;
/// Largest image side and pixel count that is decoded at all.
pub const MAX_IMAGE_DIMENSION: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
/// Largest side a thumbnail or viewer image is scaled to.
pub const MAX_DISPLAY_DIMENSION: u32 = 2048;
/// Compressed bytes fetched for an image preview.
pub const MAX_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Compressed bytes an Open may cache. Four may be in flight, so four of them
/// always fit in the disk cache. Explicit downloads are not bounded by this.
pub const MAX_FILE_BYTES: u64 = DISK_CACHE_LIMIT / 4;
/// Decoder allocation ceiling: one 16-bit RGBA frame at the pixel limit.
const MAX_DECODE_ALLOC: u64 = MAX_IMAGE_PIXELS * 8 + 32 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const FETCH_SLOTS: usize = 4;
/// User downloads to a chosen path have their own slots, so a long one never
/// holds up avatars, emoji and previews (#167).
const DOWNLOAD_SLOTS: usize = 2;
const DECODE_SLOTS: usize = 2;
const CACHE_FORMAT_VERSION: &str = "attachment-v2";
/// Chromium's `Accept` for an image subresource (avatar, emoji, preview, embed).
const IMAGE_ACCEPT: &str = "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";
/// Chromium's `Accept` for a plain `fetch()` of a file: no type preference.
const FILE_ACCEPT: &str = "*/*";
/// The bundled desktop browser user agent, as the app passes it in; tests only.
#[cfg(test)]
const TEST_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) discord/1.0.9265 Chrome/148.0.7778.280 Electron/42.11.15 Safari/537.36";

/// Stable identity independent of expiring CDN signatures.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct AttachmentKey {
    pub account: Snowflake,
    pub channel: Snowflake,
    pub message: Snowflake,
    pub attachment: Snowflake,
}

impl AttachmentKey {
    fn disk_name(self) -> String {
        let mut hash = Sha256::new();
        hash.update(CACHE_FORMAT_VERSION);
        hash.update(self.account.0.to_le_bytes());
        hash.update(self.channel.0.to_le_bytes());
        hash.update(self.message.0.to_le_bytes());
        hash.update(self.attachment.0.to_le_bytes());
        format!("{:x}", hash.finalize())
    }

    /// File name in the account directory; the extension only helps the OS
    /// choose an application and is reduced to a safe token.
    fn file_name(self, filename: &str) -> String {
        format!("{}.{}", self.disk_name(), safe_extension(filename))
    }
}

/// Obtains a fresh signed URL through the authenticated API. The fetcher calls
/// it at most once per fetch, after the CDN answered 403 or 404.
pub trait UrlRefresh: Send + Sync {
    fn refresh(&self, url: String) -> impl Future<Output = Result<String, AttachmentError>> + Send;
}

/// Account-scoped cache; all blocking file and image work runs on the blocking
/// pool, never the caller's thread.
#[derive(Clone)]
pub struct AttachmentCache {
    account: Snowflake,
    root: Arc<PathBuf>,
    /// Sized public images (`thumbs`): a sibling tree with its own byte bound.
    thumbs: Arc<PathBuf>,
    http: fastcord_net::HttpClient,
    decoded: Arc<Mutex<DecodedLru>>,
    fetch_slots: Arc<tokio::sync::Semaphore>,
    download_slots: Arc<tokio::sync::Semaphore>,
    decode_slots: Arc<tokio::sync::Semaphore>,
    active: Arc<AtomicBool>,
    prepared: Arc<AtomicBool>,
    allow_local_http: bool,
    #[cfg(test)]
    decode_gate: Option<Arc<DecodeGate>>,
}

impl std::fmt::Debug for AttachmentCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentCache")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Shared RGBA pixels of a thumbnail no larger than the requested display
    /// box. Cloning shares the buffer, including into an iced image handle.
    pub rgba: bytes::Bytes,
}

impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Small emoji frames only. A single animation is at most 512 KiB of RGBA;
/// callers also bound all visible frame handles and cancel off-screen work.
#[derive(Clone, Debug)]
pub struct DecodedAnimation {
    pub frames: Vec<DecodedImage>,
    pub delays: Vec<Duration>,
}

const MAX_EMOJI_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EMOJI_FRAMES: usize = 120;
const MAX_EMOJI_RGBA: usize = 512 * 1024;
const EMOJI_SIDE: u32 = 32;
/// Avatars and icons are requested at `size=64` and never decoded larger.
pub const AVATAR_SIDE: u32 = 64;
const MAX_AVATAR_FILE_BYTES: u64 = 512 * 1024;
const MAX_EMBED_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOUND_FILE_BYTES: u64 = fastcord_audio::soundboard::MAX_INPUT_BYTES as u64;

struct PublicEmoji;
impl UrlRefresh for PublicEmoji {
    async fn refresh(&self, _url: String) -> Result<String, AttachmentError> {
        // An emoji URL is public and unsigned. Deleted/unavailable emoji keep
        // their text fallback; never request attachment refresh or send a token.
        Err(AttachmentError::Http(404))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentError {
    InvalidUrl,
    Network,
    Http(u16),
    TooLarge,
    UnsupportedImage,
    InvalidImage,
    CacheUnavailable,
    DestinationUnavailable,
    WorkerStopped,
    RefreshFailed,
}

impl std::fmt::Display for AttachmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidUrl => tr("Attachment URL is invalid."),
            Self::Network => tr("Attachment could not be fetched."),
            Self::Http(_) => tr("Attachment server rejected the request."),
            Self::TooLarge => tr("Attachment exceeds the local image or open limit."),
            Self::UnsupportedImage => tr("This attachment is not a supported image."),
            Self::InvalidImage => tr("The image data could not be decoded."),
            Self::CacheUnavailable => tr("The local attachment cache is unavailable."),
            Self::DestinationUnavailable => tr("The file could not be saved there."),
            Self::WorkerStopped => tr("Attachment work was cancelled."),
            Self::RefreshFailed => tr("The attachment URL could not be refreshed."),
        })
    }
}

impl std::error::Error for AttachmentError {}

fn build_http(
    network: &fastcord_net::NetworkContext,
    local: bool,
    user_agent: &str,
) -> Result<fastcord_net::HttpClient, AttachmentError> {
    let builder = network.http_builder(fastcord_net::Role::Cdn);
    #[cfg(test)]
    let builder = if local {
        network.debug_loopback_http_builder(fastcord_net::Role::Cdn)
    } else {
        builder
    };
    let builder = builder
        .map_err(|_| AttachmentError::Network)?
        .use_rustls_tls()
        // Only the public `User-Agent`: no cookies, credentials or identity
        // headers. Redirects are followed below, one validated hop at a time.
        .user_agent(user_agent)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .https_only(!local);
    let builder = if local { builder.no_proxy() } else { builder };
    builder.build().map_err(|_| AttachmentError::Network)
}

impl AttachmentCache {
    pub fn new(
        network: &fastcord_net::NetworkContext,
        account: Snowflake,
        root: PathBuf,
        user_agent: &str,
    ) -> Result<Self, AttachmentError> {
        Self::with_http(
            account,
            root,
            build_http(network, false, user_agent)?,
            false,
        )
    }

    fn with_http(
        account: Snowflake,
        root: PathBuf,
        http: fastcord_net::HttpClient,
        allow_local_http: bool,
    ) -> Result<Self, AttachmentError> {
        Ok(Self {
            account,
            thumbs: Arc::new(root.join("images").join(account.0.to_string())),
            root: Arc::new(root.join(account.0.to_string())),
            http,
            decoded: Arc::new(Mutex::new(DecodedLru::default())),
            fetch_slots: Arc::new(tokio::sync::Semaphore::new(FETCH_SLOTS)),
            download_slots: Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_SLOTS)),
            decode_slots: Arc::new(tokio::sync::Semaphore::new(DECODE_SLOTS)),
            active: Arc::new(AtomicBool::new(true)),
            prepared: Arc::new(AtomicBool::new(false)),
            allow_local_http,
            #[cfg(test)]
            decode_gate: None,
        })
    }

    /// A cache whose HTTP client may reach `http://127.0.0.1`; tests only.
    #[cfg(test)]
    fn for_local_tests(account: Snowflake, root: PathBuf) -> Self {
        Self::with_http(
            account,
            root,
            build_http(&fastcord_net::NetworkContext::off(), true, TEST_USER_AGENT).unwrap(),
            true,
        )
        .unwrap()
    }

    pub fn default_root(
        network: &fastcord_net::NetworkContext,
        account: Snowflake,
        user_agent: &str,
    ) -> Result<Self, AttachmentError> {
        let base = cache_base().ok_or(AttachmentError::CacheUnavailable)?;
        Self::new(
            network,
            account,
            base.join(crate::audio_devices::app_dir_name())
                .join("attachments"),
            user_agent,
        )
    }

    fn ensure_active(&self) -> Result<(), AttachmentError> {
        if self.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(AttachmentError::WorkerStopped)
        }
    }

    fn check(&self, key: AttachmentKey) -> Result<(), AttachmentError> {
        if key.account != self.account {
            return Err(AttachmentError::InvalidUrl);
        }
        self.ensure_active()
    }

    /// Cancels this cache's work and removes the account's files. Nothing the
    /// cache started can write again afterwards; build a new cache to resume.
    pub async fn purge(&self) -> Result<(), AttachmentError> {
        self.active.store(false, Ordering::Release);
        {
            let mut decoded = self.decoded.lock();
            *decoded = DecodedLru::default();
        }
        let root = self.root.clone();
        let thumbs = self.thumbs.clone();
        tokio::task::spawn_blocking(move || {
            let _ = fs::remove_dir_all(thumbs.as_path());
            crate::disk_index::forget_dir(thumbs.as_path());
            // A just-cancelled download may still be closing its file.
            for attempt in 0..5 {
                let removed = fs::remove_dir_all(root.as_path());
                crate::disk_index::forget_dir(root.as_path());
                match removed {
                    Ok(()) => return Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(_) if attempt < 4 => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => {}
                }
            }
            Err(AttachmentError::CacheUnavailable)
        })
        .await
        .map_err(|_| AttachmentError::WorkerStopped)?
    }

    /// Returns the cached file for `key`, fetching it first if needed: the URL
    /// exactly as supplied, and after a 403/404 once more with the URL the
    /// authenticated API returns. Files over `limit` bytes are never stored.
    pub async fn fetch_file<R: UrlRefresh>(
        &self,
        refresh: &R,
        key: AttachmentKey,
        filename: &str,
        url: &str,
        limit: u64,
    ) -> Result<PathBuf, AttachmentError> {
        self.fetch_file_accepting(refresh, key, filename, url, limit, FILE_ACCEPT)
            .await
    }

    /// `fetch_file` with the `Accept` of the fetch purpose: Chromium's image list
    /// for an image subresource, `*/*` for a file.
    async fn fetch_file_accepting<R: UrlRefresh>(
        &self,
        refresh: &R,
        key: AttachmentKey,
        filename: &str,
        url: &str,
        limit: u64,
        accept: &'static str,
    ) -> Result<PathBuf, AttachmentError> {
        self.check(key)?;
        let _permit = self
            .fetch_slots
            .acquire()
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?;
        let name = key.file_name(filename);
        if let Some(path) = self.cached(name.clone(), limit).await? {
            return Ok(path);
        }
        let temporary = self.temporary_path(&name);
        let cleanup = TemporaryFile(Some(temporary.clone()));
        let result = self
            .download_and_store(refresh, url, limit, &name, &temporary, accept)
            .await;
        cleanup.remove().await;
        result
    }

    /// Streams `url` straight to `destination`, the path the user chose in the
    /// save dialog. Nothing enters the cache, so the cache size limit does not
    /// apply. The bytes go to a `.part` file beside the destination, renamed into
    /// place only once complete: a failed or cancelled download leaves the
    /// destination exactly as it was.
    pub async fn download_to<R: UrlRefresh>(
        &self,
        refresh: &R,
        key: AttachmentKey,
        url: &str,
        destination: &Path,
    ) -> Result<(), AttachmentError> {
        self.check(key)?;
        let _permit = self
            .download_slots
            .acquire()
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?;
        let response = self
            .fetch_response_refreshing(refresh, url, FILE_ACCEPT)
            .await?;
        let part = partial_path(destination)?;
        // The guard exists before the open is awaited: the file appears on
        // disk as soon as the blocking open runs, and a cancellation in that
        // window would otherwise leave it behind for good.
        let cleanup = TemporaryFile(Some(part.clone()));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
            .map_err(|_| AttachmentError::DestinationUnavailable)?;
        match self.write_part(response, file, &part, destination).await {
            Ok(()) => {
                cleanup.keep();
                Ok(())
            }
            Err(error) => {
                cleanup.remove().await;
                Err(error)
            }
        }
    }

    /// Writes `response`'s body into `file`, then moves the finished `part` onto
    /// `destination`. Stops at once if the cache is purged.
    async fn write_part(
        &self,
        response: reqwest::Response,
        mut file: tokio::fs::File,
        part: &Path,
        destination: &Path,
    ) -> Result<(), AttachmentError> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            self.ensure_active()?;
            let chunk = chunk.map_err(|_| AttachmentError::Network)?;
            file.write_all(&chunk)
                .await
                .map_err(|_| AttachmentError::DestinationUnavailable)?;
        }
        file.flush()
            .await
            .map_err(|_| AttachmentError::DestinationUnavailable)?;
        file.sync_all()
            .await
            .map_err(|_| AttachmentError::DestinationUnavailable)?;
        drop(file);
        self.ensure_active()?;
        let part = part.to_owned();
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || fs::rename(&part, &destination))
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?
            .map_err(|_| AttachmentError::DestinationUnavailable)
    }

    async fn download_and_store<R: UrlRefresh>(
        &self,
        refresh: &R,
        url: &str,
        limit: u64,
        name: &str,
        temporary: &Path,
        accept: &'static str,
    ) -> Result<PathBuf, AttachmentError> {
        let response = self.fetch_response_refreshing(refresh, url, accept).await?;
        let size = self.download(response, limit, temporary).await?;
        let root = self.root.clone();
        let active = self.active.clone();
        let temporary = temporary.to_owned();
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            if !active.load(Ordering::Acquire) {
                return Err(AttachmentError::WorkerStopped);
            }
            let destination = root.join(name);
            if fs::rename(&temporary, &destination).is_err() {
                // Windows cannot rename over an existing file.
                let _ = fs::remove_file(&destination);
                fs::rename(&temporary, &destination)
                    .map_err(|_| AttachmentError::CacheUnavailable)?;
            }
            debug_assert!(size <= limit);
            // No scan: `reserve` already made room for the whole body, so the
            // ledger only swaps the partial's charge for the stored file's (#168).
            let (victims, _) =
                crate::disk_index::record(&destination, size, Some(&temporary), DISK_CACHE_LIMIT);
            crate::disk_index::delete(victims);
            if !active.load(Ordering::Acquire) {
                // A purge ran meanwhile; leave nothing in the removed account.
                let _ = fs::remove_file(&destination);
                crate::disk_index::forget(&destination);
                return Err(AttachmentError::WorkerStopped);
            }
            Ok(destination)
        })
        .await
        .map_err(|_| AttachmentError::WorkerStopped)?
    }

    fn temporary_path(&self, name: &str) -> PathBuf {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!(
            "{name}-{}-{sequence}.{TEMPORARY_EXTENSION}",
            std::process::id()
        ))
    }

    /// The file if it is already cached (and refreshes its recency).
    async fn cached(&self, name: String, limit: u64) -> Result<Option<PathBuf>, AttachmentError> {
        let root = self.root.clone();
        let active = self.active.clone();
        let prepared = self.prepared.clone();
        tokio::task::spawn_blocking(move || {
            if !active.load(Ordering::Acquire) {
                return Err(AttachmentError::WorkerStopped);
            }
            prepare_root(&root, &prepared)?;
            crate::disk_index::ensure(root.parent().unwrap_or(&root))?;
            let path = root.join(name);
            match fs::metadata(&path) {
                Ok(metadata) if metadata.len() <= limit => {
                    let _ = touch(&path);
                    Ok(Some(path))
                }
                Ok(_) => Err(AttachmentError::TooLarge),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(AttachmentError::CacheUnavailable),
            }
        })
        .await
        .map_err(|_| AttachmentError::WorkerStopped)?
    }

    /// Fetches `url` with the credential-free client, following redirects by
    /// hand. A 4xx or 5xx status is an error, and no body is read.
    async fn fetch_response(
        &self,
        url: &str,
        accept: &'static str,
    ) -> Result<reqwest::Response, AttachmentError> {
        self.ensure_active()?;
        let response =
            send_following_redirects(&self.http, url, self.allow_local_http, accept).await?;
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            return Err(AttachmentError::Http(status.as_u16()));
        }
        Ok(response)
    }

    /// `fetch_response`, except that after a 403 or 404 it asks for a fresh URL
    /// once and fetches that instead.
    async fn fetch_response_refreshing<R: UrlRefresh>(
        &self,
        refresh: &R,
        url: &str,
        accept: &'static str,
    ) -> Result<reqwest::Response, AttachmentError> {
        match self.fetch_response(url, accept).await {
            Err(AttachmentError::Http(403 | 404)) => {
                let fresh = refresh.refresh(url.to_owned()).await?;
                self.fetch_response(&fresh, accept).await
            }
            other => other,
        }
    }

    /// Streams `response`'s body into `temporary`, returning its size. Evicts old
    /// files first so the cache stays within its limit while the file grows.
    async fn download(
        &self,
        response: reqwest::Response,
        limit: u64,
        temporary: &Path,
    ) -> Result<u64, AttachmentError> {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        if let Some(length) = response.content_length()
            && length > limit
        {
            return Err(AttachmentError::TooLarge);
        }
        // Reserve the entire body atomically, including unknown-length bodies.
        // A sparse partial file accounts for the reservation while streaming;
        // otherwise concurrent fetches could all reserve the same free bytes.
        let (reserved, temporary_cleanup) = self
            .reserve(temporary, response.content_length().unwrap_or(limit))
            .await?;
        // fetch_file now owns cleanup; the reservation guard covered cancellation
        // while the blocking reservation itself was still running.
        temporary_cleanup.keep();
        let mut file = tokio::fs::File::from_std(reserved);
        let mut stream = response.bytes_stream();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            self.ensure_active()?;
            let chunk = chunk.map_err(|_| AttachmentError::Network)?;
            written += chunk.len() as u64;
            if written > limit {
                return Err(AttachmentError::TooLarge);
            }
            file.write_all(&chunk)
                .await
                .map_err(|_| AttachmentError::CacheUnavailable)?;
        }
        file.set_len(written)
            .await
            .map_err(|_| AttachmentError::CacheUnavailable)?;
        file.flush()
            .await
            .map_err(|_| AttachmentError::CacheUnavailable)?;
        drop(file);
        Ok(written)
    }

    /// The body of `url`, at most `limit` bytes, held in memory: for small public
    /// images that are decoded at once and never served from this cache.
    async fn fetch_bytes<R: UrlRefresh>(
        &self,
        refresh: &R,
        url: &str,
        limit: u64,
        accept: &'static str,
    ) -> Result<Vec<u8>, AttachmentError> {
        use futures_util::StreamExt;

        self.ensure_active()?;
        let _permit = self
            .fetch_slots
            .acquire()
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?;
        let response = self.fetch_response_refreshing(refresh, url, accept).await?;
        if response
            .content_length()
            .is_some_and(|length| length > limit)
        {
            return Err(AttachmentError::TooLarge);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            self.ensure_active()?;
            let chunk = chunk.map_err(|_| AttachmentError::Network)?;
            if (body.len() + chunk.len()) as u64 > limit {
                return Err(AttachmentError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Atomically accounts for a complete in-flight file before its first byte
    /// is written. Dropping the guard cleans up cancelled or failed downloads.
    async fn reserve(
        &self,
        temporary: &Path,
        bytes: u64,
    ) -> Result<(fs::File, TemporaryFile), AttachmentError> {
        let temporary = temporary.to_owned();
        let root = self.root.clone();
        let active = self.active.clone();
        let prepared = self.prepared.clone();
        tokio::task::spawn_blocking(move || {
            if !active.load(Ordering::Acquire) {
                return Err(AttachmentError::WorkerStopped);
            }
            prepare_root(&root, &prepared)?;
            crate::disk_index::ensure(root.parent().unwrap_or(&root))?;
            // Charge the whole partial, evicting older files from the ledger;
            // only the unlinks touch the disk (#168).
            let (victims, fits) =
                crate::disk_index::record(&temporary, bytes, None, DISK_CACHE_LIMIT);
            crate::disk_index::delete(victims);
            if !fits {
                return Err(AttachmentError::CacheUnavailable);
            }
            let cleanup = TemporaryFile(Some(temporary.clone()));
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| AttachmentError::CacheUnavailable)?;
            file.set_len(bytes)
                .map_err(|_| AttachmentError::CacheUnavailable)?;
            Ok((file, cleanup))
        })
        .await
        .map_err(|_| AttachmentError::WorkerStopped)?
    }

    /// A cached local file with a safe extension for the OS default handler. The
    /// file is the cache entry itself. Explicit downloads stream to their chosen
    /// path through `download_to` instead.
    pub async fn local_file<R: UrlRefresh>(
        &self,
        refresh: &R,
        key: AttachmentKey,
        filename: &str,
        url: &str,
    ) -> Result<PathBuf, AttachmentError> {
        self.fetch_file(refresh, key, filename, url, MAX_FILE_BYTES)
            .await
    }

    /// An image decoded to at most `max_width` x `max_height` (clamped to
    /// [`MAX_DISPLAY_DIMENSION`]), from the cache when possible.
    pub async fn thumbnail<R: UrlRefresh>(
        &self,
        refresh: &R,
        key: AttachmentKey,
        attachment: &Attachment,
        max_width: u32,
        max_height: u32,
    ) -> Result<DecodedImage, AttachmentError> {
        self.check(key)?;
        let dims = (
            max_width.clamp(1, MAX_DISPLAY_DIMENSION),
            max_height.clamp(1, MAX_DISPLAY_DIMENSION),
        );
        let cache_key = (key, dims.0, dims.1);
        if let Some(image) = self.decoded.lock().get(cache_key) {
            return Ok(image);
        }
        // Declared sizes are hints from the message: refuse obviously huge
        // images before any request, then verify against the real header.
        if attachment.size > MAX_IMAGE_FILE_BYTES
            || attachment
                .width
                .zip(attachment.height)
                .is_some_and(|(width, height)| exceeds_pixel_limits(width, height))
        {
            return Err(AttachmentError::TooLarge);
        }
        let path = self
            .fetch_file_accepting(
                refresh,
                key,
                &attachment.filename,
                &attachment.url,
                MAX_IMAGE_FILE_BYTES,
                IMAGE_ACCEPT,
            )
            .await?;
        let permit = self
            .decode_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?;
        self.ensure_active()?;
        #[cfg(test)]
        let gate = self.decode_gate.clone();
        let image = tokio::task::spawn_blocking(move || {
            // A cancelled future cannot stop spawn_blocking. The permit must
            // belong to that worker until it actually finishes, not its caller.
            let _permit = permit;
            #[cfg(test)]
            if let Some(gate) = gate
                && gate.entered.fetch_add(1, Ordering::SeqCst) < DECODE_SLOTS
            {
                gate.started.wait();
                gate.release.wait();
            }
            decode_thumbnail(&path, dims.0, dims.1)
        })
        .await
        .map_err(|_| AttachmentError::WorkerStopped)??;
        self.ensure_active()?;
        self.decoded.lock().insert(cache_key, image.clone());
        Ok(image)
    }

    pub async fn custom_emoji(
        &self,
        id: Snowflake,
        animated: bool,
    ) -> Result<bytes::Bytes, AttachmentError> {
        let extension = if animated { "gif" } else { "png" };
        // Discord channel IDs are nonzero; the reserved zero-channel identity
        // cannot collide with an account's private attachment cache entry.
        let key = AttachmentKey {
            account: self.account,
            channel: Snowflake(0),
            message: Snowflake(u64::from(animated)),
            attachment: id,
        };
        let path = self
            .fetch_file_accepting(
                &PublicEmoji,
                key,
                &format!("emoji.{extension}"),
                &format!(
                    "https://cdn.discordapp.com/emojis/{id}.{extension}?size=32&quality=lossless"
                ),
                MAX_EMOJI_FILE_BYTES,
                IMAGE_ACCEPT,
            )
            .await?;
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|_| AttachmentError::CacheUnavailable)?;
        if bytes.len() as u64 > MAX_EMOJI_FILE_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        self.ensure_active()?;
        Ok(bytes.into())
    }

    /// A soundboard sound from the public CDN through the same credential-free fetcher
    /// and disk LRU, at most [`MAX_SOUND_FILE_BYTES`]. The file is decoded by the caller
    /// on a blocking worker. The cache identity is the sound ID; channel 1 cannot be a
    /// real channel and so cannot collide with an attachment.
    pub async fn soundboard_file(&self, id: Snowflake) -> Result<PathBuf, AttachmentError> {
        let key = AttachmentKey {
            account: self.account,
            channel: Snowflake(1),
            message: Snowflake(0),
            attachment: id,
        };
        self.fetch_file(
            &PublicEmoji,
            key,
            "sound.audio",
            &format!("https://cdn.discordapp.com/soundboard-sounds/{id}"),
            MAX_SOUND_FILE_BYTES,
        )
        .await
    }

    /// A public avatar or server icon (`kind` 2 = user, 3 = server icon, 4 =
    /// default avatar) from the same credential-free fetcher and disk LRU,
    /// decoded to at most [`AVATAR_SIDE`] square on the shared decode slots. The
    /// cache identity is the kind, id and hash, never the URL.
    pub async fn avatar(
        &self,
        kind: u8,
        id: Snowflake,
        hash: &str,
        url: &str,
    ) -> Result<DecodedImage, AttachmentError> {
        self.public_image(
            kind,
            id,
            hash,
            url,
            MAX_AVATAR_FILE_BYTES,
            (AVATAR_SIDE, AVATAR_SIDE),
        )
        .await
    }

    /// A link embed's thumbnail or image from Discord's media proxy: same
    /// credential-free fetcher and disk LRU, at most [`MAX_EMBED_FILE_BYTES`],
    /// decoded to fit `fit` (width, height). `hash` identifies the picture
    /// without the signed query (kind 7).
    pub async fn embed_image(
        &self,
        hash: &str,
        url: &str,
        fit: (u32, u32),
    ) -> Result<DecodedImage, AttachmentError> {
        self.public_image(7, Snowflake(0), hash, url, MAX_EMBED_FILE_BYTES, fit)
            .await
    }

    async fn public_image(
        &self,
        kind: u8,
        id: Snowflake,
        hash: &str,
        url: &str,
        limit: u64,
        fit: (u32, u32),
    ) -> Result<DecodedImage, AttachmentError> {
        // A sized copy from an earlier run answers with one small file read:
        // no fetch slot, no network, no decode.
        let thumb = crate::thumbs::name(kind, id.0, hash, fit);
        {
            let (dir, active, name) = (self.thumbs.clone(), self.active.clone(), thumb.clone());
            let hit = tokio::task::spawn_blocking(move || {
                active
                    .load(Ordering::Acquire)
                    .then(|| crate::thumbs::read(&dir, &name))
                    .flatten()
            })
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?;
            if let Some(image) = hit {
                self.ensure_active()?;
                return Ok(image);
            }
        }
        // When the sized copy is sure to fit the thumbnail store (avatars,
        // icons, stickers), it is the only copy ever read again: decode the
        // original from memory and do not store it (#168). Larger pictures keep
        // their compressed copy in the attachment cache.
        let source =
            if u64::from(fit.0) * u64::from(fit.1) * 4 <= crate::thumbs::MAX_THUMB_BYTES as u64 {
                Source::Memory(
                    self.fetch_bytes(&PublicEmoji, url, limit, IMAGE_ACCEPT)
                        .await?,
                )
            } else {
                // Zero channel plus a message >= 2 cannot collide with emoji (0/1)
                // or with any real attachment.
                let digest = Sha256::digest(hash.as_bytes());
                let mut fold = [0u8; 8];
                fold.copy_from_slice(&digest[..8]);
                let key = AttachmentKey {
                    account: self.account,
                    channel: Snowflake(0),
                    message: Snowflake((u64::from_le_bytes(fold) << 8) | u64::from(kind.max(2))),
                    attachment: id,
                };
                Source::File(
                    self.fetch_file_accepting(
                        &PublicEmoji,
                        key,
                        "avatar.png",
                        url,
                        limit,
                        IMAGE_ACCEPT,
                    )
                    .await?,
                )
            };
        let permit = self
            .decode_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?;
        self.ensure_active()?;
        let image = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match source {
                Source::Memory(body) => {
                    decode_thumbnail_from(std::io::Cursor::new(body), fit.0, fit.1)
                }
                Source::File(path) => decode_thumbnail(&path, fit.0, fit.1),
            }
        })
        .await
        .map_err(|_| AttachmentError::WorkerStopped)??;
        self.ensure_active()?;
        let (dir, active, stored) = (self.thumbs.clone(), self.active.clone(), image.clone());
        // Best effort: a failed write only costs a decode next launch.
        let _ = tokio::task::spawn_blocking(move || {
            if active.load(Ordering::Acquire) {
                let _ = crate::thumbs::write(&dir, &thumb, &stored);
            }
        })
        .await;
        Ok(image)
    }

    /// Shares the attachment decoder semaphore: no second independent pool.
    /// PNGs are embedded compressed bytes; animated custom assets are bounded
    /// cached GIFs, decoded one frame at a time off the UI thread.
    pub async fn decode_emoji(
        &self,
        data: bytes::Bytes,
    ) -> Result<DecodedAnimation, AttachmentError> {
        self.ensure_active()?;
        if data.len() as u64 > MAX_EMOJI_FILE_BYTES {
            return Err(AttachmentError::TooLarge);
        }
        let permit = self
            .decode_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AttachmentError::WorkerStopped)?;
        let active = self.active.clone();
        #[cfg(test)]
        let gate = self.decode_gate.clone();
        let animation = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            #[cfg(test)]
            if let Some(gate) = gate
                && gate.entered.fetch_add(1, Ordering::SeqCst) < DECODE_SLOTS
            {
                gate.started.wait();
                gate.release.wait();
            }
            decode_small_animation(data, &active)
        })
        .await
        .map_err(|_| AttachmentError::WorkerStopped)??;
        self.ensure_active()?;
        Ok(animation)
    }
}

fn prepare_root(root: &Path, prepared: &AtomicBool) -> Result<(), AttachmentError> {
    fs::create_dir_all(root).map_err(|_| AttachmentError::CacheUnavailable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(|_| AttachmentError::CacheUnavailable)?;
    }
    if !prepared.swap(true, Ordering::AcqRel) {
        // Partial downloads left by a crash are never valid cache entries.
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                if is_temporary(&entry.path()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    Ok(())
}

/// Sends `GET`s from the credential-free client, following redirects by hand:
/// each hop must again be an HTTPS URL without embedded credentials. Only the
/// public `User-Agent` (a client default) and `accept` are sent.
async fn send_following_redirects(
    http: &fastcord_net::HttpClient,
    raw_url: &str,
    allow_local_http: bool,
    accept: &'static str,
) -> Result<reqwest::Response, AttachmentError> {
    let mut url = reqwest::Url::parse(raw_url).map_err(|_| AttachmentError::InvalidUrl)?;
    if !valid_cdn_url(&url, allow_local_http) {
        return Err(AttachmentError::InvalidUrl);
    }
    // Fake mode: no avatar, emoji or attachment fetch ever leaves the machine.
    #[cfg(feature = "fake-discord")]
    if crate::fake::active() && url.host_str() != Some("127.0.0.1") {
        return Err(AttachmentError::Network);
    }
    for _ in 0..=MAX_REDIRECTS {
        let response = http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .map_err(|_| AttachmentError::Network)?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(AttachmentError::Network)?;
        let next = url
            .join(location)
            .map_err(|_| AttachmentError::InvalidUrl)?;
        if !valid_cdn_url(&next, allow_local_http) {
            return Err(AttachmentError::InvalidUrl);
        }
        #[cfg(feature = "fake-discord")]
        if crate::fake::active() && next.host_str() != Some("127.0.0.1") {
            return Err(AttachmentError::Network);
        }
        url = next;
    }
    Err(AttachmentError::Network)
}

fn valid_cdn_url(url: &reqwest::Url, allow_local_http: bool) -> bool {
    url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && (url.scheme() == "https"
            || (allow_local_http
                && url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))))
}

fn exceeds_pixel_limits(width: u32, height: u32) -> bool {
    width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
}

/// Decodes a cached image no larger than the display box. The header is read
/// and checked against the dimension, pixel, and allocation limits before the
/// decoder allocates anything for pixels. Animated formats show their first frame.
/// Where a public image's original bytes are.
enum Source {
    Memory(Vec<u8>),
    File(PathBuf),
}

fn decode_thumbnail(
    path: &Path,
    max_width: u32,
    max_height: u32,
) -> Result<DecodedImage, AttachmentError> {
    let file = fs::File::open(path).map_err(|_| AttachmentError::CacheUnavailable)?;
    decode_thumbnail_from(std::io::BufReader::new(file), max_width, max_height)
}

fn decode_thumbnail_from(
    source: impl std::io::BufRead + std::io::Seek,
    max_width: u32,
    max_height: u32,
) -> Result<DecodedImage, AttachmentError> {
    let mut reader = image::ImageReader::new(source)
        .with_guessed_format()
        .map_err(|_| AttachmentError::CacheUnavailable)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    if exceeds_pixel_limits(width, height) || decoder.total_bytes() > MAX_DECODE_ALLOC {
        return Err(AttachmentError::TooLarge);
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    image.apply_orientation(orientation);
    let (width, height) = (image.width(), image.height());
    let scale = (f64::from(max_width) / f64::from(width))
        .min(f64::from(max_height) / f64::from(height))
        .min(1.0);
    let target_width = ((f64::from(width) * scale).round() as u32).max(1);
    let target_height = ((f64::from(height) * scale).round() as u32).max(1);
    if (target_width, target_height) != (width, height) {
        image = image.resize_exact(target_width, target_height, FilterType::Triangle);
    }
    let rgba = image.into_rgba8();
    Ok(DecodedImage {
        width: target_width,
        height: target_height,
        rgba: rgba.into_raw().into(),
    })
}

fn decode_error(error: ImageError) -> AttachmentError {
    match error {
        ImageError::Limits(_) => AttachmentError::TooLarge,
        ImageError::Unsupported(_) => AttachmentError::UnsupportedImage,
        _ => AttachmentError::InvalidImage,
    }
}

fn emoji_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(256);
    limits.max_image_height = Some(256);
    limits.max_alloc = Some(8 * 1024 * 1024);
    limits
}

fn small_frame(frame: image::RgbaImage) -> DecodedImage {
    let scale = (EMOJI_SIDE as f32 / frame.width() as f32)
        .min(EMOJI_SIDE as f32 / frame.height() as f32)
        .min(1.0);
    let width = ((frame.width() as f32 * scale).round() as u32).max(1);
    let height = ((frame.height() as f32 * scale).round() as u32).max(1);
    let rgba = if (width, height) == frame.dimensions() {
        frame
    } else {
        image::imageops::resize(&frame, width, height, FilterType::Triangle)
    };
    DecodedImage {
        width,
        height,
        rgba: rgba.into_raw().into(),
    }
}

fn decode_small_animation(
    data: bytes::Bytes,
    active: &AtomicBool,
) -> Result<DecodedAnimation, AttachmentError> {
    use image::AnimationDecoder;
    if !active.load(Ordering::Acquire) {
        return Err(AttachmentError::WorkerStopped);
    }
    let format = image::guess_format(&data).map_err(decode_error)?;
    if format != image::ImageFormat::Gif {
        let mut reader = image::ImageReader::with_format(std::io::Cursor::new(data), format);
        reader.limits(emoji_limits());
        let image = reader.decode().map_err(decode_error)?.into_rgba8();
        return Ok(DecodedAnimation {
            frames: vec![small_frame(image)],
            delays: vec![Duration::from_secs(1)],
        });
    }
    let mut decoder =
        image::codecs::gif::GifDecoder::new(std::io::Cursor::new(data)).map_err(decode_error)?;
    decoder.set_limits(emoji_limits()).map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > 256 || height > 256 {
        return Err(AttachmentError::TooLarge);
    }
    let mut frames = Vec::new();
    let mut delays = Vec::new();
    let mut bytes = 0;
    for frame in decoder.into_frames() {
        if !active.load(Ordering::Acquire) {
            return Err(AttachmentError::WorkerStopped);
        }
        if frames.len() == MAX_EMOJI_FRAMES {
            return Err(AttachmentError::TooLarge);
        }
        let frame = frame.map_err(decode_error)?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay =
            Duration::from_micros(u64::from(numerator) * 1000 / u64::from(denominator.max(1)));
        // Browser semantics: GIF delays of 10 ms or less are treated as 100 ms.
        let delay = if delay <= Duration::from_millis(10) {
            Duration::from_millis(100)
        } else {
            delay.min(Duration::from_secs(10))
        };
        let frame = small_frame(frame.into_buffer());
        bytes += frame.rgba.len();
        if bytes > MAX_EMOJI_RGBA {
            return Err(AttachmentError::TooLarge);
        }
        frames.push(frame);
        delays.push(delay);
    }
    if frames.is_empty() {
        return Err(AttachmentError::InvalidImage);
    }
    Ok(DecodedAnimation { frames, delays })
}

#[derive(Default)]
struct DecodedLru {
    entries: HashMap<(AttachmentKey, u32, u32), DecodedImage>,
    order: VecDeque<(AttachmentKey, u32, u32)>,
    bytes: usize,
}

impl DecodedLru {
    fn get(&mut self, key: (AttachmentKey, u32, u32)) -> Option<DecodedImage> {
        let image = self.entries.get(&key)?.clone();
        self.order.retain(|entry| *entry != key);
        self.order.push_back(key);
        Some(image)
    }

    fn insert(&mut self, key: (AttachmentKey, u32, u32), image: DecodedImage) {
        let size = image.rgba.len();
        if size > DECODED_CACHE_LIMIT {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.rgba.len();
        }
        self.order.retain(|entry| *entry != key);
        while self.bytes.saturating_add(size) > DECODED_CACHE_LIMIT {
            let Some(old_key) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&old_key) {
                self.bytes -= old.rgba.len();
            }
        }
        self.bytes += size;
        self.order.push_back(key);
        self.entries.insert(key, image);
    }
}

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const TEMPORARY_EXTENSION: &str = "tmp";

/// A partial file owns its cleanup even if its async caller is aborted.
struct TemporaryFile(Option<PathBuf>);

impl TemporaryFile {
    fn keep(mut self) {
        self.0.take();
    }

    async fn remove(mut self) {
        if let Some(path) = self.0.take() {
            let _ = tokio::task::spawn_blocking(move || remove_temporary(&path)).await;
        }
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            tokio::task::spawn_blocking(move || remove_temporary(&path));
        }
    }
}

fn remove_temporary(path: &Path) {
    crate::disk_index::forget(path);
    // Windows can still be closing an aborted tokio file operation.
    for attempt in 0..5 {
        match fs::remove_file(path) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) if attempt < 4 => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
}

fn is_temporary(path: &Path) -> bool {
    crate::disk_index::is_temporary(path)
}

/// The `.part` file a download writes beside its destination. The name is
/// unique to this process and call, and its extension is not the cache's
/// temporary one, so it can never be mistaken for a cache partial.
fn partial_path(destination: &Path) -> Result<PathBuf, AttachmentError> {
    let parent = destination
        .parent()
        .ok_or(AttachmentError::DestinationUnavailable)?;
    let pid = std::process::id();
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".fastcord-download-{pid}-{sequence}.part")))
}

fn touch(path: &Path) -> std::io::Result<()> {
    crate::disk_index::touch(path);
    filetime::set_file_mtime(path, filetime::FileTime::now())
}

fn safe_extension(filename: &str) -> String {
    Path::new(filename)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 12
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "bin".to_owned())
}

fn cache_base() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Caches"))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
    }
}

#[cfg(test)]
struct DecodeGate {
    entered: std::sync::atomic::AtomicUsize,
    started: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::BoxFuture;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ACCOUNT: Snowflake = Snowflake(1);

    fn key(attachment: u64) -> AttachmentKey {
        AttachmentKey {
            account: ACCOUNT,
            channel: Snowflake(2),
            message: Snowflake(3),
            attachment: Snowflake(attachment),
        }
    }

    fn attachment(filename: &str, url: &str) -> Attachment {
        Attachment {
            id: Snowflake(4),
            filename: filename.to_owned(),
            size: 0,
            url: url.to_owned(),
            proxy_url: None,
            content_type: Some("image/png".to_owned()),
            width: None,
            height: None,
            duration_secs: None,
            waveform: None,
        }
    }

    fn scratch(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fastcord-attachment-{label}-{}-{}",
            std::process::id(),
            TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ))
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    /// A PNG whose header claims `width` x `height` and nothing else: any
    /// decoder that allocated for the claimed size before checking it would
    /// need gigabytes, and one that read on would find no pixel data.
    fn png_header_only(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let mut header = b"IHDR".to_vec();
        header.extend(width.to_be_bytes());
        header.extend(height.to_be_bytes());
        header.extend([8, 6, 0, 0, 0]);
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(13u32.to_be_bytes());
        bytes.extend(&header);
        bytes.extend(crc32(&header).to_be_bytes());
        bytes
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend(body);
        response
    }

    fn status(code: u16) -> Vec<u8> {
        format!("HTTP/1.1 {code} Status\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    fn redirect(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    /// Every request head the server saw, lowercased, oldest first.
    type Seen = Arc<Mutex<Vec<String>>>;
    type Handler = Arc<dyn Fn(String) -> BoxFuture<'static, Vec<u8>> + Send + Sync>;

    async fn serve(handler: Handler) -> (std::net::SocketAddr, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen: Seen = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend(&chunk[..read]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    log.lock().push(head.clone());
                    let response = handler(head).await;
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (address, seen)
    }

    fn respond(f: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Handler {
        Arc::new(move |head| {
            let response = f(&head);
            Box::pin(async move { response })
        })
    }

    /// A refresher that never expects to be called.
    struct NoRefresh;
    impl UrlRefresh for NoRefresh {
        async fn refresh(&self, _url: String) -> Result<String, AttachmentError> {
            panic!("no refresh expected");
        }
    }

    /// Records calls and answers with a fixed replacement URL.
    struct Refresher {
        calls: AtomicUsize,
        fresh: Option<String>,
    }
    impl Refresher {
        fn new(fresh: Option<String>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fresh,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl UrlRefresh for Refresher {
        async fn refresh(&self, _url: String) -> Result<String, AttachmentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.fresh.clone().ok_or(AttachmentError::RefreshFailed)
        }
    }

    fn files_in(root: &Path, account: Snowflake) -> Vec<PathBuf> {
        fs::read_dir(root.join(account.0.to_string()))
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    }

    /// Names of the files directly inside `dir`, sorted. Empty if it is missing.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Every cached file for the test account under `root`, with its length.
    fn cache_snapshot(root: &Path) -> Vec<(PathBuf, u64)> {
        let mut files: Vec<(PathBuf, u64)> = files_in(root, ACCOUNT)
            .into_iter()
            .map(|path| {
                let length = fs::metadata(&path).unwrap().len();
                (path, length)
            })
            .collect();
        files.sort();
        files
    }

    /// Polls until `done` holds, failing the test after five seconds.
    async fn wait_until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition not reached in time");
    }

    /// Reads the request head the test server received, through its blank line.
    async fn read_request_head(socket: &mut tokio::net::TcpStream) {
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "the client closed before sending its request");
            head.extend_from_slice(&chunk[..read]);
        }
    }

    #[test]
    fn the_logical_key_names_the_account_message_and_attachment_but_never_a_url() {
        let a = key(4);
        assert_eq!(a.disk_name(), key(4).disk_name());
        for other in [
            key(5),
            AttachmentKey {
                message: Snowflake(9),
                ..a
            },
            AttachmentKey {
                channel: Snowflake(9),
                ..a
            },
            AttachmentKey {
                account: Snowflake(9),
                ..a
            },
        ] {
            assert_ne!(other.disk_name(), a.disk_name());
        }
        assert_eq!(a.disk_name().len(), 64);
        assert!(a.file_name("../../a.PNG").ends_with(".png"));
        assert!(!a.file_name("../../a.PNG").contains(['/', '\\']));
    }

    #[tokio::test]
    async fn the_cdn_request_keeps_the_signed_query_and_carries_no_credentials_across_redirects() {
        let png = png(8, 4);
        let (cdn, cdn_seen) = serve(respond({
            let png = png.clone();
            move |_| ok(&png)
        }))
        .await;
        let (front, front_seen) = serve(respond(move |_| {
            redirect(&format!(
                "http://{cdn}/target?ex=1&is=2&hm=abc&redirected=yes"
            ))
        }))
        .await;
        let root = scratch("auth");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let path = cache
            .fetch_file(
                &NoRefresh,
                key(4),
                "a.png",
                &format!("http://{front}/attachments/1/2/a.png?ex=65d903de&is=65c68ede&hm=2481f3&"),
                MAX_FILE_BYTES,
            )
            .await
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), png);
        let front_seen = front_seen.lock().clone();
        let cdn_seen = cdn_seen.lock().clone();
        assert!(
            front_seen[0].starts_with(
                "get /attachments/1/2/a.png?ex=65d903de&is=65c68ede&hm=2481f3& http/1.1"
            ),
            "{}",
            front_seen[0]
        );
        assert!(
            cdn_seen[0].starts_with("get /target?ex=1&is=2&hm=abc&redirected=yes http/1.1"),
            "{}",
            cdn_seen[0]
        );
        for head in front_seen.iter().chain(&cdn_seen) {
            for forbidden in ["authorization:", "cookie:", "proxy-authorization:"] {
                assert!(!head.contains(forbidden), "{forbidden} sent: {head}");
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn redirects_to_credentials_other_schemes_or_endless_chains_are_refused() {
        let root = scratch("redirect");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        for location in [
            "http://user:pass@127.0.0.1:1/x".to_owned(),
            "ftp://127.0.0.1/x".to_owned(),
            "http://example.invalid/x".to_owned(),
        ] {
            let (address, _) = serve(respond(move |_| redirect(&location))).await;
            let result = cache
                .fetch_file(
                    &NoRefresh,
                    key(4),
                    "a.bin",
                    &format!("http://{address}/a"),
                    1024,
                )
                .await;
            assert_eq!(result, Err(AttachmentError::InvalidUrl));
        }
        // A chain that never ends stops after a fixed number of hops.
        let (address, seen) = serve(respond(move |_| redirect("/again"))).await;
        let result = cache
            .fetch_file(
                &NoRefresh,
                key(4),
                "a.bin",
                &format!("http://{address}/a"),
                1024,
            )
            .await;
        assert_eq!(result, Err(AttachmentError::Network));
        assert_eq!(seen.lock().len(), MAX_REDIRECTS + 1);
        assert!(
            files_in(&root, ACCOUNT).is_empty(),
            "nothing partial is left"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn plain_http_outside_the_loopback_test_harness_is_rejected() {
        let root = scratch("https-only");
        let cache = AttachmentCache::new(
            &fastcord_net::NetworkContext::off(),
            ACCOUNT,
            root.clone(),
            TEST_USER_AGENT,
        )
        .unwrap();
        for url in [
            "http://cdn.discordapp.com/a.png",
            "https://user:secret@cdn.discordapp.com/a.png",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert_eq!(
                cache
                    .fetch_file(&NoRefresh, key(4), "a.png", url, 1024)
                    .await,
                Err(AttachmentError::InvalidUrl),
                "{url}"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_cached_attachment_is_served_without_any_request_whatever_url_it_is_given() {
        let png = png(4, 4);
        let (address, seen) = serve(respond({
            let png = png.clone();
            move |_| ok(&png)
        }))
        .await;
        let root = scratch("logical");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let first = cache
            .fetch_file(
                &NoRefresh,
                key(4),
                "a.png",
                &format!("http://{address}/a?sig=old"),
                1 << 20,
            )
            .await
            .unwrap();
        // A new signature (or an unreachable host) for the same attachment is
        // the same cache entry.
        let second = cache
            .fetch_file(
                &NoRefresh,
                key(4),
                "a.png",
                "http://127.0.0.1:1/a?sig=new",
                1 << 20,
            )
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(seen.lock().len(), 1);
        assert_eq!(files_in(&root, ACCOUNT).len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_expired_url_is_refreshed_once_through_the_api_and_retried_once() {
        for expired in [403, 404] {
            let (address, seen) = serve(respond(move |head| {
                if head.contains("sig=fresh") {
                    ok(b"fresh bytes")
                } else {
                    status(expired)
                }
            }))
            .await;
            let root = scratch("refresh");
            let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
            let refresher = Refresher::new(Some(format!("http://{address}/a?sig=fresh")));
            let path = cache
                .fetch_file(
                    &refresher,
                    key(4),
                    "a.bin",
                    &format!("http://{address}/a?sig=old"),
                    1024,
                )
                .await
                .unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"fresh bytes");
            assert_eq!(refresher.calls(), 1, "{expired}");
            let seen = seen.lock().clone();
            assert_eq!(seen.len(), 2);
            assert!(seen[0].contains("sig=old") && seen[1].contains("sig=fresh"));
            // The refreshed bytes are now cached: no further request or refresh.
            cache
                .fetch_file(
                    &refresher,
                    key(4),
                    "a.bin",
                    &format!("http://{address}/a?sig=old"),
                    1024,
                )
                .await
                .unwrap();
            assert_eq!((refresher.calls(), seen.len()), (1, 2));
            let _ = fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn refresh_is_not_repeated_and_other_failures_do_not_trigger_it() {
        let root = scratch("refresh-limits");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        // The refreshed URL is rejected as well: one refresh, one retry, then the error.
        let (address, seen) = serve(respond(|_| status(403))).await;
        let refresher = Refresher::new(Some(format!("http://{address}/a?sig=fresh")));
        let result = cache
            .fetch_file(
                &refresher,
                key(4),
                "a.bin",
                &format!("http://{address}/a"),
                1024,
            )
            .await;
        assert_eq!(result, Err(AttachmentError::Http(403)));
        assert_eq!((refresher.calls(), seen.lock().len()), (1, 2));
        // The API cannot refresh: a distinct error, and no retry request.
        let (address, seen) = serve(respond(|_| status(404))).await;
        let refresher = Refresher::new(None);
        let result = cache
            .fetch_file(
                &refresher,
                key(5),
                "a.bin",
                &format!("http://{address}/a"),
                1024,
            )
            .await;
        assert_eq!(result, Err(AttachmentError::RefreshFailed));
        assert_eq!((refresher.calls(), seen.lock().len()), (1, 1));
        // A server error is not an expired signature.
        let (address, _) = serve(respond(|_| status(500))).await;
        let refresher = Refresher::new(Some("unused".to_owned()));
        let result = cache
            .fetch_file(
                &refresher,
                key(6),
                "a.bin",
                &format!("http://{address}/a"),
                1024,
            )
            .await;
        assert_eq!(result, Err(AttachmentError::Http(500)));
        assert_eq!(refresher.calls(), 0);
        assert!(files_in(&root, ACCOUNT).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn downloads_over_the_limit_are_refused_and_leave_nothing_behind() {
        let root = scratch("limit");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        // Declared length: refused before the body is read.
        let (address, _) = serve(respond(|_| ok(&[7u8; 2048]))).await;
        let result = cache
            .fetch_file(
                &NoRefresh,
                key(4),
                "a.bin",
                &format!("http://{address}/a"),
                1024,
            )
            .await;
        assert_eq!(result, Err(AttachmentError::TooLarge));
        // No declared length: counted as it streams.
        let (address, _) = serve(respond(|_| {
            let mut response =
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                    .to_vec();
            for _ in 0..4 {
                response.extend(b"200\r\n");
                response.extend([7u8; 512]);
                response.extend(b"\r\n");
            }
            response.extend(b"0\r\n\r\n");
            response
        }))
        .await;
        let result = cache
            .fetch_file(
                &NoRefresh,
                key(5),
                "a.bin",
                &format!("http://{address}/a"),
                1024,
            )
            .await;
        assert_eq!(result, Err(AttachmentError::TooLarge));
        assert!(files_in(&root, ACCOUNT).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn no_more_than_four_fetches_run_at_once() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let handler: Handler = {
            let (in_flight, peak) = (in_flight.clone(), peak.clone());
            Arc::new(move |_| {
                let (in_flight, peak) = (in_flight.clone(), peak.clone());
                Box::pin(async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    ok(b"x")
                })
            })
        };
        let (address, _) = serve(handler).await;
        let root = scratch("slots");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let fetches: Vec<_> = (10..22)
            .map(|id| {
                let cache = cache.clone();
                let url = format!("http://{address}/{id}");
                tokio::spawn(async move {
                    cache
                        .fetch_file(&NoRefresh, key(id), "a.bin", &url, 1024)
                        .await
                })
            })
            .collect();
        for fetch in fetches {
            fetch.await.unwrap().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), FETCH_SLOTS);
        assert_eq!(cache.decode_slots.available_permits(), DECODE_SLOTS);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_avatar_is_fetched_without_credentials_bounded_cached_and_distinct_per_hash() {
        let (address, seen) = serve(respond({
            let png = png(256, 256);
            move |_| ok(&png)
        }))
        .await;
        let root = scratch("avatar");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let url = format!("http://{address}/avatars/9/h1.png?size=64");
        let first = cache.avatar(2, Snowflake(9), "h1", &url).await.unwrap();
        assert_eq!((first.width, first.height), (AVATAR_SIDE, AVATAR_SIDE));
        cache.avatar(2, Snowflake(9), "h1", &url).await.unwrap();
        assert_eq!(seen.lock().len(), 1, "the second read is a disk hit");
        {
            let head = &seen.lock()[0];
            assert!(head.starts_with("get /avatars/9/h1.png?size=64 "));
            assert!(
                !head.contains("authorization")
                    && !head.contains("cookie")
                    && !head.contains("x-super-properties")
                    && !head.contains("x-installation-id")
                    && !head.contains("x-fingerprint")
                    && !head.contains("x-discord-locale")
                    && !head.contains("sec-ch-ua"),
                "a credential or identity header was sent to the CDN"
            );
            // An image subresource carries the public user agent and Chromium's
            // image `Accept`, and nothing else that identifies the account.
            assert!(
                head.contains(&format!("user-agent: {}", TEST_USER_AGENT.to_lowercase())),
                "the public user agent was not sent"
            );
            assert!(
                head.contains(&format!("accept: {}", IMAGE_ACCEPT.to_lowercase())),
                "an image fetch did not send the image Accept: {head}"
            );
        }
        // A changed hash is a different cache entry; so is a different kind.
        cache.avatar(2, Snowflake(9), "h2", &url).await.unwrap();
        cache.avatar(3, Snowflake(9), "h1", &url).await.unwrap();
        assert_eq!(seen.lock().len(), 3);
        // An unavailable avatar fails, leaving the caller's fallback.
        let (address, _) = serve(respond(|_| status(404))).await;
        let missing = format!("http://{address}/avatars/9/gone.png");
        assert_eq!(
            cache
                .avatar(2, Snowflake(9), "gone", &missing)
                .await
                .unwrap_err(),
            AttachmentError::Http(404)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_restarted_cache_shows_avatars_without_the_network_and_purge_forgets_them() {
        let (address, seen) = serve(respond({
            let png = png(256, 256);
            move |_| ok(&png)
        }))
        .await;
        let root = scratch("avatar-restart");
        let url = format!("http://{address}/avatars/9/h1.png?size=64");
        let first = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        first.avatar(2, Snowflake(9), "h1", &url).await.unwrap();
        assert_eq!(seen.lock().len(), 1);
        assert!(root.join("images").join("1").is_dir());
        drop(first);
        // The compressed original was never stored (#168): only the sized
        // thumbnail can answer a new instance whose URL is unreachable.
        assert!(!root.join("1").exists());
        let second = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let hit = second
            .avatar(2, Snowflake(9), "h1", "http://127.0.0.1:1/never")
            .await
            .unwrap();
        assert_eq!((hit.width, hit.height), (AVATAR_SIDE, AVATAR_SIDE));
        assert_eq!(seen.lock().len(), 1);
        // A new hash is a different entry and does need the network.
        assert!(
            second
                .avatar(2, Snowflake(9), "h2", "http://127.0.0.1:1/never")
                .await
                .is_err()
        );
        second.purge().await.unwrap();
        assert!(!root.join("images").join("1").exists());
        let third = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        assert!(
            third
                .avatar(2, Snowflake(9), "h1", "http://127.0.0.1:1/never")
                .await
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_embed_picture_is_fetched_without_credentials_capped_and_cached() {
        let (address, seen) = serve(respond({
            let png = png(800, 400);
            move |_| ok(&png)
        }))
        .await;
        let root = scratch("embed");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let url = format!("http://{address}/external/x/a.png?ex=1&hm=2");
        let first = cache.embed_image("h", &url, (400, 300)).await.unwrap();
        assert_eq!((first.width, first.height), (400, 200), "capped to the box");
        cache.embed_image("h", &url, (400, 300)).await.unwrap();
        assert_eq!(seen.lock().len(), 1, "the second read is a disk hit");
        {
            let head = &seen.lock()[0];
            assert!(!head.contains("authorization") && !head.contains("cookie"));
        }
        let (address, _) = serve(respond(|_| status(404))).await;
        assert_eq!(
            cache
                .embed_image("gone", &format!("http://{address}/x.png"), (80, 80))
                .await
                .unwrap_err(),
            AttachmentError::Http(404)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_thumbnail_is_fetched_decoded_scaled_cached_and_never_refetched() {
        let (address, seen) = serve(respond({
            let png = png(80, 40);
            move |_| ok(&png)
        }))
        .await;
        let root = scratch("thumb");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let item = attachment("a.png", &format!("http://{address}/a.png?sig=1"));
        let first = cache
            .thumbnail(&NoRefresh, key(4), &item, 16, 16)
            .await
            .unwrap();
        assert_eq!((first.width, first.height), (16, 8));
        assert_eq!(first.rgba.len(), 16 * 8 * 4);
        let again = cache
            .thumbnail(&NoRefresh, key(4), &item, 16, 16)
            .await
            .unwrap();
        assert_eq!(
            first.rgba.as_ptr(),
            again.rgba.as_ptr(),
            "a decoded cache hit shares pixels"
        );
        // Another size decodes again from the cached file, still without a request.
        let larger = cache
            .thumbnail(&NoRefresh, key(4), &item, 40, 40)
            .await
            .unwrap();
        assert_eq!((larger.width, larger.height), (40, 20));
        assert_eq!(seen.lock().len(), 1);
        // A small image is never enlarged.
        let small = cache
            .thumbnail(&NoRefresh, key(4), &item, 2000, 2000)
            .await
            .unwrap();
        assert_eq!((small.width, small.height), (80, 40));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dimension_and_pixel_bombs_are_rejected_from_the_header_before_any_pixel_allocation() {
        let root = scratch("bomb");
        fs::create_dir_all(&root).unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = root.join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        // Headers claiming gigapixels with no pixel data at all: rejected as too
        // large (not "invalid image"), which proves nothing tried to decode them.
        for (width, height) in [
            (60_000, 60_000),
            (MAX_IMAGE_DIMENSION + 1, 1),
            (1, MAX_IMAGE_DIMENSION + 1),
            (u32::MAX >> 1, 1),
        ] {
            let path = write("bomb.png", &png_header_only(width, height));
            assert_eq!(
                decode_thumbnail(&path, 320, 240).unwrap_err(),
                AttachmentError::TooLarge,
                "{width}x{height}"
            );
        }
        // A real image just over the pixel cap (within the side limit) is refused
        // from its header too: gray PNGs of zeros are tiny on disk but 16.8 MP.
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(image::GrayImage::new(4097, 4097))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let path = write("pixels.png", bytes.get_ref());
        assert!(fs::metadata(&path).unwrap().len() < 1024 * 1024);
        assert_eq!(
            decode_thumbnail(&path, 320, 240).unwrap_err(),
            AttachmentError::TooLarge
        );
        // Within the limits it is the missing pixel data that fails.
        let path = write("truncated.png", &png_header_only(64, 64));
        assert_eq!(
            decode_thumbnail(&path, 320, 240).unwrap_err(),
            AttachmentError::InvalidImage
        );
        // Not an image at all.
        let path = write("text.png", b"just some text, definitely not an image");
        assert!(matches!(
            decode_thumbnail(&path, 320, 240).unwrap_err(),
            AttachmentError::UnsupportedImage | AttachmentError::InvalidImage
        ));
        // The largest allowed image still decodes, scaled to the display box.
        let path = write("wide.png", &png(MAX_IMAGE_DIMENSION, 2));
        let decoded = decode_thumbnail(&path, 320, 240).unwrap();
        assert_eq!((decoded.width, decoded.height), (320, 1));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn declared_oversize_images_are_refused_without_any_request() {
        let (address, seen) = serve(respond(|_| ok(b"unused"))).await;
        let root = scratch("declared");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let mut item = attachment("a.png", &format!("http://{address}/a.png"));
        item.size = MAX_IMAGE_FILE_BYTES + 1;
        assert_eq!(
            cache
                .thumbnail(&NoRefresh, key(4), &item, 320, 240)
                .await
                .unwrap_err(),
            AttachmentError::TooLarge
        );
        item.size = 1;
        (item.width, item.height) = (Some(60_000), Some(60_000));
        assert_eq!(
            cache
                .thumbnail(&NoRefresh, key(4), &item, 320, 240)
                .await
                .unwrap_err(),
            AttachmentError::TooLarge
        );
        assert!(seen.lock().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_real_image_that_lies_about_its_size_is_still_caught_by_its_header() {
        let (address, _) = serve(respond(|_| ok(&png_header_only(60_000, 60_000)))).await;
        let root = scratch("lie");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let mut item = attachment("a.png", &format!("http://{address}/a.png"));
        (item.width, item.height) = (Some(10), Some(10));
        assert_eq!(
            cache
                .thumbnail(&NoRefresh, key(4), &item, 320, 240)
                .await
                .unwrap_err(),
            AttachmentError::TooLarge
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn decoded_lru_evicts_oldest_entries_at_the_byte_limit_and_refreshes_on_use() {
        let image = DecodedImage {
            width: 512,
            height: 512,
            rgba: bytes::Bytes::from(vec![0; 1024 * 1024]),
        };
        let keys: Vec<_> = (1..=14).map(key).collect();
        let mut cache = DecodedLru::default();
        for (index, item) in keys.iter().take(12).enumerate() {
            cache.insert((*item, 512, 512), image.clone());
            if index == 5 {
                // Use the oldest entry so the next-oldest goes first.
                assert!(cache.get((keys[0], 512, 512)).is_some());
            }
        }
        assert_eq!(cache.bytes, DECODED_CACHE_LIMIT);
        cache.insert((keys[12], 512, 512), image.clone());
        cache.insert((keys[13], 512, 512), image.clone());
        assert_eq!(cache.bytes, DECODED_CACHE_LIMIT);
        assert_eq!(cache.entries.len(), 12);
        assert!(cache.entries.contains_key(&(keys[0], 512, 512)));
        assert!(!cache.entries.contains_key(&(keys[1], 512, 512)));
        assert!(!cache.entries.contains_key(&(keys[2], 512, 512)));
        // An image larger than the whole budget is never kept.
        let huge = DecodedImage {
            width: 2048,
            height: 2048,
            rgba: bytes::Bytes::from(vec![0; DECODED_CACHE_LIMIT + 4]),
        };
        cache.insert((keys[0], 2048, 2048), huge);
        assert!(cache.bytes <= DECODED_CACHE_LIMIT);
        assert!(!cache.entries.contains_key(&(keys[0], 2048, 2048)));
        // And a same-key re-insert replaces rather than double counts.
        cache.insert((keys[13], 512, 512), image);
        assert_eq!(cache.bytes, DECODED_CACHE_LIMIT);
    }

    #[tokio::test]
    async fn a_miss_does_not_rescan_the_disk_and_the_budget_holds_after_a_restart() {
        let root = scratch("ledger");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let account_dir = root.join(ACCOUNT.0.to_string());
        fs::create_dir_all(&account_dir).unwrap();
        let old = account_dir.join("old.bin");
        fs::write(&old, vec![0u8; 10_000]).unwrap();
        filetime::set_file_mtime(&old, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let (first, cleanup) = cache
            .reserve(&cache.temporary_path("a"), 10_000)
            .await
            .unwrap();
        // The first use scanned: the old file and the partial are counted.
        assert_eq!(crate::disk_index::total(&root), 20_000);
        // A file the ledger never saw proves the next reservation does not walk
        // the directory (#168).
        fs::write(account_dir.join("stranger.bin"), vec![0u8; 50_000]).unwrap();
        let (second, cleanup_b) = cache
            .reserve(&cache.temporary_path("b"), 10_000)
            .await
            .unwrap();
        assert_eq!(crate::disk_index::total(&root), 30_000);
        drop((first, second));
        cleanup.remove().await;
        cleanup_b.remove().await;
        assert_eq!(crate::disk_index::total(&root), 10_000);
        // Over the limit: the oldest ledger entry goes, the newest stays.
        let victim = crate::disk_index::record(&account_dir.join("new.bin"), 10_000, None, 15_000);
        assert_eq!(victim, (vec![old], true));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn the_disk_cache_stays_within_its_limit_by_evicting_least_recently_used_files() {
        // One real 64 MiB cap would be slow to fill; the accounting is the same
        // function with a smaller limit, so exercise the full fetch path against
        // the real limit using sparse accounting: files are charged by length.
        let root = scratch("disk-bound");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let account_dir = root.join(ACCOUNT.0.to_string());
        fs::create_dir_all(&account_dir).unwrap();
        // Pre-fill 255 MiB of old entries (sparse files keep this cheap).
        for index in 0..255u64 {
            let path = account_dir.join(format!("old{index}.bin"));
            let file = fs::File::create(&path).unwrap();
            file.set_len(1024 * 1024).unwrap();
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(10 + index as i64, 0),
            )
            .unwrap();
        }
        let (address, _) = serve(respond(|_| ok(&vec![1u8; 3 * 1024 * 1024]))).await;
        let path = cache
            .fetch_file(
                &NoRefresh,
                key(4),
                "a.bin",
                &format!("http://{address}/a"),
                MAX_FILE_BYTES,
            )
            .await
            .unwrap();
        let total: u64 = files_in(&root, ACCOUNT)
            .iter()
            .map(|file| {
                fs::metadata(file)
                    .unwrap()
                    .len()
                    .max(crate::disk_index::MIN_CHARGE)
            })
            .sum();
        assert!(total <= DISK_CACHE_LIMIT, "{total}");
        assert!(path.exists());
        // The oldest entries went first.
        assert!(!account_dir.join("old0.bin").exists());
        assert!(account_dir.join("old254.bin").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_partial_reservations_account_for_the_full_disk_budget() {
        let root = scratch("reservations");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let reservations = futures_util::future::join_all((0..FETCH_SLOTS).map(|index| {
            let cache = cache.clone();
            async move {
                cache
                    .reserve(&cache.temporary_path(&index.to_string()), MAX_FILE_BYTES)
                    .await
                    .unwrap()
            }
        }))
        .await;
        let total: u64 = files_in(&root, ACCOUNT)
            .iter()
            .map(|path| fs::metadata(path).unwrap().len())
            .sum();
        assert_eq!(total, DISK_CACHE_LIMIT);
        assert!(matches!(
            cache.reserve(&cache.temporary_path("overflow"), 1).await,
            Err(AttachmentError::CacheUnavailable)
        ));
        for (file, cleanup) in reservations {
            drop(file);
            cleanup.remove().await;
        }
        assert!(files_in(&root, ACCOUNT).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn aborting_a_stream_removes_its_reserved_partial_file() {
        let root = scratch("abort-stream");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                head.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nx")
                .await
                .unwrap();
            sent.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let worker = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .local_file(&NoRefresh, key(4), "a.txt", &format!("http://{address}/a"))
                    .await
            }
        });
        received.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while files_in(&root, ACCOUNT).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !files_in(&root, ACCOUNT).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(cache.fetch_slots.available_permits(), FETCH_SLOTS);
        server.abort();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelling_decode_callers_does_not_release_still_running_worker_slots() {
        let root = scratch("decode-abort");
        let mut cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let gate = Arc::new(DecodeGate {
            entered: std::sync::atomic::AtomicUsize::new(0),
            started: std::sync::Barrier::new(DECODE_SLOTS + 1),
            release: std::sync::Barrier::new(DECODE_SLOTS + 1),
        });
        cache.decode_gate = Some(gate.clone());
        let (address, _) = serve(respond({
            let bytes = png(8, 8);
            move |_| ok(&bytes)
        }))
        .await;
        let spawn = |id| {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .thumbnail(
                        &NoRefresh,
                        key(id),
                        &attachment("a.png", &format!("http://{address}/a")),
                        8,
                        8,
                    )
                    .await
            })
        };
        let first = spawn(10);
        let second = spawn(11);
        let started = gate.clone();
        tokio::task::spawn_blocking(move || started.started.wait())
            .await
            .unwrap();
        first.abort();
        second.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(second.await.unwrap_err().is_cancelled());
        assert_eq!(cache.decode_slots.available_permits(), 0);
        let third = spawn(12);
        assert_eq!(gate.entered.load(Ordering::SeqCst), DECODE_SLOTS);
        let release = gate.clone();
        tokio::task::spawn_blocking(move || release.release.wait())
            .await
            .unwrap();
        assert_eq!(third.await.unwrap().unwrap().rgba.len(), 8 * 8 * 4);
        assert_eq!(gate.entered.load(Ordering::SeqCst), DECODE_SLOTS + 1);
        cache.purge().await.unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn local_files_get_a_safe_extension_and_purge_removes_them_and_stops_work() {
        let (address, seen) = serve(respond(|_| ok(b"fixture bytes"))).await;
        let root = scratch("purge");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let path = cache
            .local_file(
                &NoRefresh,
                key(4),
                "../../media.MP4",
                &format!("http://{address}/m"),
            )
            .await
            .unwrap();
        assert_eq!(
            path.extension().and_then(std::ffi::OsStr::to_str),
            Some("mp4")
        );
        assert!(path.starts_with(root.join("1")));
        assert_eq!(fs::read(&path).unwrap(), b"fixture bytes");
        cache.purge().await.unwrap();
        assert!(!path.exists() && !root.join("1").exists());
        // The purged cache can no longer fetch or write anything.
        let result = cache
            .local_file(&NoRefresh, key(5), "x.bin", &format!("http://{address}/x"))
            .await;
        assert_eq!(result, Err(AttachmentError::WorkerStopped));
        assert_eq!(seen.lock().len(), 1);
        assert!(!root.join("1").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn another_accounts_key_is_refused_and_files_are_account_scoped() {
        let root = scratch("scope");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let foreign = AttachmentKey {
            account: Snowflake(77),
            ..key(4)
        };
        assert_eq!(
            cache
                .fetch_file(&NoRefresh, foreign, "a.bin", "http://127.0.0.1:1/", 10)
                .await,
            Err(AttachmentError::InvalidUrl)
        );
        assert_eq!(cache.root.as_path(), root.join("1"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_partial_downloads_are_removed_when_the_cache_is_first_used() {
        let root = scratch("stale");
        let account_dir = root.join("1");
        fs::create_dir_all(&account_dir).unwrap();
        let partial = account_dir.join("abc.bin-1-1.tmp");
        let kept = account_dir.join("abc.bin");
        fs::write(&partial, b"half").unwrap();
        fs::write(&kept, b"whole").unwrap();
        let (address, _) = serve(respond(|_| ok(b"x"))).await;
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        cache
            .fetch_file(
                &NoRefresh,
                key(4),
                "a.bin",
                &format!("http://{address}/a"),
                10,
            )
            .await
            .unwrap();
        assert!(!partial.exists());
        assert!(kept.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_extensions_cannot_escape_cache_paths() {
        assert_eq!(safe_extension("../../movie.MP4"), "mp4");
        assert_eq!(safe_extension("no-extension"), "bin");
        assert_eq!(safe_extension("unsafe.exe;arg"), "bin");
        assert_eq!(safe_extension("a.averyveryverylongextension"), "bin");
    }

    fn gif(frames: usize) -> bytes::Bytes {
        let mut data = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut data);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();
            for frame in 0..frames {
                let color = if frame % 2 == 0 {
                    [255, 0, 0, 255]
                } else {
                    [0, 255, 0, 255]
                };
                encoder
                    .encode_frame(image::Frame::from_parts(
                        image::RgbaImage::from_pixel(32, 32, image::Rgba(color)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(if frame % 2 == 0 { 0 } else { 70 }, 1),
                    ))
                    .unwrap();
            }
        }
        data.into()
    }

    #[tokio::test]
    async fn emoji_png_and_gif_decoding_preserves_frames_delays_and_bounds() {
        let root = scratch("emoji-decode");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let png = cache.decode_emoji(png(96, 24).into()).await.unwrap();
        assert_eq!((png.frames[0].width, png.frames[0].height), (32, 8));
        assert_eq!(png.frames.len(), 1);
        let animation = cache.decode_emoji(gif(2)).await.unwrap();
        assert_eq!(
            animation.delays,
            [Duration::from_millis(100), Duration::from_millis(70)]
        );
        assert_eq!(animation.frames.len(), 2);
        assert_eq!(&animation.frames[0].rgba[..4], [255, 0, 0, 255]);
        assert_eq!(&animation.frames[1].rgba[..4], [0, 255, 0, 255]);
        let shared = animation.clone();
        assert_eq!(
            shared.frames[0].rgba.as_ptr(),
            animation.frames[0].rgba.as_ptr()
        );
        assert!(matches!(
            cache.decode_emoji(gif(MAX_EMOJI_FRAMES + 1)).await,
            Err(AttachmentError::TooLarge)
        ));
        assert!(matches!(
            cache.decode_emoji(png_header_only(257, 32).into()).await,
            Err(AttachmentError::TooLarge)
        ));
        assert!(matches!(
            cache
                .decode_emoji(vec![0; MAX_EMOJI_FILE_BYTES as usize + 1].into())
                .await,
            Err(AttachmentError::TooLarge)
        ));
        assert!(
            cache
                .decode_emoji(bytes::Bytes::from_static(b"not an image"))
                .await
                .is_err()
        );
        cache.purge().await.unwrap();
        assert!(matches!(
            cache.decode_emoji(gif(2)).await,
            Err(AttachmentError::WorkerStopped)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_emoji_decodes_keep_the_shared_decode_slots_until_the_workers_finish() {
        let root = scratch("emoji-abort");
        let mut cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let gate = Arc::new(DecodeGate {
            entered: AtomicUsize::new(0),
            started: std::sync::Barrier::new(DECODE_SLOTS + 1),
            release: std::sync::Barrier::new(DECODE_SLOTS + 1),
        });
        cache.decode_gate = Some(gate.clone());
        let spawn = || {
            let cache = cache.clone();
            tokio::spawn(async move { cache.decode_emoji(gif(2)).await })
        };
        let first = spawn();
        let second = spawn();
        let started = gate.clone();
        tokio::task::spawn_blocking(move || started.started.wait())
            .await
            .unwrap();
        first.abort();
        second.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(second.await.unwrap_err().is_cancelled());
        assert_eq!(cache.decode_slots.available_permits(), 0);
        let third = spawn();
        let release = gate.clone();
        tokio::task::spawn_blocking(move || release.release.wait())
            .await
            .unwrap();
        assert_eq!(third.await.unwrap().unwrap().frames.len(), 2);
        cache.purge().await.unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn large_downloads_are_saved_exactly_and_leave_the_cache_alone() {
        let body = Arc::new(
            (0..MAX_FILE_BYTES as usize + 1024 * 1024)
                .map(|i| (i % 251) as u8)
                .collect::<Vec<u8>>(),
        );
        let small = b"small image".to_vec();
        let (address, seen) = serve(respond({
            let body = body.clone();
            move |head: &str| {
                if head.starts_with("get /small.png ") {
                    ok(&small)
                } else {
                    ok(&body)
                }
            }
        }))
        .await;
        let root = scratch("download-large");
        let dir = scratch("download-large-dest");
        fs::create_dir_all(&dir).unwrap();
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        cache
            .fetch_file(
                &NoRefresh,
                key(5),
                "small.png",
                &format!("http://{address}/small.png"),
                MAX_FILE_BYTES,
            )
            .await
            .unwrap();
        let cached = cache_snapshot(&root);
        let destination = dir.join("big.bin");
        fs::write(&destination, b"previous").unwrap();
        cache
            .download_to(
                &NoRefresh,
                key(6),
                &format!("http://{address}/big.bin"),
                &destination,
            )
            .await
            .unwrap();
        assert!(
            fs::read(&destination).unwrap() == *body,
            "the saved file is not the served body"
        );
        assert_eq!(cache_snapshot(&root), cached);
        assert_eq!(names_in(&dir), ["big.bin"]);
        assert_eq!(seen.lock().len(), 2);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn download_keeps_the_signed_query_and_sends_no_credentials_across_redirects() {
        let body = b"download body".to_vec();
        let (cdn, cdn_seen) = serve(respond({
            let body = body.clone();
            move |_| ok(&body)
        }))
        .await;
        let (front, front_seen) = serve(respond(move |_| {
            redirect(&format!(
                "http://{cdn}/target?ex=1&is=2&hm=abc&redirected=yes"
            ))
        }))
        .await;
        let root = scratch("download-auth");
        let dir = scratch("download-auth-dest");
        fs::create_dir_all(&dir).unwrap();
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        cache
            .download_to(
                &NoRefresh,
                key(4),
                &format!("http://{front}/attachments/1/2/a.txt?ex=65d903de&is=65c68ede&hm=2481f3&"),
                &dir.join("a.txt"),
            )
            .await
            .unwrap();
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), body);
        let front_seen = front_seen.lock().clone();
        let cdn_seen = cdn_seen.lock().clone();
        assert!(
            front_seen[0].starts_with(
                "get /attachments/1/2/a.txt?ex=65d903de&is=65c68ede&hm=2481f3& http/1.1"
            ),
            "the signed query was not sent exactly as written"
        );
        assert!(
            cdn_seen[0].starts_with("get /target?ex=1&is=2&hm=abc&redirected=yes http/1.1"),
            "the redirect target was not requested exactly as written"
        );
        for head in front_seen.iter().chain(&cdn_seen) {
            for forbidden in [
                "authorization:",
                "cookie:",
                "proxy-authorization:",
                "x-super-properties:",
                "x-installation-id:",
                "x-fingerprint:",
                "x-discord-locale:",
                "sec-ch-ua",
            ] {
                assert!(!head.contains(forbidden), "a credential header was sent");
            }
            // A file download is not an image subresource: `*/*`, Chromium's
            // `fetch()` default, and only the public user agent besides it.
            assert!(head.contains("accept: */*"), "wrong Accept for a file");
            assert!(
                head.contains(&format!("user-agent: {}", TEST_USER_AGENT.to_lowercase())),
                "the public user agent was not sent"
            );
        }
        assert!(!root.exists(), "a download must not create the cache");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn download_redirects_to_credentials_or_other_schemes_are_refused_and_create_nothing() {
        let root = scratch("download-redirect");
        let dir = scratch("download-redirect-dest");
        fs::create_dir_all(&dir).unwrap();
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        for location in [
            "http://user:pass@127.0.0.1:1/x".to_owned(),
            "ftp://127.0.0.1/x".to_owned(),
            "http://example.invalid/x".to_owned(),
        ] {
            let (address, _) = serve(respond(move |_| redirect(&location))).await;
            let result = cache
                .download_to(
                    &NoRefresh,
                    key(4),
                    &format!("http://{address}/a"),
                    &dir.join("a.bin"),
                )
                .await;
            assert_eq!(result, Err(AttachmentError::InvalidUrl));
        }
        // A chain that never ends stops after a fixed number of hops.
        let (address, seen) = serve(respond(move |_| redirect("/again"))).await;
        let result = cache
            .download_to(
                &NoRefresh,
                key(4),
                &format!("http://{address}/a"),
                &dir.join("a.bin"),
            )
            .await;
        assert_eq!(result, Err(AttachmentError::Network));
        assert_eq!(seen.lock().len(), MAX_REDIRECTS + 1);
        assert!(names_in(&dir).is_empty(), "nothing was written");
        assert!(!root.exists(), "a download must not create the cache");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn failed_downloads_leave_no_partial_file_and_keep_the_previous_destination() {
        let root = scratch("download-failed");
        let dir = scratch("download-failed-dest");
        fs::create_dir_all(&dir).unwrap();
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let (address, _) = serve(respond(|head: &str| {
            if head.starts_with("get /status ") {
                status(500)
            } else {
                // Declares 4096 bytes, sends 1000, then closes the connection.
                let mut response =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n"
                        .to_vec();
                response.extend([b'x'; 1000]);
                response
            }
        }))
        .await;
        for previous in [false, true] {
            let destination = dir.join("a.bin");
            if previous {
                fs::write(&destination, b"previous").unwrap();
            }
            let result = cache
                .download_to(
                    &NoRefresh,
                    key(4),
                    &format!("http://{address}/status"),
                    &destination,
                )
                .await;
            assert_eq!(result, Err(AttachmentError::Http(500)));
            let result = cache
                .download_to(
                    &NoRefresh,
                    key(4),
                    &format!("http://{address}/truncated"),
                    &destination,
                )
                .await;
            assert_eq!(result, Err(AttachmentError::Network));
            let expected: Vec<&str> = if previous { vec!["a.bin"] } else { Vec::new() };
            assert_eq!(names_in(&dir), expected);
            if previous {
                assert_eq!(fs::read(&destination).unwrap(), b"previous");
            }
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_save_into_a_missing_folder_is_refused_and_creates_nothing() {
        let root = scratch("download-missing");
        let dir = scratch("download-missing-dest");
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let (address, _) = serve(respond(|_| ok(b"abc"))).await;
        let result = cache
            .download_to(
                &NoRefresh,
                key(4),
                &format!("http://{address}/a"),
                &dir.join("a.bin"),
            )
            .await;
        assert_eq!(result, Err(AttachmentError::DestinationUnavailable));
        assert!(!dir.exists(), "the folder must not be created");
        assert!(!root.exists(), "a download must not create the cache");
    }

    #[tokio::test]
    async fn an_expired_signature_is_refreshed_once_for_a_download_too() {
        let (address, _) = serve(respond(|head: &str| {
            if head.contains("sig=old") {
                status(403)
            } else {
                ok(b"fresh bytes")
            }
        }))
        .await;
        let root = scratch("download-refresh");
        let dir = scratch("download-refresh-dest");
        fs::create_dir_all(&dir).unwrap();
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let expired = format!("http://{address}/a?sig=old");
        let fresh = format!("http://{address}/a?sig=new");

        let refresher = Refresher::new(Some(fresh));
        cache
            .download_to(&refresher, key(4), &expired, &dir.join("a.bin"))
            .await
            .unwrap();
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"fresh bytes");
        assert_eq!(refresher.calls(), 1);

        let refresher = Refresher::new(None);
        let result = cache
            .download_to(&refresher, key(4), &expired, &dir.join("b.bin"))
            .await;
        assert_eq!(result, Err(AttachmentError::RefreshFailed));
        assert_eq!(refresher.calls(), 1);
        assert!(!dir.join("b.bin").exists());

        let refresher = Refresher::new(Some(expired.clone()));
        let result = cache
            .download_to(&refresher, key(4), &expired, &dir.join("c.bin"))
            .await;
        assert_eq!(result, Err(AttachmentError::Http(403)));
        assert_eq!(refresher.calls(), 1);
        assert!(!dir.join("c.bin").exists());
        assert_eq!(names_in(&dir), ["a.bin"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cancelled_downloads_remove_their_part_file_and_keep_the_old_destination() {
        let root = scratch("download-abort");
        let dir = scratch("download-abort-dest");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.bin"), b"previous").unwrap();
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nx")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let worker = tokio::spawn({
            let cache = cache.clone();
            let destination = dir.join("a.bin");
            async move {
                cache
                    .download_to(
                        &NoRefresh,
                        key(4),
                        &format!("http://{address}/a"),
                        &destination,
                    )
                    .await
            }
        });
        wait_until(|| names_in(&dir).len() == 2).await;
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        wait_until(|| names_in(&dir) == ["a.bin"]).await;
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"previous");
        assert_eq!(cache.fetch_slots.available_permits(), FETCH_SLOTS);
        server.abort();
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn purge_mid_download_stops_the_stream_and_removes_its_part_file() {
        let root = scratch("download-purge");
        let dir = scratch("download-purge-dest");
        fs::create_dir_all(&dir).unwrap();
        let cache = AttachmentCache::for_local_tests(ACCOUNT, root.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nx")
                .await
                .unwrap();
            // The rest of the body arrives only after the cache is purged.
            let _ = released.await;
            let _ = socket.write_all(&[b'x'; 4095]).await;
            std::future::pending::<()>().await;
        });
        let worker = tokio::spawn({
            let cache = cache.clone();
            let destination = dir.join("a.bin");
            async move {
                cache
                    .download_to(
                        &NoRefresh,
                        key(4),
                        &format!("http://{address}/a"),
                        &destination,
                    )
                    .await
            }
        });
        wait_until(|| names_in(&dir).len() == 1).await;
        cache.purge().await.unwrap();
        release.send(()).unwrap();
        assert_eq!(worker.await.unwrap(), Err(AttachmentError::WorkerStopped));
        assert!(names_in(&dir).is_empty(), "the part file was not removed");
        server.abort();
        let _ = fs::remove_dir_all(dir);
    }
}
