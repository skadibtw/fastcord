//! Discord message links: `https://discord.com/channels/{guild|@me}/{channel}/{message}`.
//!
//! Copying builds one from IDs the account already holds. Opening one parses
//! the clicked URL (nothing is fetched to resolve it): a link into the open
//! channel is a plain jump, which reports a deleted or unavailable target; a
//! link into another channel asks for that channel through the normal
//! selection, which only opens channels the account can see.

use fastcord_model::Snowflake;

/// Hosts whose `/channels/…` links are Discord message links.
const HOSTS: [&str; 4] = [
    "discord.com",
    "ptb.discord.com",
    "canary.discord.com",
    "discordapp.com",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageLink {
    /// `None` for a direct or group conversation (`@me`).
    pub guild: Option<Snowflake>,
    pub channel: Snowflake,
    pub message: Snowflake,
}

/// The link to copy for a message; `guild` is `None` in a conversation.
pub fn format(guild: Option<Snowflake>, channel: Snowflake, message: Snowflake) -> String {
    match guild {
        Some(guild) => format!("https://discord.com/channels/{guild}/{channel}/{message}"),
        None => format!("https://discord.com/channels/@me/{channel}/{message}"),
    }
}

fn id(part: &str) -> Option<Snowflake> {
    // Decimal digits only (no sign), at most a u64.
    (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        .then(|| part.parse::<u64>().ok())
        .flatten()
        .filter(|&n| n != 0)
        .map(Snowflake)
}

/// The message a clicked URL names, if it is a message link. Query and
/// fragment are ignored; a channel link without a message is not one.
pub fn parse(url: &str) -> Option<MessageLink> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if !HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
        return None;
    }
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.trim_end_matches('/').split('/');
    if parts.next()? != "channels" {
        return None;
    }
    let guild = match parts.next()? {
        "@me" => None,
        part => Some(id(part)?),
    };
    let channel = id(parts.next()?)?;
    let message = id(parts.next()?)?;
    parts.next().is_none().then_some(MessageLink {
        guild,
        channel,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_round_trip_and_only_exact_message_links_parse() {
        let (g, c, m) = (Snowflake(1), Snowflake(2), Snowflake(3));
        let guild = format(Some(g), c, m);
        assert_eq!(guild, "https://discord.com/channels/1/2/3");
        assert_eq!(
            parse(&guild),
            Some(MessageLink {
                guild: Some(g),
                channel: c,
                message: m
            })
        );
        let direct = format(None, c, m);
        assert_eq!(parse(&direct).unwrap().guild, None);
        assert_eq!(
            parse("https://PTB.discord.com/channels/1/2/3/?x=1#y").map(|l| l.message),
            Some(m)
        );
        for bad in [
            "http://discord.com/channels/1/2/3",
            "https://discord.com/channels/1/2",
            "https://discord.com/channels/1/2/3/4",
            "https://discord.com/channels/x/2/3",
            "https://discord.com/channels/1/0/3",
            "https://discord.com/channels/+1/2/3",
            "https://discord.com/channels/1/2/99999999999999999999",
            "https://evil.example/channels/1/2/3",
            "https://discord.com.evil.example/channels/1/2/3",
            "https://discord.com/users/1/2/3",
            "",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
