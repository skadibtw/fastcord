//! The platform H.264 backend: the merged candidate list a screen share (or
//! any everyday decode) opens through.
//!
//! VA-API hardware candidates come first, in the DRM device order discovery
//! produced ([`vaapi::encoder_candidates`]/[`vaapi::decoder_candidates`]); the
//! bundled OpenH264 software codec follows as the single fallback candidate.
//! Selection itself is [`select`]: automatic preference walks hardware, then
//! software, and reports every candidate it skipped, while an explicit
//! hardware or software preference never falls back.
//!
//! Rejected settings are answered with [`OpenError::Invalid`] before any
//! device is touched, and a broken DRM discovery (`/dev/dri` unreadable)
//! fails before any candidate exists, as [`OpenError::Discovery`].

use std::time::Duration;

use crate::codec::{
    Backend, BackendKind, BackendPreference, CodecError, DecodedFrame, EncoderConfig, VideoDecoder,
    VideoEncoder,
};
use crate::openh264::{SoftwareDecoderSource, SoftwareEncoderSource};
use crate::select::{Candidate, OpenError, Selected, select};
use crate::vaapi::{self, HardwareSource};

/// One merged encoder candidate source: a VA-API device or OpenH264.
pub(crate) enum EncoderSource {
    Hardware(HardwareSource),
    Software(SoftwareEncoderSource),
}

/// One merged decoder candidate source: a VA-API device or OpenH264.
pub(crate) enum DecoderSource {
    Hardware(HardwareSource),
    Software(SoftwareDecoderSource),
}

/// The merged encoder candidates: `hardware` (the platform's device order)
/// first, then OpenH264's single software candidate.
fn merged_encoder_candidates(
    hardware: Vec<Candidate<HardwareSource>>,
) -> Vec<Candidate<EncoderSource>> {
    let software = crate::openh264::encoder_candidates();
    hardware
        .into_iter()
        .map(|candidate| Candidate {
            kind: candidate.kind,
            name: candidate.name,
            source: EncoderSource::Hardware(candidate.source),
        })
        .chain(software.into_iter().map(|candidate| Candidate {
            kind: candidate.kind,
            name: candidate.name,
            source: EncoderSource::Software(candidate.source),
        }))
        .collect()
}

/// The merged decoder candidates, in the same order as above.
fn merged_decoder_candidates(
    hardware: Vec<Candidate<HardwareSource>>,
) -> Vec<Candidate<DecoderSource>> {
    let software = crate::openh264::decoder_candidates();
    hardware
        .into_iter()
        .map(|candidate| Candidate {
            kind: candidate.kind,
            name: candidate.name,
            source: DecoderSource::Hardware(candidate.source),
        })
        .chain(software.into_iter().map(|candidate| Candidate {
            kind: candidate.kind,
            name: candidate.name,
            source: DecoderSource::Software(candidate.source),
        }))
        .collect()
}

/// Opens the platform encoder: VA-API hardware first, OpenH264 last, as
/// `preference` allows. Settings are validated before any device is touched,
/// so a rejected configuration is reported as [`OpenError::Invalid`] without
/// an attempt, and a broken device discovery as [`OpenError::Discovery`].
pub(crate) fn open_encoder(
    config: EncoderConfig,
    preference: BackendPreference,
) -> Result<Selected<Box<dyn VideoEncoder>>, OpenError> {
    // A rejected configuration never touches the devices (nor the software
    // codec's own validation).
    config.validate().map_err(OpenError::Invalid)?;
    let hardware = vaapi::encoder_candidates().map_err(OpenError::Discovery)?;
    select(
        preference,
        merged_encoder_candidates(hardware),
        |source| match source {
            EncoderSource::Hardware(hardware) => {
                Ok(Box::new(hardware.open_encoder(config)?) as Box<dyn VideoEncoder>)
            }
            EncoderSource::Software(software) => {
                Ok(Box::new(software.open(config)?) as Box<dyn VideoEncoder>)
            }
        },
    )
}

/// Opens the platform decoder, in the same order and with the same
/// preference semantics as above. Under `Auto` a VA-API decoder is wrapped so
/// that a stream it cannot express moves to OpenH264 ([`Fallback`]).
pub(crate) fn open_decoder(
    preference: BackendPreference,
) -> Result<Selected<Box<dyn VideoDecoder>>, OpenError> {
    let hardware = vaapi::decoder_candidates().map_err(OpenError::Discovery)?;
    let mut selected = select(
        preference,
        merged_decoder_candidates(hardware),
        |source| match source {
            DecoderSource::Hardware(hardware) => {
                Ok(Box::new(hardware.open_decoder()?) as Box<dyn VideoDecoder>)
            }
            DecoderSource::Software(software) => {
                Ok(Box::new(software.open()?) as Box<dyn VideoDecoder>)
            }
        },
    )?;
    if preference == BackendPreference::Auto
        && selected.codec.backend().kind == BackendKind::Hardware
    {
        selected.codec = Box::new(Fallback {
            current: selected.codec,
            software: Some(SoftwareDecoderSource),
        });
    }
    Ok(selected)
}

/// A VA-API decoder that hands over to OpenH264 once (#126). VA-API is chosen
/// before the stream is seen and refuses B slices, POC type 1, deep DPBs and
/// other common High-profile features as `Unsupported`; OpenH264 decodes
/// them. VA-API's references are lost in the switch, so the failing unit is
/// reported as damaged and the caller resumes at the next keyframe.
struct Fallback {
    current: Box<dyn VideoDecoder>,
    software: Option<SoftwareDecoderSource>,
}

impl VideoDecoder for Fallback {
    fn backend(&self) -> &Backend {
        self.current.backend()
    }

    fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        match self.current.decode(access_unit, timestamp, out) {
            Err(CodecError::Unsupported(_)) if self.software.is_some() => {
                if let Some(software) = self.software.take() {
                    self.current = Box::new(software.open()?);
                }
                Err(CodecError::InvalidBitstream(
                    "the hardware decoder cannot decode this stream; continuing in software",
                ))
            }
            other => other,
        }
    }

    fn flush(&mut self, out: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        self.current.flush(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::{Attempt, OpenError};
    use crate::vaapi::HardwareSource;
    use crate::vaapi::bindings::VAProfile_VAProfileH264ConstrainedBaseline;
    use std::path::PathBuf;

    /// A hardware candidate whose device exists only in name, so opening it
    /// fails without touching any real hardware.
    fn hardware(path: &str, encoder: bool) -> Candidate<HardwareSource> {
        Candidate {
            kind: BackendKind::Hardware,
            name: format!("test vendor on {path}"),
            source: HardwareSource {
                path: PathBuf::from(path),
                vendor: Some("test vendor".to_owned()),
                encoder: encoder.then_some(VAProfile_VAProfileH264ConstrainedBaseline),
                decoder: true,
            },
        }
    }

    #[test]
    fn merged_candidates_walk_hardware_then_the_single_software_source() {
        let merged = merged_encoder_candidates(vec![
            hardware("nonexistent renderD127", true),
            hardware("nonexistent renderD128", true),
        ]);
        assert_eq!(merged.len(), 3);
        let kinds: Vec<BackendKind> = merged.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                BackendKind::Hardware,
                BackendKind::Hardware,
                BackendKind::Software
            ],
            "hardware candidates keep their device order"
        );
        assert!(matches!(merged[0].source, EncoderSource::Hardware(_)));
        assert!(matches!(merged[1].source, EncoderSource::Hardware(_)));
        assert!(matches!(merged[2].source, EncoderSource::Software(_)));
        // The names carry the device order the discovery produced.
        let names: Vec<&str> = merged.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "test vendor on nonexistent renderD127",
                "test vendor on nonexistent renderD128",
                crate::openh264::encoder_candidates()[0].name.as_str(),
            ]
        );
        // Exactly one software source is appended.
        assert_eq!(crate::openh264::encoder_candidates().len(), 1);
        // Decoders merge with the same shape.
        let merged = merged_decoder_candidates(vec![hardware("nonexistent renderD127", false)]);
        let kinds: Vec<BackendKind> = merged.iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [BackendKind::Hardware, BackendKind::Software]);
        assert!(matches!(merged[0].source, DecoderSource::Hardware(_)));
        assert!(matches!(merged[1].source, DecoderSource::Software(_)));
    }

    #[test]
    fn rejected_encoder_settings_are_reported_before_any_device_is_touched() {
        // Zero dimensions fail validation; had discovery run first on this
        // box the answer would have been `Discovery`, not `Invalid`.
        let error = open_encoder(
            EncoderConfig {
                width: 0,
                height: 96,
                frame_rate: 30,
                bitrate: 2_500_000,
                keyframe_interval: 150,
            },
            BackendPreference::Auto,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            OpenError::Invalid(CodecError::InvalidConfig(_))
        ));
    }

    #[test]
    fn an_explicit_hardware_preference_never_builds_a_software_attempt() {
        let candidates = merged_encoder_candidates(vec![hardware("nonexistent renderD127", true)]);
        let error = select(
            BackendPreference::Hardware,
            candidates,
            |source| match source {
                EncoderSource::Hardware(_) => Err::<(), _>(CodecError::PlatformUnavailable(
                    "the VA-API device could not be opened",
                )),
                EncoderSource::Software(_) => {
                    // The software candidate must never be tried here.
                    Err::<(), _>(CodecError::Unsupported(
                        "the software candidate was reached under hardware preference",
                    ))
                }
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            OpenError::Failed(vec![Attempt {
                kind: BackendKind::Hardware,
                name: "test vendor on nonexistent renderD127".to_owned(),
                error: CodecError::PlatformUnavailable("the VA-API device could not be opened",),
            }])
        );
    }

    #[test]
    fn automatic_preference_reports_the_skipped_hardware_attempt() {
        let candidates = merged_encoder_candidates(vec![hardware("nonexistent renderD127", true)]);
        let error = select(BackendPreference::Auto, candidates, |source| match source {
            EncoderSource::Hardware(_) => Err::<(), _>(CodecError::PlatformUnavailable(
                "the VA-API device could not be opened",
            )),
            EncoderSource::Software(_) => Err::<(), _>(CodecError::Unsupported(
                "the software encoder was not opened in this test either",
            )),
        })
        .unwrap_err();
        let names: Vec<&str> = match &error {
            OpenError::Failed(attempts) => attempts
                .iter()
                .map(|attempt| attempt.name.as_str())
                .collect(),
            _ => panic!("automatic selection fell through to {error}"),
        };
        assert_eq!(
            names,
            [
                "test vendor on nonexistent renderD127",
                crate::openh264::NAME
            ]
        );
    }

    /// A hardware decoder that refuses the stream, as VA-API does B slices.
    struct Refusing(Backend);

    impl VideoDecoder for Refusing {
        fn backend(&self) -> &Backend {
            &self.0
        }
        fn decode(
            &mut self,
            _: &[u8],
            _: Duration,
            _: &mut dyn FnMut(DecodedFrame<'_>),
        ) -> Result<(), CodecError> {
            Err(CodecError::Unsupported(
                "B, SP and SI slices are not supported",
            ))
        }
        fn flush(&mut self, _: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
            Ok(())
        }
    }

    #[test]
    fn a_stream_the_hardware_refuses_moves_to_openh264_once() {
        let mut decoder = Fallback {
            current: Box::new(Refusing(Backend {
                kind: BackendKind::Hardware,
                api: "VA-API",
                name: "test".to_owned(),
            })),
            software: Some(SoftwareDecoderSource),
        };
        let unit = [0, 0, 0, 1, 0x41, 0x9a];
        let switched = decoder.decode(&unit, Duration::ZERO, &mut |_| {});
        // Not fatal: the caller waits for the next keyframe.
        assert!(matches!(switched, Err(CodecError::InvalidBitstream(_))));
        assert_eq!(decoder.backend().kind, BackendKind::Software);
        assert!(decoder.software.is_none(), "only once");
    }
}
