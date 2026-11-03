//! Sender NTP maps video onto actual stream-audio device playout. No guessed
//! fixed delay: absent/stale clocks leave video unsynchronized and immediate.
use fastcord_audio::playout::PlayoutClock;
use fastcord_model::Snowflake;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
const REPORT_AGE: Duration = Duration::from_secs(12);
pub(super) const MAX_WAIT: Duration = Duration::from_millis(500);
const LATE: Duration = Duration::from_millis(80);
#[derive(Clone, Copy)]
struct Mapping {
    user: Snowflake,
    ssrc: u32,
    rtp: u32,
    ntp: u64,
    received: Instant,
}
#[derive(Default)]
struct State {
    audio: Option<Mapping>,
    video: Option<Mapping>,
    audio_ssrc: Option<u32>,
    clock: Option<Arc<PlayoutClock>>,
}
#[derive(Default)]
pub(super) struct AvSync(Mutex<State>);
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Immediate,
    Wait(Duration),
    Late,
}
impl AvSync {
    pub(super) fn audio(&self, ssrc: u32, clock: Option<Arc<PlayoutClock>>) {
        let mut state = self.0.lock();
        if state.audio.is_some_and(|m| m.ssrc != ssrc) {
            state.audio = None;
        }
        state.audio_ssrc = Some(ssrc);
        state.clock = clock;
    }
    pub(super) fn reset(&self) {
        *self.0.lock() = State::default();
    }
    pub(super) fn disable_audio(&self) {
        self.0.lock().clock = None;
    }
    pub(super) fn report(
        &self,
        user: Snowflake,
        ssrc: u32,
        video: bool,
        rtp: u32,
        ntp: u64,
        received: Instant,
    ) {
        if ntp == 0 {
            return;
        }
        let mut state = self.0.lock();
        let slot = if video {
            &mut state.video
        } else {
            &mut state.audio
        };
        if let Some(old) = *slot
            && old.ssrc == ssrc
        {
            if received < old.received {
                return;
            }
            if ntp == old.ntp && rtp == old.rtp {
                return; // A repeated report does not create a new clock epoch.
            }
            let elapsed = ntp.wrapping_sub(old.ntp) as i64;
            let ticks = rtp.wrapping_sub(old.rtp) as i32 as i64;
            let rate: i64 = if video { 90_000 } else { 48_000 };
            let expected = ((i128::from(elapsed) * i128::from(rate)) >> 32) as i64;
            if elapsed <= 0
                || expected <= 0
                || (ticks - expected).abs() > expected.abs() / 20 + rate / 10
            {
                // A discontinuous report starts a fresh mapping; the other
                // clock is forgotten so it cannot bridge epochs accidentally.
                state.audio = None;
                state.video = None;
            }
        }
        let mapping = Some(Mapping {
            user,
            ssrc,
            rtp,
            ntp,
            received,
        });
        if video {
            state.video = mapping;
        } else {
            state.audio = mapping;
        }
    }
    pub(super) fn decide(&self, ssrc: u32, rtp: u32, now: Instant) -> Decision {
        let state = self.0.lock();
        let (Some(audio), Some(video), Some(clock)) =
            (state.audio, state.video, state.clock.as_ref())
        else {
            return Decision::Immediate;
        };
        if video.ssrc != ssrc
            || state.audio_ssrc != Some(audio.ssrc)
            || audio.user != video.user
            || now.saturating_duration_since(audio.received) > REPORT_AGE
            || now.saturating_duration_since(video.received) > REPORT_AGE
        {
            return Decision::Immediate;
        }
        let Some(point) = clock.point(audio.ssrc) else {
            return Decision::Immediate;
        };
        let video_ticks = rtp.wrapping_sub(video.rtp) as i32;
        let audio_ticks = point.rtp_timestamp.wrapping_sub(audio.rtp) as i32;
        if video_ticks.unsigned_abs() > 90_000 * 15 || audio_ticks.unsigned_abs() > 48_000 * 15 {
            return Decision::Immediate;
        }
        let delta = (video.ntp.wrapping_sub(audio.ntp) as i64 as i128) * 1_000_000_000
            / (1i128 << 32)
            + i128::from(video_ticks) * 1_000_000_000 / 90_000
            - i128::from(audio_ticks) * 1_000_000_000 / 48_000;
        let duration = Duration::from_nanos(delta.unsigned_abs().min(u64::MAX as u128) as u64);
        let due = if delta >= 0 {
            point.at.checked_add(duration)
        } else {
            point.at.checked_sub(duration)
        };
        let Some(due) = due else {
            return Decision::Immediate;
        };
        if due > now {
            let wait = due.duration_since(now);
            if wait > MAX_WAIT {
                Decision::Immediate
            } else {
                Decision::Wait(wait)
            }
        } else if now.duration_since(due) > LATE {
            Decision::Late
        } else {
            Decision::Immediate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_audio::test_support::PlayoutHarness;
    const AUDIO: u32 = 42;
    const VIDEO: u32 = 43;
    const NTP: u64 = 100 << 32;
    fn setup(rate: u32, timestamp: u32) -> (AvSync, PlayoutHarness, Instant) {
        let mut audio = PlayoutHarness::new(rate, AUDIO, timestamp);
        audio.callback(Some(Duration::from_millis(8)));
        let point = audio
            .clock
            .point(AUDIO)
            .expect("real mixer/callback published RTP position");
        let sync = AvSync::default();
        sync.audio(AUDIO, Some(audio.clock.clone()));
        sync.report(
            Snowflake(7),
            AUDIO,
            false,
            point.rtp_timestamp,
            NTP,
            point.observed_at,
        );
        sync.report(
            Snowflake(7),
            VIDEO,
            true,
            u32::MAX - 4499,
            NTP,
            point.observed_at,
        );
        (sync, audio, point.at)
    }
    #[test]
    fn real_opus_playout_maps_wrap_and_tracks_device_clock_at_both_rates() {
        for rate in [48_000, 44_100] {
            let (sync, mut audio, at) = setup(rate, u32::MAX - 100);
            assert_eq!(
                sync.decide(VIDEO, 4500, at),
                Decision::Wait(Duration::from_millis(100))
            );
            assert_eq!(
                sync.decide(VIDEO, u32::MAX - 4499, at + Duration::from_millis(100)),
                Decision::Late
            );
            assert_eq!(
                sync.decide(VIDEO, 90_000, at),
                Decision::Immediate,
                "excessive skew never freezes video"
            );
            let before = audio.clock.point(AUDIO).unwrap();
            audio.callback(Some(Duration::from_millis(28)));
            let after = audio.clock.point(AUDIO).unwrap();
            assert_eq!(after.rtp_timestamp.wrapping_sub(before.rtp_timestamp), 480);
            assert!(after.at > before.at + Duration::from_millis(19));
            audio.disable();
            assert_eq!(sync.decide(VIDEO, 4500, at), Decision::Immediate);
        }
    }
    #[test]
    fn resets_mute_source_changes_and_dropped_device_clear_sync() {
        let (sync, audio, at) = setup(48_000, 100);
        sync.disable_audio();
        assert_eq!(sync.decide(VIDEO, 4500, at), Decision::Immediate);
        sync.audio(AUDIO, Some(audio.clock.clone()));
        assert!(matches!(sync.decide(VIDEO, 4500, at), Decision::Wait(_)));
        sync.reset();
        assert_eq!(sync.decide(VIDEO, 4500, at), Decision::Immediate);
        let (sync, audio, at) = setup(48_000, 100);
        drop(audio);
        assert_eq!(sync.decide(VIDEO, 4500, at), Decision::Immediate);
        let (sync, audio, at) = setup(48_000, 100);
        sync.audio(AUDIO + 1, Some(audio.clock.clone()));
        assert_eq!(sync.decide(VIDEO, 4500, at), Decision::Immediate);
    }
    #[test]
    fn discontinuous_or_unrelated_sender_reports_cannot_bridge_epochs() {
        let (sync, _audio, at) = setup(48_000, 100);
        sync.report(Snowflake(7), VIDEO, true, u32::MAX - 4499, NTP, at);
        assert!(
            matches!(sync.decide(VIDEO, 4500, at), Decision::Wait(_)),
            "duplicate harmless"
        );
        sync.report(Snowflake(7), VIDEO, true, 123, NTP + (5 << 32), at);
        assert_eq!(sync.decide(VIDEO, 123, at), Decision::Immediate);
        let (sync, _audio, at) = setup(48_000, 100);
        sync.report(Snowflake(9), VIDEO, true, 4500, NTP + (1 << 32), at);
        assert_eq!(sync.decide(VIDEO, 4500, at), Decision::Immediate);
        assert_eq!(
            sync.decide(VIDEO, 4500, at + REPORT_AGE + Duration::from_secs(1)),
            Decision::Immediate
        );
    }
}
