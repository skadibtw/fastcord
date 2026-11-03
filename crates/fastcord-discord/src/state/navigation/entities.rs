//! Bounded held-entity choices. No search, member fetch, or subscription is started.
use super::*;
use fastcord_model::{EntityKind, EntityValue, Select, known_channel_type};

pub const MAX_COMPONENT_ENTITIES_PER_KIND: usize = 128;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityCandidate {
    pub value: EntityValue,
    /// At most 64 Unicode scalars / 256 UTF-8 bytes.
    pub label: String,
    pub channel_type: Option<u8>,
}

impl Store {
    pub fn component_entity_source_allowed(
        &self,
        account: Snowflake,
        guild: Option<Snowflake>,
        source: Snowflake,
    ) -> bool {
        if self.current_user().map(|user| user.id) != Some(account) {
            return false;
        }
        match guild {
            Some(id) => self.guild(id).is_some_and(|guild| {
                let Some(channel) = guild.channel(source) else {
                    return false;
                };
                channels::PermissionContext::new(self, guild).is_some_and(|context| {
                    let permissions = context.permissions(guild, channel);
                    permissions.view_channel
                        && permissions.read_history
                        && permissions.send_messages
                })
            }),
            None => self.private_channel(source).is_some_and(|channel| {
                channel.kind.is_private()
                    && (channel.kind == ModelChannelKind::GroupDm
                        || channel.is_message_request == Some(false))
                    && channel.is_spam != Some(true)
                    && channel
                        .recipient_ids
                        .iter()
                        .all(|id| self.suppression(*id).is_none())
            }),
        }
    }

    /// Validates one typed choice against the current reducer state, including
    /// inherited/private-thread permissions. Absence is not authority.
    pub fn component_entity_allowed(
        &self,
        account: Snowflake,
        guild: Option<Snowflake>,
        source: Snowflake,
        select: &Select,
        value: EntityValue,
    ) -> bool {
        if !self.component_entity_source_allowed(account, guild, source) {
            return false;
        }
        match (guild, value.kind) {
            (Some(id), EntityKind::Channel) => self.guild(id).is_some_and(|guild| {
                let Some(channel) = guild.channel(value.id) else {
                    return false;
                };
                select.accepts(value, Some(channel.kind.to_u8()))
                    && channels::PermissionContext::new(self, guild)
                        .is_some_and(|context| context.permissions(guild, channel).view_channel)
            }),
            (Some(id), EntityKind::Role) => {
                select.accepts(value, None)
                    && value.id != id
                    && self
                        .guild(id)
                        .is_some_and(|guild| guild.roles().iter().any(|role| role.id == value.id))
            }
            (Some(id), EntityKind::User) => {
                select.accepts(value, None)
                    && self.user(value.id).is_some()
                    && self.suppression(value.id).is_none()
                    && self
                        .guild(id)
                        .is_some_and(|guild| guild.member(value.id).is_some())
            }
            (None, EntityKind::User) => {
                select.accepts(value, None)
                    && self.user(value.id).is_some()
                    && self.suppression(value.id).is_none()
                    && (value.id == account
                        || self
                            .private_channel(source)
                            .is_some_and(|channel| channel.recipient_ids.contains(&value.id)))
            }
            _ => false,
        }
    }

    /// Published only for a visible history containing entity components.
    /// IDs are sorted before truncation so HashMap iteration cannot churn drafts.
    pub fn component_entities(
        &self,
        account: Snowflake,
        guild: Option<Snowflake>,
        source: Snowflake,
    ) -> Vec<EntityCandidate> {
        if !self.component_entity_source_allowed(account, guild, source) {
            return Vec::new();
        }
        let mut result = Vec::new();
        let label = |name: &str| name.chars().take(64).collect::<String>();
        let mut users = std::collections::BTreeSet::new();
        let mut keep_user = |id| {
            if self.suppression(id).is_none() && self.user(id).is_some() {
                users.insert(id);
                if users.len() > MAX_COMPONENT_ENTITIES_PER_KIND {
                    users.pop_last();
                }
            }
        };
        if let Some(guild) = guild.and_then(|id| self.guild(id)) {
            guild.members().for_each(|member| keep_user(member.user_id));
        } else if let Some(channel) = self.private_channel(source) {
            channel
                .recipient_ids
                .iter()
                .copied()
                .chain(std::iter::once(account))
                .for_each(&mut keep_user);
        }
        for id in users {
            let user = self.user(id).expect("filtered held user");
            let nick = guild
                .and_then(|id| self.guild(id))
                .and_then(|g| g.member(id))
                .and_then(|m| m.nick.as_deref());
            result.push(EntityCandidate {
                value: EntityValue {
                    kind: EntityKind::User,
                    id,
                },
                label: label(
                    nick.or(user.global_name.as_deref())
                        .unwrap_or(&user.username),
                ),
                channel_type: None,
            });
        }
        if let Some(guild) = guild.and_then(|id| self.guild(id)) {
            for role in guild
                .roles()
                .iter()
                .filter(|role| role.id != guild.id())
                .take(MAX_COMPONENT_ENTITIES_PER_KIND)
            {
                result.push(EntityCandidate {
                    value: EntityValue {
                        kind: EntityKind::Role,
                        id: role.id,
                    },
                    label: label(&role.name),
                    channel_type: None,
                });
            }
            if let Some(context) = channels::PermissionContext::new(self, guild) {
                for channel in guild
                    .channels()
                    .filter(|channel| {
                        known_channel_type(channel.kind.to_u8())
                            && context.permissions(guild, channel).view_channel
                    })
                    .take(MAX_COMPONENT_ENTITIES_PER_KIND)
                {
                    result.push(EntityCandidate {
                        value: EntityValue {
                            kind: EntityKind::Channel,
                            id: channel.id,
                        },
                        label: label(channel.name.as_deref().unwrap_or("")),
                        channel_type: Some(channel.kind.to_u8()),
                    });
                }
            }
        }
        result
    }
}
