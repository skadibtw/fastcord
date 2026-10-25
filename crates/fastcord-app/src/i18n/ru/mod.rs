//! Russian. One file per area of the interface; each holds
//! `(English, Russian)` rows. Plural rows hold three `|`-separated forms
//! (one, few, many: 1 / 2-4 / 5-20).

mod account;
mod conversation;
mod messages;
mod moderation;
mod common;
mod errors;
mod window;
mod server;
mod voice;

const PARTS: &[&[(&str, &str)]] = &[
    common::ENTRIES,
    window::ENTRIES,
    messages::ENTRIES,
    conversation::ENTRIES,
    server::ENTRIES,
    moderation::ENTRIES,
    account::ENTRIES,
    voice::ENTRIES,
    errors::ENTRIES,
];

pub fn entries() -> impl Iterator<Item = (&'static str, &'static str)> {
    PARTS.iter().flat_map(|part| part.iter().copied())
}
