//! Live DM and group-DM calls (SPEC §16 milestone 47).
//!
//! At most [`MAX_CALLS`] calls with [`MAX_CALL_USERS`] ringing and connected
//! users each are held (later ones are refused; a known call always updates).
//! Calls are not part of READY: the set starts empty on every READY and is
//! rebuilt from CALL_CREATE, so a call that ended while disconnected never
//! lingers. The sorted view the UI reads is rebuilt once per change.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::Arc;

use fastcord_model::{CallCreate, CallUpdate, Snowflake, VoiceState};

pub const MAX_CALLS: usize = 32;
/// Group DMs have at most ten members; the bound leaves room for oddities.
pub const MAX_CALL_USERS: usize = 16;

/// One call as the UI sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallInfo {
    pub channel_id: Snowflake,
    /// Users still being rung.
    pub ringing: Vec<Snowflake>,
    /// Users connected to the call.
    pub participants: Vec<Snowflake>,
    /// The account itself is being rung: an incoming call to answer.
    pub ringing_me: bool,
}

#[derive(Debug, Default)]
pub(super) struct Calls {
    me: Option<Snowflake>,
    map: BTreeMap<Snowflake, CallInfo>,
    rows: Arc<[CallInfo]>,
}

fn capped(mut users: Vec<Snowflake>) -> Vec<Snowflake> {
    users.truncate(MAX_CALL_USERS);
    users.dedup();
    users
}

impl Calls {
    pub(super) fn reset(&mut self, me: Option<Snowflake>) {
        *self = Self {
            me,
            ..Self::default()
        };
    }

    fn entry(&mut self, channel: Snowflake) -> Option<&mut CallInfo> {
        if self.map.len() >= MAX_CALLS && !self.map.contains_key(&channel) {
            return None;
        }
        Some(self.map.entry(channel).or_insert_with(|| CallInfo {
            channel_id: channel,
            ringing: Vec::new(),
            participants: Vec::new(),
            ringing_me: false,
        }))
    }

    /// Re-derives `ringing` and `ringing_me`, rebuilds the shared view when
    /// the call differs from `before`, and returns whether it did.
    fn finish(&mut self, channel: Snowflake, before: Option<CallInfo>) -> bool {
        let me = self.me;
        if let Some(call) = self.map.get_mut(&channel) {
            let connected = call.participants.clone();
            call.ringing.retain(|user| !connected.contains(user));
            call.ringing_me = me.is_some_and(|me| call.ringing.contains(&me));
        }
        let changed = self.map.get(&channel) != before.as_ref();
        if changed {
            self.rows = self.map.values().cloned().collect();
        }
        changed
    }

    pub(super) fn create(&mut self, create: CallCreate) -> bool {
        let before = self.map.get(&create.channel_id).cloned();
        let Some(call) = self.entry(create.channel_id) else {
            return false;
        };
        call.ringing = capped(create.ringing);
        call.participants = capped(
            create
                .voice_states
                .iter()
                .filter(|state| state.channel_id == Some(create.channel_id))
                .map(|state| state.user_id)
                .collect(),
        );
        self.finish(create.channel_id, before)
    }

    pub(super) fn update(&mut self, update: CallUpdate) -> bool {
        let before = self.map.get(&update.channel_id).cloned();
        let Some(call) = self.entry(update.channel_id) else {
            return false;
        };
        call.ringing = capped(update.ringing);
        self.finish(update.channel_id, before)
    }

    pub(super) fn delete(&mut self, channel: Snowflake) -> bool {
        if self.map.remove(&channel).is_none() {
            return false;
        }
        self.rows = self.map.values().cloned().collect();
        true
    }

    /// A voice state without a guild: someone joined, moved or left a private
    /// call. A join for a call nobody announced still creates it (CALL_CREATE
    /// replaces it if it arrives later); a leave never creates one.
    pub(super) fn voice(&mut self, state: &VoiceState) -> bool {
        let mut changed = false;
        let left: Vec<Snowflake> = self
            .map
            .iter()
            .filter(|(id, call)| {
                state.channel_id != Some(**id) && call.participants.contains(&state.user_id)
            })
            .map(|(id, _)| *id)
            .collect();
        for channel in left {
            let before = self.map.get(&channel).cloned();
            if let Some(call) = self.map.get_mut(&channel) {
                call.participants.retain(|user| *user != state.user_id);
            }
            changed |= self.finish(channel, before);
        }
        if let Some(channel) = state.channel_id {
            let before = self.map.get(&channel).cloned();
            if let Some(call) = self.entry(channel)
                && !call.participants.contains(&state.user_id)
                && call.participants.len() < MAX_CALL_USERS
            {
                call.participants.push(state.user_id);
            }
            changed |= self.finish(channel, before);
        }
        changed
    }

    pub(super) fn rows(&self) -> Arc<[CallInfo]> {
        Arc::clone(&self.rows)
    }

    pub(super) fn bytes(&self) -> usize {
        // Held twice: in the map and in the shared view.
        self.map
            .values()
            .map(|call| {
                2 * (size_of::<CallInfo>()
                    + size_of::<Snowflake>() * (call.ringing.len() + call.participants.len()))
                    + 48
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> Snowflake {
        Snowflake(n)
    }

    fn state(user: u64, channel: Option<u64>) -> VoiceState {
        VoiceState {
            guild_id: None,
            channel_id: channel.map(id),
            user_id: id(user),
            session_id: String::new(),
            deaf: false,
            mute: false,
            self_deaf: false,
            self_mute: false,
            self_stream: false,
            self_video: false,
            suppress: false,
        }
    }

    fn calls() -> Calls {
        let mut calls = Calls::default();
        calls.reset(Some(id(1)));
        calls
    }

    #[test]
    fn create_marks_an_incoming_call_and_delete_clears_it() {
        let mut calls = calls();
        let create = CallCreate {
            channel_id: id(9),
            ringing: vec![id(1), id(2)],
            voice_states: vec![state(3, Some(9))],
        };
        assert!(calls.create(create.clone()));
        let [call] = &*calls.rows() else {
            panic!("one call")
        };
        assert!(call.ringing_me);
        assert_eq!(call.participants, [id(3)]);
        // The same event again changes nothing.
        assert!(!calls.create(create));
        assert!(calls.update(CallUpdate {
            channel_id: id(9),
            ringing: vec![id(2)],
        }));
        assert!(!calls.rows()[0].ringing_me);
        assert!(calls.delete(id(9)));
        assert!(calls.rows().is_empty() && calls.bytes() == 0);
        assert!(!calls.delete(id(9)));
    }

    #[test]
    fn joining_stops_ringing_and_moving_or_leaving_removes_the_participant() {
        let mut calls = calls();
        calls.create(CallCreate {
            channel_id: id(9),
            ringing: vec![id(1)],
            voice_states: Vec::new(),
        });
        assert!(calls.voice(&state(1, Some(9))));
        let call = calls.rows()[0].clone();
        assert!(!call.ringing_me && call.participants == [id(1)]);
        // Moving to another call leaves the first one empty.
        assert!(calls.voice(&state(1, Some(10))));
        let rows = calls.rows();
        assert!(
            rows.iter()
                .any(|c| c.channel_id == id(9) && c.participants.is_empty())
        );
        assert!(
            rows.iter()
                .any(|c| c.channel_id == id(10) && c.participants == [id(1)])
        );
        assert!(calls.voice(&state(1, None)));
        assert!(calls.rows().iter().all(|c| c.participants.is_empty()));
        // A leave for a user in no call changes nothing and creates nothing.
        let held = calls.rows().len();
        assert!(!calls.voice(&state(7, None)));
        assert_eq!(calls.rows().len(), held);
    }

    #[test]
    fn calls_and_users_are_bounded() {
        let mut calls = calls();
        for n in 0..(MAX_CALLS as u64 + 5) {
            calls.update(CallUpdate {
                channel_id: id(100 + n),
                ringing: (0..40).map(id).collect(),
            });
        }
        assert_eq!(calls.rows().len(), MAX_CALLS);
        assert!(
            calls
                .rows()
                .iter()
                .all(|c| c.ringing.len() <= MAX_CALL_USERS)
        );
        // A known call still updates at the limit.
        assert!(calls.update(CallUpdate {
            channel_id: id(100),
            ringing: Vec::new(),
        }));
    }
}
