//! Owner-side stream audio (SPEC §8.7, milestone 35): the loopback capture of
//! the shared application or system, the Opus encoder thread, and the slot
//! that connects the encoder to the stream's RTC connection once it exists.
//!
//! The microphone is never involved: it stays on the parent voice connection,
//! and this audio leaves on the stream connection only, as SOUNDSHARE.
//! Dropping an [`AudioShare`] stops the capture thread first, then the
//! encoder thread, so nothing outlives a share; the worker drops it off the
//! async task because the joins block.

use std::sync::Arc;

use fastcord_audio::soundshare::{self, ShareFrame, SoundShare};
use fastcord_media::MediaSender;
use fastcord_platform::capture::SourceId;
use fastcord_platform::loopback::{self, Loopback, LoopbackError, LoopbackScope, LoopbackSupport};
use parking_lot::Mutex;

/// The stream connection's sender, set by the stream task once it is connected
/// and cleared when it ends; the encoder thread sends through it.
pub(super) type AudioSlot = Arc<Mutex<Option<MediaSender>>>;

/// What this machine can share as audio, for the source picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioOffer {
    /// No audio capture on this system.
    Unavailable,
    /// A window shares its application, a screen shares everything except
    /// fastcord.
    PerApplication,
    /// Only whole-device capture: the call is part of it.
    Broad,
}

impl AudioOffer {
    pub fn from_support(support: LoopbackSupport) -> Self {
        if support.process_loopback {
            Self::PerApplication
        } else if support.system_loopback {
            Self::Broad
        } else {
            Self::Unavailable
        }
    }

    /// Cheap; never opens a device.
    pub fn current() -> Self {
        Self::from_support(loopback::support())
    }

    /// Whether the picker must warn that viewers may hear the call.
    pub fn warns(self) -> bool {
        self == Self::Broad
    }

    /// The picker's description of what "Share audio" does.
    pub fn note(self) -> &'static str {
        match self {
            Self::Unavailable => "Sharing audio is not available on this system yet.",
            Self::PerApplication => {
                "A window shares that application's sound; a screen shares all system sound except fastcord, so your call is not sent. Your microphone stays separate."
            }
            Self::Broad => {
                "Broad capture: this Windows version can only capture everything your speakers play, including your call, so viewers may hear the people in your call. Turn this off to share the picture only."
            }
        }
    }
}

/// What a running share's audio captures, for the share's status text.
pub(super) fn scope_text(scope: LoopbackScope) -> &'static str {
    match scope {
        LoopbackScope::Application { .. } => "app audio",
        LoopbackScope::SystemExcludingFastcord => "system audio without fastcord",
        LoopbackScope::SystemIncludingFastcord => "all system audio (viewers may hear your call)",
    }
}

pub(super) fn error_text(error: &LoopbackError) -> String {
    format!("Could not capture audio: {error}. Turn off “Share audio” to share only the picture.")
}

/// A started audio share.
pub(super) struct AudioShare {
    // Declaration order is drop order: the capture stops feeding before the
    // encoder it feeds is stopped.
    _capture: Loopback,
    _encoder: SoundShare,
    pub(super) slot: AudioSlot,
    pub(super) scope: LoopbackScope,
}

/// Starts capturing the audio that goes with `source`. Blocks briefly.
pub(super) fn start(source: SourceId) -> Result<AudioShare, String> {
    let scope = loopback::scope_for(source, loopback::support())
        .ok_or_else(|| AudioOffer::Unavailable.note().to_owned())?;
    let slot: AudioSlot = Arc::new(Mutex::new(None));
    let sink_slot = Arc::clone(&slot);
    let (encoder, mut input) = soundshare::start(move |frame| {
        if let Some(media) = sink_slot.lock().as_ref() {
            // A full queue or a closing connection drops the frame: stream
            // audio is live, never queued for later.
            let _ = match frame {
                ShareFrame::Audio(packet) => media.send_soundshare(packet),
                ShareFrame::End => media.end_speech(),
            };
        }
    })
    .map_err(|error| format!("Could not start the audio encoder: {error}."))?;
    let capture = loopback::start(scope, Box::new(move |block| input.push(block)))
        .map_err(|error| error_text(&error))?;
    Ok(AudioShare {
        _capture: capture,
        _encoder: encoder,
        slot,
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picker_offers_per_application_audio_and_warns_only_when_broad() {
        let process = LoopbackSupport {
            process_loopback: true,
            system_loopback: true,
        };
        let broad = LoopbackSupport {
            process_loopback: false,
            system_loopback: true,
        };
        assert_eq!(
            AudioOffer::from_support(process),
            AudioOffer::PerApplication
        );
        assert_eq!(AudioOffer::from_support(broad), AudioOffer::Broad);
        assert_eq!(
            AudioOffer::from_support(LoopbackSupport::default()),
            AudioOffer::Unavailable
        );
        assert!(AudioOffer::Broad.warns());
        assert!(!AudioOffer::PerApplication.warns());
        assert!(!AudioOffer::Unavailable.warns());
        assert!(AudioOffer::Broad.note().contains("may hear"));
        assert!(
            AudioOffer::PerApplication
                .note()
                .contains("except fastcord")
        );
    }

    /// A screen share's audio starts, encodes into the (still unconnected)
    /// slot, and tears down both threads on drop. No Discord involved.
    #[test]
    #[cfg(windows)]
    #[ignore = "needs a Windows audio session"]
    fn a_screen_share_audio_starts_and_stops_without_leftovers() {
        use fastcord_platform::capture::SourceKind;
        let audio = start(SourceId::from_raw(SourceKind::Display, 0)).expect("audio starts");
        assert_eq!(audio.scope, LoopbackScope::SystemExcludingFastcord);
        assert!(audio.slot.lock().is_none());
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(audio);
    }

    #[test]
    fn a_failed_audio_source_is_an_error_naming_the_way_out() {
        let text = error_text(&LoopbackError::PermissionDenied);
        assert!(text.contains("refused"));
        assert!(text.contains("Share audio"));
        assert!(scope_text(LoopbackScope::SystemIncludingFastcord).contains("call"));
    }
}
