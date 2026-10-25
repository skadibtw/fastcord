//! Settings, Advanced: "Test unverified actions (alt account only)". One
//! checkbox per `WriteGroup`. The state lives only in the current account's
//! `RestClient` (see `RestClient::set_write_group`), so it is never saved and
//! starts over on restart, logout and account switch. In fake mode every group
//! is already on.

use fastcord_discord::{RestClient, WriteGroup};
use iced::Element;
use iced::widget::{button, checkbox, column, row, text};

use crate::Message;
use crate::i18n::tr;

const WARNING: &str = "Writes that have not been checked on a live account are off, so nothing reaches Discord by accident. Turn a group on only on the alt account, one at a time, and report what happens. This resets when you restart, log out or switch accounts.";

/// Every group at once: the list's "Allow all" and "Turn all off" shortcut.
pub fn set_all(client: &RestClient, on: bool) {
    for &group in WriteGroup::ALL {
        client.set_write_group(group, on);
    }
}

/// Fake mode sends nothing to Discord, so every group is on there.
pub fn enable_all(client: &RestClient) {
    set_all(client, true);
}

/// Every group with its current state; all off when no account is signed in.
pub fn rows(client: Option<&RestClient>) -> Vec<(WriteGroup, bool)> {
    WriteGroup::ALL
        .iter()
        .map(|&group| (group, client.is_some_and(|c| c.write_group_enabled(group))))
        .collect()
}

pub fn view<'a>(client: Option<&RestClient>, open: bool) -> Element<'a, Message> {
    let mut list = column![
        text(tr("Advanced")).size(18),
        button(if open {
            tr("Hide: Test unverified actions (alt account only)")
        } else {
            tr("Show: Test unverified actions (alt account only)")
        })
        .on_press(Message::ToggleUnverifiedList),
    ]
    .spacing(8);
    if open {
        list = list.push(text(tr(WARNING)).size(12));
        if client.is_none() {
            list = list.push(text(tr("Sign in to change these.")).size(12));
        } else {
            list = list.push(
                row![
                    button(tr("Allow all")).on_press(Message::UnverifiedWriteAll(true)),
                    button(tr("Turn all off")).on_press(Message::UnverifiedWriteAll(false)),
                    text(tr("Every group at once; still this session only.")).size(11),
                ]
                .spacing(8)
                .align_y(iced::alignment::Vertical::Center),
            );
        }
        for (group, on) in rows(client) {
            list = list
                .push(
                    checkbox(on).label(tr(group.name())).on_toggle_maybe(
                        client
                            .is_some()
                            .then_some(move |on| Message::UnverifiedWrite(group, on)),
                    ),
                )
                .push(text(group.routes()).size(11));
        }
    }
    list.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> RestClient {
        crate::login::fixture_session().client.clone()
    }

    #[test]
    fn rows_follow_the_client_and_default_to_the_verified_groups() {
        assert!(rows(None).iter().all(|(_, on)| !on));
        let client = client();
        let rows_now = rows(Some(&client));
        assert_eq!(rows_now.len(), WriteGroup::ALL.len());
        assert!(rows_now.iter().all(|(g, on)| *on == g.default_on()));
        client.set_write_group(WriteGroup::Pins, true);
        assert!(rows(Some(&client)).contains(&(WriteGroup::Pins, true)));
    }

    #[test]
    fn a_new_client_after_logout_or_switch_starts_over() {
        let first = client();
        first.set_write_group(WriteGroup::Reports, true);
        first.set_write_group(WriteGroup::Typing, false);
        // Logout drops the session; the next sign-in builds a new client.
        let next = client();
        assert!(!next.write_group_enabled(WriteGroup::Reports));
        assert!(next.write_group_enabled(WriteGroup::Typing));
    }

    #[test]
    fn panels_show_the_refusal_text_not_a_generic_failure() {
        use fastcord_discord::{RestError, WRITE_NOT_VERIFIED};
        let error = RestError::WriteNotVerified(WriteGroup::Pins);
        assert!(WRITE_NOT_VERIFIED.starts_with("Not yet verified on a live account."));
        assert_eq!(crate::friends::failure(error), WRITE_NOT_VERIFIED);
        assert_eq!(crate::moderation::failure(error), WRITE_NOT_VERIFIED);
        assert_eq!(crate::guild_admin::describe_rest(error), WRITE_NOT_VERIFIED);
        assert_eq!(crate::invite::describe(error), WRITE_NOT_VERIFIED);
        assert_eq!(crate::announce::describe(error, "", ""), WRITE_NOT_VERIFIED);
        assert_eq!(crate::changes::edit_failure(error), WRITE_NOT_VERIFIED);
        assert_eq!(crate::changes::delete_failure(error), WRITE_NOT_VERIFIED);
        assert_eq!(
            crate::reactions::failure(crate::reactions::Action::Add, error),
            WRITE_NOT_VERIFIED
        );
        assert_eq!(
            crate::polls::failure(&crate::polls::Kind::End, error),
            WRITE_NOT_VERIFIED
        );
        assert_eq!(crate::interactions::failure(error), WRITE_NOT_VERIFIED);
    }

    #[test]
    fn fake_mode_enables_every_group() {
        let client = client();
        assert!(!client.write_group_enabled(WriteGroup::Reports));
        enable_all(&client);
        assert!(rows(Some(&client)).iter().all(|(_, on)| *on));
    }

    #[test]
    fn allow_all_turns_every_group_on_and_off_again() {
        let client = client();
        set_all(&client, true);
        assert!(rows(Some(&client)).iter().all(|(_, on)| *on));
        assert!(
            WriteGroup::ALL
                .iter()
                .all(|&g| client.write_group_enabled(g))
        );
        set_all(&client, false);
        assert!(rows(Some(&client)).iter().all(|(_, on)| !*on));
        assert!(
            WriteGroup::ALL
                .iter()
                .all(|&g| !client.write_group_enabled(g))
        );
    }

    #[test]
    fn the_open_list_builds_with_and_without_a_client() {
        let client = client();
        // The "Allow all" row needs a client; every state must still build.
        let _ = view(Some(&client), true);
        let _ = view(None, true);
        let _ = view(Some(&client), false);
    }
}
