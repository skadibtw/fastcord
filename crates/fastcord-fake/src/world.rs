//! The synthetic world (`fixtures/fake/world.json`) and the state REST edits.
//! Entities stay JSON values in the shapes Discord sends, so the client
//! parses exactly what the world file says.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::log::now_ms;

pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const MANAGE_MESSAGES: u64 = 1 << 13;
pub const READ_HISTORY: u64 = 1 << 16;
pub const MANAGE_CHANNELS: u64 = 1 << 4;
pub const MANAGE_ROLES: u64 = 1 << 28;
const ADMINISTRATOR: u64 = 1 << 3;

pub struct Chatter {
    pub guild_channel: String,
    pub dm_channel: String,
    pub friend: String,
}

pub struct World {
    pub token: String,
    pub me: Value,
    pub users: Vec<Value>,
    pub presences: Map<String, Value>,
    pub guilds: Vec<Value>,
    pub private_channels: Vec<Value>,
    pub relationships: Vec<Value>,
    pub read_state: Vec<Value>,
    pub messages: HashMap<String, Vec<Value>>,
    pub scheduled_events: Map<String, Value>,
    pub invites: Map<String, Value>,
    pub member_verification: Map<String, Value>,
    pub chatter: Chatter,
    /// Message id to the time it was pinned.
    pub pins: HashMap<String, String>,
    last_id: u64,
    invite_seq: u64,
}

pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn list(doc: &Value, key: &str) -> Result<Vec<Value>, String> {
    doc.get(key)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| format!("world: `{key}` must be an array"))
}

fn map(doc: &Value, key: &str) -> Result<Map<String, Value>, String> {
    doc.get(key)
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| format!("world: `{key}` must be an object"))
}

pub fn iso(ms: u64) -> String {
    let secs = ms / 1000;
    let (year, month, day) = civil((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}+00:00",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        (ms % 1000) * 1000
    )
}

/// Days since 1970-01-01 to (year, month, day).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

impl World {
    pub fn load(text: &str) -> Result<Self, String> {
        let doc: Value = serde_json::from_str(text).map_err(|e| format!("world: {e}"))?;
        let messages: HashMap<String, Vec<Value>> = map(&doc, "messages")?
            .into_iter()
            .map(|(channel, list)| match list {
                Value::Array(list) => Ok((channel, list)),
                _ => Err("world: `messages` values must be arrays".to_owned()),
            })
            .collect::<Result<_, String>>()?;
        let chatter = doc.get("chatter").cloned().unwrap_or(Value::Null);
        let mut world = Self {
            token: self::text(&doc, "token").to_owned(),
            me: doc.get("user").cloned().ok_or("world: missing `user`")?,
            users: list(&doc, "users")?,
            presences: map(&doc, "presences")?,
            guilds: list(&doc, "guilds")?,
            private_channels: list(&doc, "private_channels")?,
            relationships: list(&doc, "relationships")?,
            read_state: list(&doc, "read_state")?,
            messages,
            scheduled_events: map(&doc, "scheduled_events")?,
            invites: map(&doc, "invites")?,
            member_verification: map(&doc, "member_verification")?,
            chatter: Chatter {
                guild_channel: self::text(&chatter, "guild_channel").to_owned(),
                dm_channel: self::text(&chatter, "dm_channel").to_owned(),
                friend: self::text(&chatter, "friend").to_owned(),
            },
            pins: HashMap::new(),
            last_id: 0,
            invite_seq: 0,
        };
        if world.token.is_empty() {
            return Err("world: missing `token`".to_owned());
        }
        for list in world.messages.values() {
            for message in list {
                if message.get("pinned") == Some(&Value::Bool(true)) {
                    world.pins.insert(
                        self::text(message, "id").to_owned(),
                        self::text(message, "timestamp").to_owned(),
                    );
                }
            }
        }
        Ok(world)
    }

    pub fn me_id(&self) -> &str {
        text(&self.me, "id")
    }

    pub fn user(&self, id: &str) -> Option<Value> {
        if id == self.me_id() {
            return Some(self.me.clone());
        }
        self.users.iter().find(|u| text(u, "id") == id).cloned()
    }

    /// A fresh snowflake, strictly increasing.
    pub fn next_id(&mut self) -> String {
        let id = (now_ms().saturating_sub(DISCORD_EPOCH_MS)) << 22;
        self.last_id = id.max(self.last_id + 1);
        self.last_id.to_string()
    }

    pub fn next_invite_code(&mut self) -> String {
        self.invite_seq += 1;
        format!("fake{:04}", self.invite_seq)
    }

    pub fn guild_index(&self, guild: &str) -> Option<usize> {
        self.guilds.iter().position(|g| text(g, "id") == guild)
    }

    /// The guild index and channel index of a guild channel.
    pub fn guild_channel_at(&self, channel: &str) -> Option<(usize, usize)> {
        self.guilds.iter().enumerate().find_map(|(g, guild)| {
            let channels = guild.get("channels")?.as_array()?;
            Some((g, channels.iter().position(|c| text(c, "id") == channel)?))
        })
    }

    /// The channel as a JSON object, guild or private, with its guild id.
    pub fn channel(&self, channel: &str) -> Option<Value> {
        if let Some((g, c)) = self.guild_channel_at(channel) {
            let mut value = self.guilds[g]["channels"][c].clone();
            value["guild_id"] = self.guilds[g]["id"].clone();
            return Some(value);
        }
        self.private_channels
            .iter()
            .find(|c| text(c, "id") == channel)
            .cloned()
    }

    pub fn guild_of_channel(&self, channel: &str) -> Option<String> {
        self.guild_channel_at(channel)
            .map(|(g, _)| text(&self.guilds[g], "id").to_owned())
    }

    fn my_member(&self, guild: &Value) -> Option<Value> {
        let me = self.me_id();
        guild
            .get("members")?
            .as_array()?
            .iter()
            .find(|m| text(m, "user_id") == me)
            .cloned()
    }

    /// Server-wide permissions of the account: the owner and administrators
    /// hold all, a pending member only reads, the rest is `@everyone` plus roles.
    pub fn base_permissions(&self, guild: usize) -> u64 {
        let guild = &self.guilds[guild];
        if text(guild, "owner_id") == self.me_id() {
            return u64::MAX;
        }
        let Some(member) = self.my_member(guild) else {
            return 0;
        };
        if member.get("pending") == Some(&Value::Bool(true)) {
            return VIEW_CHANNEL | READ_HISTORY;
        }
        let guild_id = text(guild, "id");
        let mine: Vec<&str> = member
            .get("roles")
            .and_then(Value::as_array)
            .map(|roles| roles.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut permissions = 0;
        for role in guild
            .get("roles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = text(role, "id");
            if id == guild_id || mine.contains(&id) {
                permissions |= text(role, "permissions").parse::<u64>().unwrap_or(0);
            }
        }
        if permissions & ADMINISTRATOR != 0 {
            u64::MAX
        } else {
            permissions
        }
    }

    /// Permissions in one guild channel: the base, then `@everyone`'s overwrite.
    pub fn channel_permissions(&self, guild: usize, channel: usize) -> u64 {
        let base = self.base_permissions(guild);
        if base == u64::MAX {
            return base;
        }
        let guild_id = text(&self.guilds[guild], "id");
        let mut permissions = base;
        let overwrites = self.guilds[guild]["channels"][channel]
            .get("permission_overwrites")
            .and_then(Value::as_array);
        for overwrite in overwrites.into_iter().flatten() {
            if text(overwrite, "id") == guild_id {
                let deny = text(overwrite, "deny").parse::<u64>().unwrap_or(0);
                let allow = text(overwrite, "allow").parse::<u64>().unwrap_or(0);
                permissions = (permissions & !deny) | allow;
            }
        }
        permissions
    }

    /// Permissions for a channel id; private channels allow everything.
    pub fn permissions_in(&self, channel: &str) -> u64 {
        match self.guild_channel_at(channel) {
            Some((g, c)) => self.channel_permissions(g, c),
            None => u64::MAX,
        }
    }

    /// Adds a message to a channel and returns it as the client sees it.
    pub fn post(
        &mut self,
        channel: &str,
        author: &str,
        content: &str,
        reference: Option<&str>,
    ) -> Value {
        let id = self.next_id();
        let mut mentions = Vec::new();
        for part in content.split("<@").skip(1) {
            let digits: String = part
                .trim_start_matches('!')
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty()
                && part.contains('>')
                && let Some(user) = self.user(&digits)
                && !mentions.contains(&user)
            {
                mentions.push(user);
            }
        }
        let referenced = reference.and_then(|id| {
            self.messages
                .get(channel)?
                .iter()
                .find(|m| text(m, "id") == id)
                .cloned()
        });
        let mut message = json!({
            "id": id,
            "type": if referenced.is_some() { 19 } else { 0 },
            "content": content,
            "channel_id": channel,
            "author": self.user(author).unwrap_or(Value::Null),
            "attachments": [],
            "embeds": [],
            "mentions": mentions,
            "mention_roles": [],
            "pinned": false,
            "mention_everyone": content.contains("@everyone"),
            "tts": false,
            "timestamp": iso(now_ms()),
            "edited_timestamp": null,
            "flags": 0,
            "components": [],
        });
        if let Some(referenced) = referenced {
            message["message_reference"] = json!({
                "type": 0, "channel_id": channel, "message_id": text(&referenced, "id"),
            });
            message["referenced_message"] = referenced;
        }
        self.messages
            .entry(channel.to_owned())
            .or_default()
            .push(message.clone());
        self.set_last_message(channel, &id);
        message
    }

    fn set_last_message(&mut self, channel: &str, id: &str) {
        if let Some((g, c)) = self.guild_channel_at(channel) {
            self.guilds[g]["channels"][c]["last_message_id"] = id.into();
        } else if let Some(c) = self
            .private_channels
            .iter_mut()
            .find(|c| text(c, "id") == channel)
        {
            c["last_message_id"] = id.into();
        }
    }

    /// A message event payload: the message plus its guild.
    pub fn with_guild(&self, message: &Value) -> Value {
        let mut data = message.clone();
        if let Some(guild) = self.guild_of_channel(text(message, "channel_id")) {
            data["guild_id"] = guild.into();
        }
        data
    }

    fn presence_status(&self, user: &str) -> &str {
        self.presences
            .get(user)
            .and_then(Value::as_str)
            .unwrap_or("offline")
    }

    /// The READY payload.
    pub fn ready(&self, session_id: &str, port: u16) -> Value {
        let guilds: Vec<Value> = self
            .guilds
            .iter()
            .map(|guild| {
                let mut guild = guild.clone();
                let id = text(&guild, "id").to_owned();
                if let Some(object) = guild.as_object_mut() {
                    object.remove("members");
                    object.insert(
                        "guild_scheduled_events".into(),
                        self.scheduled_events.get(&id).cloned().unwrap_or(json!([])),
                    );
                }
                guild
            })
            .collect();
        let merged_members: Vec<Value> = self
            .guilds
            .iter()
            .map(|guild| match self.my_member(guild) {
                Some(member) => json!([member]),
                None => json!([]),
            })
            .collect();
        let presences: Vec<Value> = self
            .relationships
            .iter()
            .filter(|r| r.get("type") == Some(&json!(1)))
            .map(|r| {
                let id = text(r, "user_id");
                let status = self.presence_status(id);
                json!({"user_id": id, "status": status, "client_status": {"desktop": status}, "activities": []})
            })
            .collect();
        // READY carries each relationship's user inline.
        let relationships: Vec<Value> = self
            .relationships
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r["user"] = self.user(text(&r, "user_id")).unwrap_or(Value::Null);
                r
            })
            .collect();
        let mut users = self.users.clone();
        users.push(self.me.clone());
        json!({
            "v": 10,
            "user": self.me,
            "users": users,
            "guilds": guilds,
            "merged_members": merged_members,
            "private_channels": self.private_channels,
            "relationships": relationships,
            "presences": presences,
            "read_state": {"entries": self.read_state, "version": 1, "partial": false},
            "user_guild_settings": {"entries": [], "version": 1, "partial": false},
            "sessions": [{"session_id": "all", "status": "online", "active": true,
                          "activities": [], "client_info": {"client": "desktop", "os": "windows", "version": 0}}],
            "session_id": session_id,
            "session_type": "normal",
            "resume_gateway_url": format!("ws://127.0.0.1:{port}/"),
            "geo_ordered_rtc_regions": [],
            "country_code": "US",
        })
    }

    pub fn ready_supplemental(&self) -> Value {
        let ids: Vec<&str> = self.guilds.iter().map(|g| text(g, "id")).collect();
        json!({
            "guilds": ids.iter().map(|id| json!({"id": id, "voice_states": []})).collect::<Vec<_>>(),
            "merged_members": ids.iter().map(|_| json!([])).collect::<Vec<_>>(),
            "merged_presences": {"friends": [], "guilds": ids.iter().map(|_| json!([])).collect::<Vec<_>>()},
            "lazy_private_channels": [],
            "disclose": [],
            "game_invites": [],
        })
    }

    /// A member-list `SYNC` for the guild over `range` (inclusive indexes):
    /// hoisted roles with their online members, then online, then offline.
    pub fn member_list_sync(&self, guild: &str, range: (u64, u64)) -> Option<Value> {
        let guild_value = &self.guilds[self.guild_index(guild)?];
        let roles = guild_value.get("roles")?.as_array()?;
        let hoisted = |id: &str| {
            roles
                .iter()
                .find(|r| text(r, "id") == id && r.get("hoist") == Some(&Value::Bool(true)))
        };
        let position = |role: &Value| role.get("position").and_then(Value::as_i64).unwrap_or(0);
        let mut buckets: Vec<(String, String, i64, Vec<Value>)> = Vec::new();
        let (mut online_count, mut total) = (0usize, 0usize);
        for member in guild_value.get("members")?.as_array()? {
            let user_id = text(member, "user_id");
            let Some(user) = self.user(user_id) else {
                continue;
            };
            total += 1;
            let status = self.presence_status(user_id);
            let online = status != "offline";
            online_count += usize::from(online);
            let mut item = json!({
                "user": user,
                "roles": member.get("roles").cloned().unwrap_or(json!([])),
                "nick": member.get("nick").cloned().unwrap_or(Value::Null),
                "joined_at": member.get("joined_at").cloned().unwrap_or(Value::Null),
            });
            if online {
                item["presence"] = json!({
                    "user": {"id": user_id}, "status": status,
                    "client_status": {"desktop": status}, "activities": [],
                });
            }
            let top = member
                .get("roles")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(hoisted)
                .max_by_key(|role| position(role));
            let (group, order) = match (online, top) {
                (true, Some(role)) => (text(role, "id").to_owned(), 1_000_000 - position(role)),
                (true, None) => ("online".to_owned(), 2_000_000),
                (false, _) => ("offline".to_owned(), 3_000_000),
            };
            match buckets.iter_mut().find(|b| b.0 == group) {
                Some(bucket) => bucket.3.push(item),
                None => buckets.push((group.clone(), group, order, vec![item])),
            }
        }
        buckets.sort_by_key(|b| b.2);
        let mut items = Vec::new();
        let mut groups = Vec::new();
        for (id, _, _, mut members) in buckets {
            members.sort_by(|a, b| text(&a["user"], "username").cmp(text(&b["user"], "username")));
            groups.push(json!({"id": id, "count": members.len()}));
            items.push(json!({"group": {"id": id, "count": members.len()}}));
            items.extend(members.into_iter().map(|m| json!({"member": m})));
        }
        let (first, last) = (range.0 as usize, range.1 as usize);
        let slice: Vec<Value> = items
            .into_iter()
            .skip(first)
            .take(last.saturating_sub(first) + 1)
            .collect();
        Some(json!({
            "id": "everyone",
            "guild_id": guild,
            "member_count": total,
            "online_count": online_count,
            "groups": groups,
            "ops": [{"op": "SYNC", "range": [range.0, range.1], "items": slice}],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn world() -> World {
        World::load(crate::DEFAULT_WORLD).unwrap()
    }

    #[test]
    fn the_bundled_world_has_the_promised_shape() {
        let w = world();
        assert_eq!(w.me_id(), "1001");
        assert_eq!(w.token, "fake-token");
        assert_eq!(w.guilds.len(), 3);
        let names: Vec<&str> = w.guilds.iter().map(|g| text(g, "name")).collect();
        assert_eq!(
            names,
            ["Fake Admin Lab", "Fake Member Club", "Fake Rules Gate"]
        );
        let admin = &w.guilds[0];
        assert_eq!(admin["members"].as_array().unwrap().len(), 30);
        // @everyone plus five named roles.
        assert_eq!(admin["roles"].as_array().unwrap().len(), 6);
        let kinds: Vec<u64> = admin["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["type"].as_u64().unwrap())
            .collect();
        for kind in [0, 2, 4, 5, 13, 15] {
            assert!(kinds.contains(&kind), "channel type {kind}");
        }
        let total: usize = w.messages.values().map(Vec::len).sum();
        assert!((190..=260).contains(&total), "{total} messages");
        assert_eq!(w.private_channels.len(), 3);
        assert_eq!(w.private_channels[2]["type"], 3);
        assert_eq!(w.relationships.iter().filter(|r| r["type"] == 1).count(), 4);
        assert_eq!(w.relationships.iter().filter(|r| r["type"] == 3).count(), 1);
        assert!(w.invites.contains_key("fakeinvite"));
        assert!(!w.pins.is_empty());
    }

    #[test]
    fn permissions_follow_ownership_roles_overwrites_and_pending() {
        let w = world();
        assert_eq!(w.base_permissions(0), u64::MAX, "owner");
        let club = w.base_permissions(1);
        assert!(club & SEND_MESSAGES != 0 && club & MANAGE_CHANNELS == 0);
        let (g, staff) = w.guild_channel_at("6103").unwrap();
        assert_eq!(w.channel_permissions(g, staff) & VIEW_CHANNEL, 0, "hidden");
        let (g, lounge) = w.guild_channel_at("6101").unwrap();
        assert!(w.channel_permissions(g, lounge) & VIEW_CHANNEL != 0);
        assert_eq!(w.base_permissions(2) & SEND_MESSAGES, 0, "pending");
    }

    #[test]
    fn ready_matches_the_world_and_points_back_at_the_fake() {
        let w = world();
        let ready = w.ready("s1", 4242);
        assert_eq!(ready["guilds"].as_array().unwrap().len(), 3);
        assert!(ready["guilds"][0].get("members").is_none());
        assert_eq!(ready["resume_gateway_url"], "ws://127.0.0.1:4242/");
        assert_eq!(ready["merged_members"].as_array().unwrap().len(), 3);
        assert_eq!(ready["merged_members"][2][0]["pending"], true);
    }

    #[test]
    fn member_list_sync_groups_hoisted_roles_then_online_then_offline() {
        let w = world();
        let sync = w.member_list_sync("5001", (0, 99)).unwrap();
        assert_eq!(sync["member_count"], 30);
        let groups = sync["groups"].as_array().unwrap();
        assert_eq!(
            groups.first().unwrap()["id"],
            "7001",
            "Admin is hoisted first"
        );
        assert_eq!(groups.last().unwrap()["id"], "offline");
        let counted: u64 = groups.iter().map(|g| g["count"].as_u64().unwrap()).sum();
        assert_eq!(counted, 30);
        let items = sync["ops"][0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 30 + groups.len());
        let narrow = w.member_list_sync("5001", (0, 4)).unwrap();
        assert_eq!(narrow["ops"][0]["items"].as_array().unwrap().len(), 5);
        assert!(w.member_list_sync("nope", (0, 99)).is_none());
    }

    #[test]
    fn posting_keeps_ids_increasing_and_resolves_mentions() {
        let mut w = world();
        let first = w.post("6005", "1001", "hello <@2001>", None);
        let second = w.post("6005", "1001", "again", Some(text(&first, "id")));
        assert!(
            text(&second, "id").parse::<u64>().unwrap()
                > text(&first, "id").parse::<u64>().unwrap()
        );
        assert_eq!(first["mentions"][0]["id"], "2001");
        assert_eq!(second["type"], 19);
        let (g, c) = w.guild_channel_at("6005").unwrap();
        assert_eq!(w.guilds[g]["channels"][c]["last_message_id"], second["id"]);
        assert_eq!(w.with_guild(&second)["guild_id"], "5001");
    }

    #[test]
    fn timestamps_format_as_discord_does() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000000+00:00");
        assert_eq!(iso(1_000_000_000_000), "2001-09-09T01:46:40.000000+00:00");
        assert_eq!(iso(1_792_000_000_123), "2026-10-14T17:46:40.123000+00:00");
    }

    #[test]
    fn broken_worlds_are_refused() {
        assert!(World::load("{}").is_err());
        assert!(World::load("not json").is_err());
    }
}
