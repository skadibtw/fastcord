"""Generates fixtures/fake/world.json, the synthetic world of `fastcord-fake`.

Everything is invented: names, ids, texts. Ids are small readable numbers
(users 1001-3099, guilds 5001.., channels 6xxx/8xxx, roles 7xxx); messages use
snowflakes below any id the server mints at run time. Re-run with:

    python fixtures/fake/gen_world.py > fixtures/fake/world.json
"""

import json
import sys

ME = "1001"
BASE_MSG = 1_549_000_000_000_000_000
STEP = 1_000_000_000
NAMES = (
    "Aster Birch Cedar Dune Ember Fable Gale Harbor Iris Jun Kite Lumen Moss Nova Onyx "
    "Pine Quill Rune Sage Tide Umber Vale Wren Xylo Yarrow Zephyr Alder Brook Cove"
).split()
TALK = [
    "Morning all, the build is green again.",
    "Has anyone looked at the voice settings page yet?",
    "I pushed the fix for the sidebar spacing.",
    "Lunch break, back in 30.",
    "That sounds right to me.",
    "Can we move the standup to 11?",
    "I reproduced it on the second try, logs attached in my notes.",
    "Nice catch! Thanks for flagging it.",
    "Let's keep the discussion in the thread so it stays tidy.",
    "The release checklist is updated.",
    "Does the new layout feel faster to you?",
    "I'll take the review after the meeting.",
    "Reminder: the freeze starts on Friday.",
    "Works on my machine, but I will try a clean profile.",
    "Great, that unblocks the next step.",
    "Quick poll: tabs or spaces? (Please do not answer.)",
    "Coffee is ready in the kitchen channel.",
    "The docs page has a typo in the second paragraph.",
    "Merged. Please pull before you continue.",
    "Anyone up for a quick call later?",
]

next_msg = [0]


def msg_id():
    next_msg[0] += 1
    return str(BASE_MSG + next_msg[0] * STEP)


def user(uid, name, bot=False):
    u = {
        "id": str(uid),
        "username": name.lower(),
        "global_name": name,
        "avatar": None,
        "discriminator": "0",
    }
    if bot:
        u["bot"] = True
    return u


me = user(ME, "Fake Me")
me["username"] = "fake_me"
me.update({"flags": 0, "mfa_enabled": False, "premium_type": 0})
people = [user(2001 + i, n) for i, n in enumerate(NAMES)]
bot = user(3001, "FakeBot", bot=True)
newcomer = user(2090, "Newcomer")
users = people + [bot, newcomer]
by_id = {u["id"]: u for u in users}
by_id[ME] = me

presence = {}
for i, p in enumerate(people):
    presence[p["id"]] = ["online", "idle", "dnd", "offline", "offline", "online"][i % 6]
presence[ME] = "online"
presence[bot["id"]] = "online"


def stamp(minutes):
    base = 20 * 24 * 60 + 8 * 60 + minutes  # 2026-10-20 08:00 plus minutes
    day, rest = divmod(base, 24 * 60)
    return "2026-10-%02dT%02d:%02d:00.000000+00:00" % (day + 1, rest // 60, rest % 60)


def message(channel, author, content, minute, **extra):
    m = {
        "id": msg_id(),
        "type": 0,
        "content": content,
        "channel_id": channel,
        "author": by_id[author],
        "attachments": [],
        "embeds": [],
        "mentions": [],
        "mention_roles": [],
        "pinned": False,
        "mention_everyone": False,
        "tts": False,
        "timestamp": stamp(minute),
        "edited_timestamp": None,
        "flags": 0,
        "components": [],
    }
    m.update(extra)
    return m


def chatter(channel, authors, count, start=0):
    out = []
    for i in range(count):
        author = authors[i % len(authors)]
        text = TALK[(i * 7 + len(channel)) % len(TALK)]
        out.append(message(channel, author, text, start + i * 7))
    return out


def reaction(name, count, me_flag=False):
    return {
        "emoji": {"id": None, "name": name},
        "count": count,
        "count_details": {"burst": 0, "normal": count},
        "burst_colors": [],
        "me_burst": False,
        "burst_me": False,
        "me": me_flag,
        "burst_count": 0,
    }


def channel(cid, kind, name, position, parent=None, topic=None, overwrites=None, **extra):
    c = {
        "id": cid,
        "type": kind,
        "name": name,
        "position": position,
        "parent_id": parent,
        "permission_overwrites": overwrites or [],
        "nsfw": False,
    }
    if topic:
        c["topic"] = topic
    if kind in (0, 5, 15):
        c["rate_limit_per_user"] = 0
    if kind in (2, 13):
        c["bitrate"] = 64000
        c["user_limit"] = 0
        c["rtc_region"] = None
    c.update(extra)
    return c


def role(rid, name, perms, position, color=0, hoist=False):
    return {
        "id": rid,
        "name": name,
        "permissions": str(perms),
        "position": position,
        "color": color,
        "hoist": hoist,
        "managed": False,
        "mentionable": name != "@everyone" and position < 5,
    }


def member(uid, roles, nick=None, day=1, pending=False):
    m = {
        "user_id": uid,
        "roles": roles,
        "nick": nick,
        "joined_at": "2026-%02d-10T09:00:00.000000+00:00" % (1 + day % 9),
        "flags": 0,
        "mute": False,
        "deaf": False,
    }
    if pending:
        m["pending"] = True
    return m


def guild(gid, name, owner, features, description, roles, channels, members, **extra):
    g = {
        "id": gid,
        "name": name,
        "icon": None,
        "owner_id": owner,
        "member_count": len(members),
        "large": False,
        "joined_at": "2026-01-10T09:00:00.000000+00:00",
        "features": features,
        "description": description,
        "verification_level": 1,
        "premium_tier": 0,
        "default_message_notifications": 1,
        "explicit_content_filter": 2,
        "mfa_level": 0,
        "afk_timeout": 300,
        "preferred_locale": "en-US",
        "roles": roles,
        "channels": channels,
        "emojis": [],
        "stickers": [],
        "stage_instances": [],
        "members": members,
    }
    g.update(extra)
    return g


messages = {}

# ---- Fake Admin Lab -------------------------------------------------------
G1 = "5001"
EVERYONE_DEFAULT = 1071698660929
R_ADMIN, R_MOD, R_BUILD, R_MEMBER, R_GUEST = "7001", "7002", "7003", "7004", "7005"
roles1 = [
    role(G1, "@everyone", EVERYONE_DEFAULT, 0),
    role(R_ADMIN, "Admin", 8, 5, 15158332, True),
    role(R_MOD, "Moderator", 2 | 4 | 8192 | (1 << 40), 4, 3447003, True),
    role(R_BUILD, "Builder", 16 | 8192, 3, 3066993, False),
    role(R_MEMBER, "Member", 0, 2, 0, False),
    role(R_GUEST, "Guest", 0, 1, 0, False),
]
C_INFO, C_RULES, C_NEWS = "6001", "6002", "6003"
C_TEXT, C_GENERAL, C_DEV, C_LAB = "6004", "6005", "6006", "6007"
C_VOICE_CAT, C_LOUNGE, C_STAGE, C_FORUM = "6008", "6009", "6010", "6011"
channels1 = [
    channel(C_INFO, 4, "Info", 0),
    channel(C_RULES, 0, "rules", 0, C_INFO, "Read these first."),
    channel(C_NEWS, 5, "announcements", 1, C_INFO, "News about the lab."),
    channel(C_TEXT, 4, "Text", 1),
    channel(C_GENERAL, 0, "general", 0, C_TEXT, "Everyday chat."),
    channel(C_DEV, 0, "dev-talk", 1, C_TEXT, "Build and review chatter."),
    channel(C_LAB, 0, "markdown-lab", 2, C_TEXT, "Formatting, mentions, embeds, components."),
    channel(C_VOICE_CAT, 4, "Voice", 2),
    channel(C_LOUNGE, 2, "Lounge", 0, C_VOICE_CAT),
    channel(C_STAGE, 13, "Stage Room", 1, C_VOICE_CAT),
    channel(C_FORUM, 15, "ideas", 3, C_TEXT, "Post an idea."),
]
role_cycle = [[R_MOD], [R_BUILD], [R_MEMBER], [R_MEMBER], [R_GUEST], []]
members1 = [member(ME, [R_ADMIN], "Fake Admin")]
for i, p in enumerate(people):
    members1.append(member(p["id"], role_cycle[i % 6], day=i))
others1 = [p["id"] for p in people]

messages[C_RULES] = [
    message(C_RULES, ME, "**Rule 1.** Be kind.\n**Rule 2.** No spam.\n**Rule 3.** Keep it on topic.", 0, pinned=True),
    message(C_RULES, ME, "Questions about the rules go to the moderators.", 3),
]
messages[C_NEWS] = [
    message(C_NEWS, ME, "Welcome to the Fake Admin Lab. Nothing here is real.", 10),
    message(C_NEWS, ME, "Maintenance window on Saturday (not real either).", 40, flags=0),
    message(C_NEWS, ME, "New channel: #markdown-lab.", 70),
]
messages[C_GENERAL] = chatter(C_GENERAL, others1[:8] + [ME], 60, 100)
messages[C_DEV] = chatter(C_DEV, others1[8:14] + [ME], 30, 200)

lab = []
lab.append(message(C_LAB, others1[0], "Formatting check: **bold**, *italic*, __underline__, ~~strike~~, `inline code`, ||spoiler||.", 300))
lab.append(message(C_LAB, others1[1], "```rust\nfn main() {\n    println!(\"hello from the fake world\");\n}\n```", 303))
lab.append(message(C_LAB, others1[2], "> A quoted line\nand a list:\n- one\n- two\n- three", 306))
m_men = message(
    C_LAB, others1[3],
    "Hey <@%s> and <@1001>, the <@&%s> role should look at <#%s>." % (others1[4], R_MOD, C_DEV), 309,
    mentions=[by_id[others1[4]], me], mention_roles=[R_MOD],
)
lab.append(m_men)
lab.append(message(
    C_LAB, others1[5], "Docs: https://example.com/fastcord", 312,
    embeds=[{
        "type": "link", "url": "https://example.com/fastcord", "title": "Example page",
        "description": "A synthetic link embed with no image.",
        "provider": {"name": "Example"},
    }],
))
first = message(C_LAB, others1[6], "Who wants to review the settings page?", 315, pinned=True)
lab.append(first)
reply = message(
    C_LAB, others1[7], "I can, after lunch.", 318, type=19,
    message_reference={"type": 0, "channel_id": C_LAB, "message_id": first["id"], "guild_id": G1},
    referenced_message=first,
)
lab.append(reply)
lab.append(message(
    C_LAB, others1[8], "Reactions on this one.", 321,
    reactions=[reaction("\U0001F44D", 3, True), reaction("❤️", 1), reaction("\U0001F602", 2)],
))
lab.append(message(C_LAB, others1[9], "Join the other lab: https://discord.gg/fakeinvite", 324))
lab.append(message(
    C_LAB, bot["id"], "Pick an action:", 327,
    components=[
        {"type": 1, "components": [
            {"type": 2, "style": 1, "label": "Confirm", "custom_id": "fake_confirm"},
            {"type": 2, "style": 4, "label": "Cancel", "custom_id": "fake_cancel"},
            {"type": 2, "style": 5, "label": "Docs", "url": "https://example.com/docs"},
        ]},
        {"type": 1, "components": [
            {"type": 3, "custom_id": "fake_select", "placeholder": "Choose a flavour", "min_values": 1,
             "max_values": 1, "options": [
                 {"label": "Alpha", "value": "a"}, {"label": "Beta", "value": "b"},
                 {"label": "Gamma", "value": "g"}]},
        ]},
    ],
    application_id="3001",
))
lab.append(message(C_LAB, ME, "My own message, editable and deletable.", 330))
lab.append(message(C_LAB, others1[10], "Last one for now, with a mention of <@1001>.", 333,
                   mentions=[me]))
messages[C_LAB] = lab

# ---- Fake Member Club -----------------------------------------------------
G2 = "5002"
roles2 = [role(G2, "@everyone", 68672, 0), role("7101", "Regular", 0, 1, 10181046)]
C2_LOUNGE, C2_HELP, C2_STAFF = "6101", "6102", "6103"
channels2 = [
    channel(C2_LOUNGE, 0, "lounge", 0, topic="Hang out."),
    channel(C2_HELP, 0, "help", 1, topic="Ask questions."),
    channel(C2_STAFF, 0, "staff-only", 2, topic="Hidden from you.",
            overwrites=[{"id": G2, "type": 0, "allow": "0", "deny": "1024"}]),
]
members2 = [member(ME, ["7101"], None, 2)]
for i, p in enumerate(people[:11]):
    members2.append(member(p["id"], [] if i % 2 else ["7101"], day=i + 3))
others2 = [p["id"] for p in people[:11]]
messages[C2_LOUNGE] = chatter(C2_LOUNGE, others2[:5] + [ME], 30, 400)
messages[C2_HELP] = chatter(C2_HELP, others2[5:9] + [ME], 15, 500)
messages[C2_STAFF] = chatter(C2_STAFF, others2[:3], 4, 600)

# ---- Fake Rules Gate ------------------------------------------------------
G3 = "5003"
roles3 = [role(G3, "@everyone", 68608, 0)]
C3_RULES, C3_WELCOME = "6201", "6202"
channels3 = [
    channel(C3_RULES, 0, "rules", 0, topic="Accept the rules to join in."),
    channel(C3_WELCOME, 0, "welcome", 1),
]
members3 = [member(ME, [], None, 4, pending=True)] + [
    member(p["id"], [], day=i) for i, p in enumerate(people[:5])
]
messages[C3_RULES] = [message(C3_RULES, people[0]["id"], "Welcome! Read the rules, then accept them.", 700)]
messages[C3_WELCOME] = chatter(C3_WELCOME, [p["id"] for p in people[:3]], 4, 710)

guilds = [
    guild(G1, "Fake Admin Lab", ME, ["COMMUNITY", "NEWS"], "A synthetic server you own.",
          roles1, channels1, members1, rules_channel_id=C_RULES, system_channel_id=C_GENERAL,
          public_updates_channel_id=C_NEWS, member_count=30),
    guild(G2, "Fake Member Club", people[0]["id"], [], "A server where you are an ordinary member.",
          roles2, channels2, members2, member_count=12),
    guild(G3, "Fake Rules Gate", people[1]["id"], ["COMMUNITY", "MEMBER_VERIFICATION_GATE_ENABLED"],
          "A server that wants you to accept its rules first.", roles3, channels3, members3,
          rules_channel_id=C3_RULES, member_count=6),
]
assert len(members1) == 30

# ---- DMs, friends ---------------------------------------------------------
D1, D2, D3 = "8001", "8002", "8003"
dm_authors1 = [people[0]["id"], ME]
dm_authors2 = [people[1]["id"], ME]
messages[D1] = chatter(D1, dm_authors1, 14, 800)
messages[D2] = chatter(D2, dm_authors2, 14, 900)
messages[D3] = chatter(D3, [people[1]["id"], people[2]["id"], ME], 15, 1000)
private_channels = [
    {"id": D1, "type": 1, "recipient_ids": [people[0]["id"]]},
    {"id": D2, "type": 1, "recipient_ids": [people[1]["id"]]},
    {"id": D3, "type": 3, "name": "Weekend plans", "owner_id": ME, "icon": None,
     "recipient_ids": [people[1]["id"], people[2]["id"]]},
]
relationships = [
    {"id": people[i]["id"], "type": 1, "user_id": people[i]["id"], "nickname": None,
     "since": "2026-02-01T00:00:00.000000+00:00"} for i in range(4)
] + [{"id": newcomer["id"], "type": 3, "user_id": newcomer["id"], "nickname": None,
      "since": "2026-10-19T00:00:00.000000+00:00"}]

# last_message_id on every conversation; unread state on a few
for g in guilds:
    for c in g["channels"]:
        if messages.get(c["id"]):
            c["last_message_id"] = messages[c["id"]][-1]["id"]
for c in private_channels:
    c["last_message_id"] = messages[c["id"]][-1]["id"]


def acked(cid, unread, mentions=0):
    ms = messages[cid]
    return {"id": cid, "last_acked_id": ms[len(ms) - 1 - unread]["id"], "mention_count": mentions}


read_state = [
    acked(C_GENERAL, 5), acked(C_DEV, 0), acked(C_LAB, 2, 1), acked(C_RULES, 0), acked(C_NEWS, 1),
    acked(C2_LOUNGE, 3), acked(C2_HELP, 0), acked(D1, 2), acked(D2, 0), acked(D3, 0),
]

scheduled = {
    G1: [
        {"id": "900", "guild_id": G1, "channel_id": C_LOUNGE, "creator_id": ME,
         "name": "Movie night", "description": "Bring snacks.",
         "scheduled_start_time": "2026-10-30T18:00:00.000000+00:00", "scheduled_end_time": None,
         "privacy_level": 2, "status": 1, "entity_type": 2, "entity_id": None,
         "entity_metadata": None, "user_count": 12},
        {"id": "901", "guild_id": G1, "channel_id": None, "creator_id": people[0]["id"],
         "name": "Meetup", "description": None,
         "scheduled_start_time": "2026-11-02T10:00:00.000000+00:00",
         "scheduled_end_time": "2026-11-02T12:00:00.000000+00:00",
         "privacy_level": 2, "status": 1, "entity_type": 3,
         "entity_metadata": {"location": "Town hall"}, "user_count": 3},
    ]
}

world = {
    "token": "fake-token",
    "user": me,
    "users": users,
    "presences": presence,
    "guilds": guilds,
    "private_channels": private_channels,
    "relationships": relationships,
    "read_state": read_state,
    "messages": messages,
    "scheduled_events": scheduled,
    "invites": {
        "fakeinvite": {
            "type": 0, "code": "fakeinvite", "expires_at": None,
            "guild": {"id": "5099", "name": "Fake Other Place", "icon": None, "features": ["COMMUNITY"]},
            "channel": {"id": "6999", "type": 0, "name": "front-door"},
            "approximate_member_count": 120, "approximate_presence_count": 34,
        }
    },
    "member_verification": {
        G3: {
            "version": "2030-01-01T00:00:00.000000+00:00",
            "description": "Welcome to the synthetic gate.",
            "form_fields": [{
                "field_type": "TERMS", "label": "Read and agree to the server rules",
                "required": True, "values": ["Be kind.", "No spam.", "Keep it on topic."],
            }],
        }
    },
    "chatter": {"guild_channel": C_GENERAL, "dm_channel": D1, "friend": people[0]["id"]},
}

total = sum(len(v) for v in messages.values())
print("messages:", total, file=sys.stderr)
sys.stdout.reconfigure(newline="\n")
json.dump(world, sys.stdout, indent=1)
print()
