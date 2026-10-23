//! The REST side: a stateful core over [`World`]. Whatever is not here
//! answers 404 `fake: not implemented`; no success is ever invented.

use serde_json::{Value, json};

use crate::http::{Request, Response, percent_decode};
use crate::log::now_ms;
use crate::world::{
    MANAGE_CHANNELS, MANAGE_MESSAGES, MANAGE_ROLES, SEND_MESSAGES, VIEW_CHANNEL, World, iso, text,
};

pub const PREFIX: &str = "/api/v10";

/// A Gateway dispatch the answer caused.
#[derive(Clone, Debug)]
pub struct Event {
    pub name: &'static str,
    pub data: Value,
}

pub struct Outcome {
    pub response: Response,
    pub events: Vec<Event>,
    pub unimplemented: bool,
}

pub struct Ctx {
    pub port: u16,
}

fn event(name: &'static str, data: Value) -> Event {
    Event { name, data }
}

fn done(response: Response, events: Vec<Event>) -> Outcome {
    Outcome {
        response,
        events,
        unimplemented: false,
    }
}

fn ok(value: Value) -> Outcome {
    done(Response::json(200, &value), Vec::new())
}

fn no_content(events: Vec<Event>) -> Outcome {
    done(Response::empty(204), events)
}

fn error(status: u16, code: u32, message: &str) -> Outcome {
    done(
        Response::json(status, &json!({"message": message, "code": code})),
        Vec::new(),
    )
}

fn missing_access() -> Outcome {
    error(403, 50001, "Missing Access")
}

fn missing_permissions() -> Outcome {
    error(403, 50013, "Missing Permissions")
}

fn unknown_channel() -> Outcome {
    error(404, 10003, "Unknown Channel")
}

pub fn not_implemented() -> Outcome {
    let mut outcome = error(404, 0, "fake: not implemented");
    outcome.unimplemented = true;
    outcome
}

fn id_of(value: &str) -> Option<u64> {
    value.parse().ok()
}

pub fn handle(world: &mut World, req: &Request, ctx: &Ctx) -> Outcome {
    let Some(path) = req.path.strip_prefix(PREFIX) else {
        return not_implemented();
    };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments != ["gateway"] && req.header("authorization") != Some(world.token.as_str()) {
        return error(401, 0, "401: Unauthorized");
    }
    let query = req.query_pairs();
    let get = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let body = req.json();
    let method = req.method.as_str();
    match (method, segments.as_slice()) {
        ("GET", ["gateway"]) => ok(json!({"url": format!("ws://127.0.0.1:{}", ctx.port)})),
        ("GET", ["users", "@me"]) => ok(world.me.clone()),
        ("GET", ["channels", channel, "messages"]) => history(world, channel, &get),
        ("GET", ["channels", channel, "messages", "pins"])
        | ("GET", ["channels", channel, "pins"]) => pins(world, channel),
        ("POST", ["channels", channel, "messages"]) => create(world, channel, body),
        ("PATCH", ["channels", channel, "messages", message]) => {
            edit(world, channel, message, body)
        }
        ("DELETE", ["channels", channel, "messages", message]) => delete(world, channel, message),
        (
            "PUT" | "DELETE",
            [
                "channels",
                channel,
                "messages",
                message,
                "reactions",
                emoji,
                tail @ ..,
            ],
        ) => react(world, method == "PUT", channel, message, emoji, tail),
        ("POST", ["channels", channel, "messages", message, "ack"]) => {
            ack(world, channel, message, body.as_ref())
        }
        ("POST", ["channels", channel, "typing"]) => match world.channel(channel) {
            Some(_) => no_content(Vec::new()),
            None => unknown_channel(),
        },
        ("PUT" | "DELETE", ["channels", channel, "pins", message]) => {
            pin(world, method == "PUT", channel, message)
        }
        ("POST", ["guilds", guild, "channels"]) => channel_create(world, guild, body),
        ("PATCH", ["guilds", guild, "channels"]) => channel_reorder(world, guild, body),
        ("PATCH", ["channels", channel]) => channel_edit(world, channel, body),
        ("DELETE", ["channels", channel]) => channel_delete(world, channel),
        ("POST", ["guilds", guild, "roles"]) => role_create(world, guild, body),
        ("PATCH", ["guilds", guild, "roles", role]) => role_edit(world, guild, role, body),
        ("DELETE", ["guilds", guild, "roles", role]) => role_delete(world, guild, role),
        ("GET", ["invites", code]) => match world.invites.get(*code) {
            Some(invite) => ok(invite.clone()),
            None => error(404, 10006, "Unknown Invite"),
        },
        ("DELETE", ["invites", code]) => match world.invites.remove(*code) {
            Some(invite) => ok(invite),
            None => error(404, 10006, "Unknown Invite"),
        },
        ("POST", ["channels", channel, "invites"]) => invite_create(world, channel, body),
        ("GET", ["channels", channel, "invites"]) => {
            invites_of(world, |invite| text(&invite["channel"], "id") == *channel)
        }
        ("GET", ["guilds", guild, "invites"]) => {
            invites_of(world, |invite| text(&invite["guild"], "id") == *guild)
        }
        ("GET", ["guilds", guild, "member-verification"]) => {
            match world.member_verification.get(*guild) {
                Some(form) => ok(form.clone()),
                None => error(404, 10068, "Unknown Form"),
            }
        }
        ("PUT", ["guilds", guild, "requests", "@me"]) => verify(world, guild),
        ("GET", ["guilds", guild, "scheduled-events"]) => match world.guild_index(guild) {
            Some(_) => ok(world
                .scheduled_events
                .get(*guild)
                .cloned()
                .unwrap_or(json!([]))),
            None => error(404, 10004, "Unknown Guild"),
        },
        _ => not_implemented(),
    }
}

fn history<'a>(world: &World, channel: &str, get: &impl Fn(&str) -> Option<&'a str>) -> Outcome {
    if world.channel(channel).is_none() {
        return unknown_channel();
    }
    if world.permissions_in(channel) & VIEW_CHANNEL == 0 {
        return missing_access();
    }
    let all = world
        .messages
        .get(channel)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let id = |message: &Value| id_of(text(message, "id")).unwrap_or(0);
    let limit = get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 100);
    let mut picked: Vec<&Value> = if let Some(before) = get("before").and_then(id_of) {
        let older: Vec<&Value> = all.iter().filter(|m| id(m) < before).collect();
        older[older.len().saturating_sub(limit)..].to_vec()
    } else if let Some(after) = get("after").and_then(id_of) {
        all.iter().filter(|m| id(m) > after).take(limit).collect()
    } else if let Some(around) = get("around").and_then(id_of) {
        let at = all
            .iter()
            .position(|m| id(m) >= around)
            .unwrap_or(all.len());
        let start = at.saturating_sub(limit / 2);
        all[start..].iter().take(limit).collect()
    } else {
        all[all.len().saturating_sub(limit)..].iter().collect()
    };
    picked.reverse();
    ok(Value::Array(picked.into_iter().cloned().collect()))
}

fn pins(world: &World, channel: &str) -> Outcome {
    if world.channel(channel).is_none() {
        return unknown_channel();
    }
    if world.permissions_in(channel) & VIEW_CHANNEL == 0 {
        return missing_access();
    }
    let mut items: Vec<(String, Value)> = world
        .messages
        .get(channel)
        .into_iter()
        .flatten()
        .filter(|m| m.get("pinned") == Some(&Value::Bool(true)))
        .map(|m| {
            let at = world.pins.get(text(m, "id")).cloned().unwrap_or_default();
            (at.clone(), json!({"pinned_at": at, "message": m}))
        })
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0));
    ok(json!({"items": items.into_iter().map(|i| i.1).collect::<Vec<_>>(), "has_more": false}))
}

fn create(world: &mut World, channel: &str, body: Option<Value>) -> Outcome {
    if world.channel(channel).is_none() {
        return unknown_channel();
    }
    let permissions = world.permissions_in(channel);
    if permissions & VIEW_CHANNEL == 0 {
        return missing_access();
    }
    if permissions & SEND_MESSAGES == 0 {
        return missing_permissions();
    }
    let body = body.unwrap_or(Value::Null);
    let content = text(&body, "content");
    if content.trim().is_empty() {
        return error(400, 50006, "Cannot send an empty message");
    }
    let reference = body
        .get("message_reference")
        .map(|r| text(r, "message_id").to_owned())
        .filter(|id| !id.is_empty());
    let me = world.me_id().to_owned();
    let mut message = world.post(channel, &me, content, reference.as_deref());
    if let Some(nonce) = body.get("nonce").filter(|n| !n.is_null()) {
        message["nonce"] = nonce.clone();
    }
    let data = world.with_guild(&message);
    done(
        Response::json(200, &message),
        vec![event("MESSAGE_CREATE", data)],
    )
}

fn find_message<'a>(world: &'a mut World, channel: &str, message: &str) -> Option<&'a mut Value> {
    world
        .messages
        .get_mut(channel)?
        .iter_mut()
        .find(|m| text(m, "id") == message)
}

fn edit(world: &mut World, channel: &str, message: &str, body: Option<Value>) -> Outcome {
    let me = world.me_id().to_owned();
    let Some(found) = find_message(world, channel, message) else {
        return error(404, 10008, "Unknown Message");
    };
    if text(&found["author"], "id") != me {
        return error(403, 50005, "Cannot edit a message authored by another user");
    }
    let content = body.as_ref().map_or("", |b| text(b, "content")).to_owned();
    if content.trim().is_empty() {
        return error(400, 50006, "Cannot send an empty message");
    }
    found["content"] = content.into();
    found["edited_timestamp"] = iso(now_ms()).into();
    let updated = found.clone();
    let data = world.with_guild(&updated);
    done(
        Response::json(200, &updated),
        vec![event("MESSAGE_UPDATE", data)],
    )
}

fn delete(world: &mut World, channel: &str, message: &str) -> Outcome {
    let me = world.me_id().to_owned();
    let others_allowed = world.permissions_in(channel) & MANAGE_MESSAGES != 0;
    let Some(list) = world.messages.get_mut(channel) else {
        return unknown_channel();
    };
    let Some(at) = list.iter().position(|m| text(m, "id") == message) else {
        return error(404, 10008, "Unknown Message");
    };
    if text(&list[at]["author"], "id") != me && !others_allowed {
        return missing_permissions();
    }
    list.remove(at);
    world.pins.remove(message);
    let mut data = json!({"id": message, "channel_id": channel});
    if let Some(guild) = world.guild_of_channel(channel) {
        data["guild_id"] = guild.into();
    }
    no_content(vec![event("MESSAGE_DELETE", data)])
}

/// `name:id` or a unicode emoji, as the route spells it.
fn emoji_of(segment: &str) -> Value {
    let decoded = percent_decode(segment);
    match decoded.split_once(':') {
        Some((name, id)) => json!({"id": id, "name": name}),
        None => json!({"id": null, "name": decoded}),
    }
}

fn react(
    world: &mut World,
    add: bool,
    channel: &str,
    message: &str,
    emoji: &str,
    tail: &[&str],
) -> Outcome {
    let me = world.me_id().to_owned();
    // `@me`, `{type}/@me` or `{user}`: who is acted on.
    let user = match tail.last() {
        None | Some(&"@me") => me.clone(),
        Some(user) => (*user).to_owned(),
    };
    let emoji = emoji_of(emoji);
    let guild = world.guild_of_channel(channel);
    if world.channel(channel).is_none() {
        return unknown_channel();
    }
    let permissions = world.permissions_in(channel);
    if permissions & VIEW_CHANNEL == 0 {
        return missing_access();
    }
    if (add && permissions & (1 << 6) == 0)
        || (!add && user != me && permissions & MANAGE_MESSAGES == 0)
    {
        return missing_permissions();
    }
    let Some(found) = find_message(world, channel, message) else {
        return error(404, 10008, "Unknown Message");
    };
    let reactions = found
        .as_object_mut()
        .map(|m| m.entry("reactions").or_insert_with(|| json!([])))
        .and_then(Value::as_array_mut);
    let Some(reactions) = reactions else {
        return not_implemented();
    };
    let same = |r: &Value| r["emoji"]["id"] == emoji["id"] && r["emoji"]["name"] == emoji["name"];
    let mine = user == me;
    let changed = match (reactions.iter().position(same), add) {
        (Some(at), true) => {
            if mine && reactions[at]["me"] == true {
                false
            } else {
                let count = reactions[at]["count"].as_u64().unwrap_or(0) + 1;
                reactions[at]["count"] = count.into();
                reactions[at]["count_details"] = json!({"burst": 0, "normal": count});
                reactions[at]["me"] = (mine || reactions[at]["me"] == true).into();
                true
            }
        }
        (None, true) => {
            reactions.push(json!({
                "emoji": emoji, "count": 1, "count_details": {"burst": 0, "normal": 1},
                "burst_colors": [], "me_burst": false, "burst_me": false, "me": mine, "burst_count": 0,
            }));
            true
        }
        (Some(at), false) => {
            let count = reactions[at]["count"]
                .as_u64()
                .unwrap_or(1)
                .saturating_sub(1);
            if count == 0 {
                reactions.remove(at);
            } else {
                reactions[at]["count"] = count.into();
                reactions[at]["count_details"] = json!({"burst": 0, "normal": count});
                if mine {
                    reactions[at]["me"] = false.into();
                }
            }
            true
        }
        (None, false) => false,
    };
    if reactions.is_empty()
        && let Some(object) = found.as_object_mut()
    {
        object.remove("reactions");
    }
    if !changed {
        return no_content(Vec::new());
    }
    let mut data = json!({
        "user_id": user, "channel_id": channel, "message_id": message,
        "emoji": emoji, "burst": false, "type": 0,
    });
    if let Some(guild) = guild {
        data["guild_id"] = guild.into();
        if add && let Some(user) = world.user(&user) {
            data["member"] = json!({"user": user, "roles": [], "nick": null, "flags": 0, "deaf": false, "mute": false});
        }
    }
    let name = if add {
        "MESSAGE_REACTION_ADD"
    } else {
        "MESSAGE_REACTION_REMOVE"
    };
    no_content(vec![event(name, data)])
}

fn ack(world: &mut World, channel: &str, message: &str, body: Option<&Value>) -> Outcome {
    if world.channel(channel).is_none() {
        return unknown_channel();
    }
    // Every answer carries a fresh token the next automatic ack must send
    // back. A mismatch is counted, not refused, so a client that restarted its
    // session can carry on.
    let issued = world.ack_tokens.get(channel).copied();
    let sent = body
        .and_then(|body| body.get("token"))
        .and_then(Value::as_str);
    let manual = body.is_some_and(|body| body.get("manual").is_some());
    if !manual && sent != issued.map(|n| format!("fake-ack-{n}")).as_deref() {
        world.ack_token_mismatches += 1;
    }
    let next = issued.unwrap_or(0) + 1;
    world.ack_tokens.insert(channel.to_owned(), next);
    match world
        .read_state
        .iter_mut()
        .find(|e| text(e, "id") == channel)
    {
        Some(entry) => {
            entry["last_acked_id"] = message.into();
            entry["mention_count"] = 0.into();
        }
        None => world
            .read_state
            .push(json!({"id": channel, "last_acked_id": message, "mention_count": 0})),
    }
    done(
        Response::json(200, &json!({"token": format!("fake-ack-{next}")})),
        vec![event(
            "MESSAGE_ACK",
            json!({"channel_id": channel, "message_id": message, "version": 1, "mention_count": 0}),
        )],
    )
}

fn pin(world: &mut World, add: bool, channel: &str, message: &str) -> Outcome {
    if world.channel(channel).is_none() {
        return unknown_channel();
    }
    if world.permissions_in(channel) & MANAGE_MESSAGES == 0 {
        return missing_permissions();
    }
    let Some(found) = find_message(world, channel, message) else {
        return error(404, 10008, "Unknown Message");
    };
    found["pinned"] = add.into();
    let updated = found.clone();
    let now = iso(now_ms());
    if add {
        world.pins.insert(message.to_owned(), now.clone());
    } else {
        world.pins.remove(message);
    }
    let mut pins_update = json!({"channel_id": channel, "last_pin_timestamp": now});
    if let Some(guild) = world.guild_of_channel(channel) {
        pins_update["guild_id"] = guild.into();
    }
    no_content(vec![
        event("MESSAGE_UPDATE", world.with_guild(&updated)),
        event("CHANNEL_PINS_UPDATE", pins_update),
    ])
}

fn guild_with(world: &World, guild: &str, needed: u64) -> Result<usize, Outcome> {
    let Some(index) = world.guild_index(guild) else {
        return Err(error(404, 10004, "Unknown Guild"));
    };
    if world.base_permissions(index) & needed == 0 {
        return Err(missing_permissions());
    }
    Ok(index)
}

const CHANNEL_FIELDS: &[&str] = &[
    "name",
    "topic",
    "position",
    "parent_id",
    "nsfw",
    "rate_limit_per_user",
    "user_limit",
    "bitrate",
    "rtc_region",
    "permission_overwrites",
    "default_auto_archive_duration",
    "video_quality_mode",
    "type",
];

fn channel_create(world: &mut World, guild: &str, body: Option<Value>) -> Outcome {
    let index = match guild_with(world, guild, MANAGE_CHANNELS) {
        Ok(index) => index,
        Err(outcome) => return outcome,
    };
    let body = body.unwrap_or(Value::Null);
    let name = text(&body, "name").trim().to_owned();
    if name.is_empty() {
        return error(400, 50035, "Invalid Form Body");
    }
    let id = world.next_id();
    let channels = world.guilds[index]["channels"]
        .as_array()
        .map_or(0, Vec::len);
    let mut channel = json!({
        "id": id, "type": body.get("type").cloned().unwrap_or(json!(0)), "name": name,
        "position": channels, "parent_id": null, "permission_overwrites": [], "nsfw": false,
    });
    for key in CHANNEL_FIELDS {
        if let Some(value) = body.get(*key) {
            channel[*key] = value.clone();
        }
    }
    if let Some(list) = world.guilds[index]["channels"].as_array_mut() {
        list.push(channel.clone());
    }
    channel["guild_id"] = guild.into();
    done(
        Response::json(201, &channel),
        vec![event("CHANNEL_CREATE", channel)],
    )
}

fn channel_edit(world: &mut World, channel: &str, body: Option<Value>) -> Outcome {
    let Some((g, c)) = world.guild_channel_at(channel) else {
        return unknown_channel();
    };
    if world.channel_permissions(g, c) & MANAGE_CHANNELS == 0 {
        return missing_permissions();
    }
    let body = body.unwrap_or(Value::Null);
    for key in CHANNEL_FIELDS.iter().filter(|key| **key != "type") {
        if let Some(value) = body.get(*key) {
            world.guilds[g]["channels"][c][*key] = value.clone();
        }
    }
    let updated = world.channel(channel).unwrap_or(Value::Null);
    done(
        Response::json(200, &updated),
        vec![event("CHANNEL_UPDATE", updated)],
    )
}

fn channel_delete(world: &mut World, channel: &str) -> Outcome {
    let Some((g, c)) = world.guild_channel_at(channel) else {
        return unknown_channel();
    };
    if world.channel_permissions(g, c) & MANAGE_CHANNELS == 0 {
        return missing_permissions();
    }
    let removed = world.channel(channel).unwrap_or(Value::Null);
    if let Some(list) = world.guilds[g]["channels"].as_array_mut() {
        list.remove(c);
        // Children of a deleted category move to the top level, as on Discord.
        for child in list
            .iter_mut()
            .filter(|ch| text(ch, "parent_id") == channel)
        {
            child["parent_id"] = Value::Null;
        }
    }
    world.messages.remove(channel);
    done(
        Response::json(200, &removed),
        vec![event("CHANNEL_DELETE", removed)],
    )
}

fn channel_reorder(world: &mut World, guild: &str, body: Option<Value>) -> Outcome {
    let index = match guild_with(world, guild, MANAGE_CHANNELS) {
        Ok(index) => index,
        Err(outcome) => return outcome,
    };
    let Some(Value::Array(moves)) = body else {
        return error(400, 50035, "Invalid Form Body");
    };
    let mut events = Vec::new();
    for entry in &moves {
        let id = text(entry, "id");
        let Some(at) = world.guilds[index]["channels"]
            .as_array()
            .and_then(|l| l.iter().position(|c| text(c, "id") == id))
        else {
            return unknown_channel();
        };
        let channel = &mut world.guilds[index]["channels"][at];
        for key in ["position", "parent_id"] {
            if let Some(value) = entry.get(key) {
                channel[key] = value.clone();
            }
        }
        let mut data = channel.clone();
        data["guild_id"] = guild.into();
        events.push(event("CHANNEL_UPDATE", data));
    }
    no_content(events)
}

const ROLE_FIELDS: &[&str] = &[
    "name",
    "permissions",
    "color",
    "hoist",
    "mentionable",
    "position",
];

fn role_create(world: &mut World, guild: &str, body: Option<Value>) -> Outcome {
    let index = match guild_with(world, guild, MANAGE_ROLES) {
        Ok(index) => index,
        Err(outcome) => return outcome,
    };
    let body = body.unwrap_or(Value::Null);
    let id = world.next_id();
    let roles = world.guilds[index]["roles"].as_array().map_or(0, Vec::len);
    let mut role = json!({
        "id": id, "name": "new role", "permissions": "0", "position": roles, "color": 0,
        "hoist": false, "managed": false, "mentionable": false,
    });
    for key in ROLE_FIELDS {
        if let Some(value) = body.get(*key) {
            role[*key] = value.clone();
        }
    }
    if let Some(list) = world.guilds[index]["roles"].as_array_mut() {
        list.push(role.clone());
    }
    done(
        Response::json(200, &role),
        vec![event(
            "GUILD_ROLE_CREATE",
            json!({"guild_id": guild, "role": role}),
        )],
    )
}

fn role_edit(world: &mut World, guild: &str, role: &str, body: Option<Value>) -> Outcome {
    let index = match guild_with(world, guild, MANAGE_ROLES) {
        Ok(index) => index,
        Err(outcome) => return outcome,
    };
    let body = body.unwrap_or(Value::Null);
    let Some(found) = world.guilds[index]["roles"]
        .as_array_mut()
        .and_then(|l| l.iter_mut().find(|r| text(r, "id") == role))
    else {
        return error(404, 10011, "Unknown Role");
    };
    for key in ROLE_FIELDS {
        if let Some(value) = body.get(*key) {
            found[*key] = value.clone();
        }
    }
    let updated = found.clone();
    done(
        Response::json(200, &updated),
        vec![event(
            "GUILD_ROLE_UPDATE",
            json!({"guild_id": guild, "role": updated}),
        )],
    )
}

fn role_delete(world: &mut World, guild: &str, role: &str) -> Outcome {
    let index = match guild_with(world, guild, MANAGE_ROLES) {
        Ok(index) => index,
        Err(outcome) => return outcome,
    };
    let Some(roles) = world.guilds[index]["roles"].as_array_mut() else {
        return error(404, 10011, "Unknown Role");
    };
    let Some(at) = roles.iter().position(|r| text(r, "id") == role) else {
        return error(404, 10011, "Unknown Role");
    };
    if role == guild {
        return missing_permissions();
    }
    roles.remove(at);
    for member in world.guilds[index]["members"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        if let Some(list) = member["roles"].as_array_mut() {
            list.retain(|r| r != role);
        }
    }
    no_content(vec![event(
        "GUILD_ROLE_DELETE",
        json!({"guild_id": guild, "role_id": role}),
    )])
}

fn invite_create(world: &mut World, channel: &str, body: Option<Value>) -> Outcome {
    let Some(found) = world.channel(channel) else {
        return unknown_channel();
    };
    let Some(guild) = world.guild_of_channel(channel) else {
        return error(403, 50003, "Cannot execute action on a DM channel");
    };
    if world.permissions_in(channel) & (1 << 0) == 0 {
        return missing_permissions();
    }
    let body = body.unwrap_or(Value::Null);
    let code = world.next_invite_code();
    let index = world.guild_index(&guild).unwrap_or(0);
    let guild_value = &world.guilds[index];
    let invite = json!({
        "type": 0, "code": code,
        "created_at": iso(now_ms()),
        "max_age": body.get("max_age").cloned().unwrap_or(json!(604_800)),
        "max_uses": body.get("max_uses").cloned().unwrap_or(json!(0)),
        "temporary": body.get("temporary").cloned().unwrap_or(json!(false)),
        "uses": 0,
        "guild": {"id": guild, "name": text(guild_value, "name"), "icon": null,
                  "features": guild_value["features"]},
        "channel": {"id": channel, "type": found["type"], "name": text(&found, "name")},
        "inviter": world.me,
    });
    world.invites.insert(code, invite.clone());
    ok(invite)
}

fn invites_of(world: &World, keep: impl Fn(&Value) -> bool) -> Outcome {
    ok(Value::Array(
        world
            .invites
            .values()
            .filter(|invite| invite.get("uses").is_some() && keep(invite))
            .cloned()
            .collect(),
    ))
}

fn verify(world: &mut World, guild: &str) -> Outcome {
    let Some(index) = world.guild_index(guild) else {
        return error(404, 10004, "Unknown Guild");
    };
    if !world.member_verification.contains_key(guild) {
        return error(404, 10068, "Unknown Form");
    }
    let me = world.me_id().to_owned();
    let Some(member) = world.guilds[index]["members"]
        .as_array_mut()
        .and_then(|l| l.iter_mut().find(|m| text(m, "user_id") == me))
    else {
        return error(404, 10007, "Unknown Member");
    };
    member["pending"] = false.into();
    let member = member.clone();
    let data = json!({
        "guild_id": guild, "user": world.me, "roles": member["roles"], "nick": member["nick"],
        "joined_at": member["joined_at"], "pending": false, "flags": 0,
    });
    done(
        Response::json(
            200,
            &json!({"application_status": "APPROVED", "guild_id": guild}),
        ),
        vec![event("GUILD_MEMBER_UPDATE", data)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn world() -> World {
        World::load(crate::DEFAULT_WORLD).unwrap()
    }

    fn call(world: &mut World, method: &str, target: &str, body: Option<Value>) -> Outcome {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_owned(), "fake-token".to_owned());
        handle(
            world,
            &Request {
                method: method.to_owned(),
                path: format!("{PREFIX}{path}"),
                query: query.to_owned(),
                headers,
                body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
            },
            &Ctx { port: 1 },
        )
    }

    fn body(outcome: &Outcome) -> Value {
        outcome.response.json_body().unwrap()
    }

    #[test]
    fn identity_and_authorization() {
        let mut w = world();
        let me = call(&mut w, "GET", "/users/@me", None);
        assert_eq!(body(&me)["id"], "1001");
        let mut bare = Request {
            method: "GET".into(),
            path: format!("{PREFIX}/users/@me"),
            query: String::new(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        };
        assert_eq!(handle(&mut w, &bare, &Ctx { port: 1 }).response.status, 401);
        bare.path = format!("{PREFIX}/gateway");
        assert_eq!(
            body(&handle(&mut w, &bare, &Ctx { port: 77 }))["url"],
            "ws://127.0.0.1:77"
        );
    }

    #[test]
    fn history_pages_newest_first_with_before_after_around() {
        let mut w = world();
        let page = body(&call(
            &mut w,
            "GET",
            "/channels/6005/messages?limit=10",
            None,
        ));
        let ids: Vec<u64> = page
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(ids.len(), 10);
        assert!(ids.windows(2).all(|p| p[0] > p[1]), "newest first");
        let oldest = ids[9];
        let before = body(&call(
            &mut w,
            "GET",
            &format!("/channels/6005/messages?limit=5&before={oldest}"),
            None,
        ));
        let first: u64 = before[0]["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(before.as_array().unwrap().len(), 5);
        assert!(first < oldest);
        let after = body(&call(
            &mut w,
            "GET",
            &format!("/channels/6005/messages?limit=3&after={oldest}"),
            None,
        ));
        let last: u64 = after[2]["id"].as_str().unwrap().parse().unwrap();
        assert!(last > oldest && last < ids[0]);
        let around = body(&call(
            &mut w,
            "GET",
            &format!("/channels/6005/messages?limit=6&around={oldest}"),
            None,
        ));
        assert_eq!(around.as_array().unwrap().len(), 6);
        assert!(
            around
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"].as_str().and_then(|s| s.parse::<u64>().ok()) == Some(oldest))
        );
    }

    #[test]
    fn sending_stores_answers_and_emits_with_the_nonce() {
        let mut w = world();
        let sent = call(
            &mut w,
            "POST",
            "/channels/6005/messages",
            Some(json!({"content": "hi <@2001>", "nonce": "12345"})),
        );
        assert_eq!(sent.response.status, 200);
        assert_eq!(body(&sent)["nonce"], "12345");
        assert_eq!(sent.events[0].name, "MESSAGE_CREATE");
        assert_eq!(sent.events[0].data["nonce"], "12345");
        assert_eq!(sent.events[0].data["guild_id"], "5001");
        let id = body(&sent)["id"].as_str().unwrap().to_owned();
        let page = body(&call(
            &mut w,
            "GET",
            "/channels/6005/messages?limit=1",
            None,
        ));
        assert_eq!(page[0]["id"], id.as_str());
        let empty = call(
            &mut w,
            "POST",
            "/channels/6005/messages",
            Some(json!({"content": " "})),
        );
        assert_eq!(empty.response.status, 400);
    }

    #[test]
    fn editing_deleting_and_other_peoples_messages() {
        let mut w = world();
        let mine = body(&call(
            &mut w,
            "POST",
            "/channels/6005/messages",
            Some(json!({"content": "one"})),
        ));
        let id = mine["id"].as_str().unwrap();
        let edited = call(
            &mut w,
            "PATCH",
            &format!("/channels/6005/messages/{id}"),
            Some(json!({"content": "two"})),
        );
        assert_eq!(body(&edited)["content"], "two");
        assert_eq!(edited.events[0].name, "MESSAGE_UPDATE");
        assert!(body(&edited)["edited_timestamp"].is_string());
        let gone = call(
            &mut w,
            "DELETE",
            &format!("/channels/6005/messages/{id}"),
            None,
        );
        assert_eq!(gone.response.status, 204);
        assert_eq!(gone.events[0].name, "MESSAGE_DELETE");
        assert_eq!(gone.events[0].data["guild_id"], "5001");
        assert_eq!(
            call(
                &mut w,
                "DELETE",
                &format!("/channels/6005/messages/{id}"),
                None
            )
            .response
            .status,
            404
        );
        // In the club, others' messages cannot be edited, or deleted without the permission.
        let club = w.messages["6101"][0]["id"].as_str().unwrap().to_owned();
        let patch = call(
            &mut w,
            "PATCH",
            &format!("/channels/6101/messages/{club}"),
            Some(json!({"content": "x"})),
        );
        assert_eq!(patch.response.status, 403);
        let del = call(
            &mut w,
            "DELETE",
            &format!("/channels/6101/messages/{club}"),
            None,
        );
        assert_eq!(del.response.status, 403);
    }

    #[test]
    fn reactions_add_remove_and_stay_idempotent() {
        let mut w = world();
        let id = w.messages["6005"][0]["id"].as_str().unwrap().to_owned();
        let url = format!("/channels/6005/messages/{id}/reactions/%F0%9F%94%A5/@me");
        let added = call(&mut w, "PUT", &url, None);
        assert_eq!(added.response.status, 204);
        assert_eq!(added.events[0].name, "MESSAGE_REACTION_ADD");
        assert_eq!(added.events[0].data["emoji"]["name"], "\u{1F525}");
        assert!(
            call(&mut w, "PUT", &url, None).events.is_empty(),
            "already reacted"
        );
        let typed = format!("/channels/6005/messages/{id}/reactions/%F0%9F%94%A5/0/@me");
        let removed = call(&mut w, "DELETE", &typed, None);
        assert_eq!(removed.events[0].name, "MESSAGE_REACTION_REMOVE");
        assert!(w.messages["6005"][0].get("reactions").is_none());
    }

    #[test]
    fn pins_acks_and_typing() {
        let mut w = world();
        let id = w.messages["6007"][1]["id"].as_str().unwrap().to_owned();
        let pinned = call(&mut w, "PUT", &format!("/channels/6007/pins/{id}"), None);
        assert_eq!(pinned.response.status, 204);
        assert_eq!(pinned.events.len(), 2);
        let list = body(&call(
            &mut w,
            "GET",
            "/channels/6007/messages/pins?limit=50",
            None,
        ));
        let items = list["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["message"]["id"] == id.as_str()));
        let unpinned = call(&mut w, "DELETE", &format!("/channels/6007/pins/{id}"), None);
        assert_eq!(unpinned.response.status, 204);
        let ack = call(
            &mut w,
            "POST",
            &format!("/channels/6005/messages/{id}/ack"),
            Some(json!({"token": null})),
        );
        assert_eq!(ack.events[0].name, "MESSAGE_ACK");
        assert_eq!(
            call(&mut w, "POST", "/channels/6005/typing", None)
                .response
                .status,
            204
        );
        assert_eq!(
            call(&mut w, "POST", "/channels/9/typing", None)
                .response
                .status,
            404
        );
        // The member in the club cannot pin.
        let club = w.messages["6101"][0]["id"].as_str().unwrap().to_owned();
        assert_eq!(
            call(&mut w, "PUT", &format!("/channels/6101/pins/{club}"), None)
                .response
                .status,
            403
        );
    }

    #[test]
    fn channels_and_roles_are_managed_with_their_events() {
        let mut w = world();
        let created = call(
            &mut w,
            "POST",
            "/guilds/5001/channels",
            Some(json!({"name": "new-room", "type": 0, "parent_id": "6004"})),
        );
        assert_eq!(created.events[0].name, "CHANNEL_CREATE");
        let id = body(&created)["id"].as_str().unwrap().to_owned();
        let edited = call(
            &mut w,
            "PATCH",
            &format!("/channels/{id}"),
            Some(json!({"name": "renamed", "topic": "t"})),
        );
        assert_eq!(body(&edited)["name"], "renamed");
        assert_eq!(edited.events[0].name, "CHANNEL_UPDATE");
        let moved = call(
            &mut w,
            "PATCH",
            "/guilds/5001/channels",
            Some(json!([{"id": id, "position": 0, "parent_id": null}])),
        );
        assert_eq!(moved.response.status, 204);
        assert_eq!(moved.events[0].data["position"], 0);
        let removed = call(&mut w, "DELETE", &format!("/channels/{id}"), None);
        assert_eq!(removed.events[0].name, "CHANNEL_DELETE");
        assert!(w.channel(&id).is_none());

        let role = call(
            &mut w,
            "POST",
            "/guilds/5001/roles",
            Some(json!({"name": "Tester", "permissions": "1024", "color": 255})),
        );
        assert_eq!(role.events[0].name, "GUILD_ROLE_CREATE");
        let rid = body(&role)["id"].as_str().unwrap().to_owned();
        let patched = call(
            &mut w,
            "PATCH",
            &format!("/guilds/5001/roles/{rid}"),
            Some(json!({"name": "Tester 2"})),
        );
        assert_eq!(patched.events[0].data["role"]["name"], "Tester 2");
        let gone = call(&mut w, "DELETE", &format!("/guilds/5001/roles/{rid}"), None);
        assert_eq!(gone.events[0].data["role_id"], rid.as_str());
        // Without the permission, nothing changes.
        assert_eq!(
            call(
                &mut w,
                "POST",
                "/guilds/5002/channels",
                Some(json!({"name": "x"}))
            )
            .response
            .status,
            403
        );
        assert_eq!(
            call(&mut w, "POST", "/guilds/5002/roles", Some(json!({})))
                .response
                .status,
            403
        );
    }

    #[test]
    fn the_hidden_channel_refuses_reading_and_writing() {
        let mut w = world();
        assert_eq!(
            call(&mut w, "GET", "/channels/6103/messages", None)
                .response
                .status,
            403
        );
        assert_eq!(
            call(
                &mut w,
                "POST",
                "/channels/6103/messages",
                Some(json!({"content": "x"}))
            )
            .response
            .status,
            403
        );
    }

    #[test]
    fn invites_are_looked_up_created_and_revoked() {
        let mut w = world();
        assert_eq!(
            body(&call(
                &mut w,
                "GET",
                "/invites/fakeinvite?with_counts=true",
                None
            ))["code"],
            "fakeinvite"
        );
        assert_eq!(
            call(&mut w, "GET", "/invites/nope", None).response.status,
            404
        );
        let made = body(&call(
            &mut w,
            "POST",
            "/channels/6005/invites",
            Some(json!({"max_age": 3600})),
        ));
        let code = made["code"].as_str().unwrap().to_owned();
        assert_eq!(made["max_age"], 3600);
        let listed = body(&call(&mut w, "GET", "/guilds/5001/invites", None));
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(
            call(&mut w, "DELETE", &format!("/invites/{code}"), None)
                .response
                .status,
            200
        );
        assert_eq!(
            call(&mut w, "GET", &format!("/invites/{code}"), None)
                .response
                .status,
            404
        );
    }

    #[test]
    fn member_verification_flips_pending_with_a_member_update() {
        let mut w = world();
        let form = body(&call(
            &mut w,
            "GET",
            "/guilds/5003/member-verification?with_guild=false",
            None,
        ));
        assert_eq!(form["form_fields"][0]["field_type"], "TERMS");
        assert_eq!(
            call(&mut w, "GET", "/guilds/5001/member-verification", None)
                .response
                .status,
            404
        );
        let put = call(
            &mut w,
            "PUT",
            "/guilds/5003/requests/@me",
            Some(json!({"version": "x", "form_fields": []})),
        );
        assert_eq!(put.events[0].name, "GUILD_MEMBER_UPDATE");
        assert_eq!(put.events[0].data["pending"], false);
        assert!(
            w.base_permissions(2) & SEND_MESSAGES != 0,
            "no longer pending"
        );
    }

    #[test]
    fn scheduled_events_list_and_everything_else_is_not_implemented() {
        let mut w = world();
        let events = body(&call(
            &mut w,
            "GET",
            "/guilds/5001/scheduled-events?with_user_count=true",
            None,
        ));
        assert_eq!(events.as_array().unwrap().len(), 2);
        for (method, path) in [
            ("POST", "/auth/login"),
            ("GET", "/users/@me/guilds"),
            ("POST", "/interactions"),
            ("GET", "/guilds/5001/audit-logs"),
        ] {
            let outcome = call(&mut w, method, path, Some(json!({})));
            assert_eq!(outcome.response.status, 404, "{path}");
            assert!(outcome.unimplemented);
            assert_eq!(
                body(&outcome),
                json!({"message": "fake: not implemented", "code": 0})
            );
        }
    }
}
