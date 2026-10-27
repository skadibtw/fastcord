//! The direct-message and group-DM table: ledgered inserts, removals and the
//! recipient events that change it.

use super::*;

impl Store {
    pub fn private_channel(&self, id: Snowflake) -> Option<&Channel> {
        self.private_channels.get(&id)
    }

    pub fn private_channels(&self) -> impl Iterator<Item = &Channel> {
        self.private_channels.values()
    }

    pub(super) fn put_private_channel(&mut self, channel: Channel) {
        let mut channel = self.normalize_channel(channel);
        if let Some(old) = self.private_channels.get(&channel.id) {
            channel.last_message_id = newest(old.last_message_id, channel.last_message_id);
            // An update that carries no participants (a partial CHANNEL_UPDATE)
            // must not empty the member list; removals arrive as their own event.
            if channel.recipient_ids.is_empty() {
                channel.recipient_ids = old.recipient_ids.clone();
            }
            channel.owner_id = channel.owner_id.or(old.owner_id);
            // A request stays a request until Discord says otherwise: an update
            // that omits the flags must not quietly accept it.
            channel.is_message_request = channel.is_message_request.or(old.is_message_request);
            channel.is_spam = channel.is_spam.or(old.is_spam);
        }
        let cost = channel_bytes(&channel);
        let old = self.private_channels.insert(channel.id, channel);
        self.private_bytes += cost;
        if let Some(old) = old {
            self.private_bytes -= channel_bytes(&old);
        }
    }

    /// Removes a conversation from the table and the ledger.
    pub(super) fn take_private_channel(&mut self, id: Snowflake, changes: &mut Changes) -> bool {
        let Some(old) = self.private_channels.remove(&id) else {
            return false;
        };
        self.private_bytes -= channel_bytes(&old);
        compact_map(&mut self.private_channels);
        changes.private_channels = true;
        true
    }

    /// A new message in a conversation moves its newest-message marker.
    pub(super) fn private_message(&mut self, message: &Message, changes: &mut Changes) {
        if let Some(channel) = self.private_channels.get_mut(&message.channel_id)
            && channel.last_message_id < Some(message.id)
        {
            channel.last_message_id = Some(message.id);
            changes.private_channels = true;
        }
    }

    pub(super) fn recipient_add(
        &mut self,
        channel_id: Snowflake,
        user: User,
        changes: &mut Changes,
    ) {
        if self.current_user != Some(user.id)
            && self
                .private_channels
                .get(&channel_id)
                .is_some_and(|c| c.recipient_ids.len() < MAX_GROUP_MEMBERS)
        {
            let user_id = user.id;
            let user_changed = self.users.get(&user_id) != Some(&user);
            if user_changed {
                self.put_user(user);
            }
            let (added, cost_delta) = {
                let channel = self.private_channels.get_mut(&channel_id).unwrap();
                let before = channel_bytes(channel);
                let added = !channel.recipient_ids.contains(&user_id);
                if added {
                    channel.recipient_ids.push(user_id);
                }
                (added, channel_bytes(channel) - before)
            };
            self.private_bytes += cost_delta;
            changes.private_channels |= user_changed || added;
        }
    }

    pub(super) fn recipient_remove(
        &mut self,
        channel_id: Snowflake,
        user_id: Snowflake,
        changes: &mut Changes,
    ) {
        if self.current_user == Some(user_id) {
            if self.take_private_channel(channel_id, changes) {
                self.sweep_users();
            }
        } else if let Some(channel) = self.private_channels.get_mut(&channel_id) {
            let before = channel.recipient_ids.len();
            channel.recipient_ids.retain(|&id| id != user_id);
            changes.private_channels |= before != channel.recipient_ids.len();
            self.sweep_users();
        }
    }
}
