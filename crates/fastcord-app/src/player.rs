//! A watched stream in a window of its own (#212).
//!
//! iced redraws every window for each message and has no sub-rectangle
//! redraw, so a picture drawn inside the main window redraws the whole client
//! per frame. A watched stream therefore never draws there: the call view
//! shows a placeholder tile, and the picture goes to a player window whose
//! video widget polls the session's latest lease (`playback::view_polled`)
//! instead of waking the application with a message.
//!
//! Closing a player window stops watching that stream (the same request as
//! "Stop watching"); there is no inline mode to return to. The windows are
//! never stored on their own: [`App::sync_players`] reconciles them with
//! `Controls::watched()` after every update, so leaving the call, moving
//! channels, switching account and logging out close them through the
//! existing teardown that empties that list.

use crate::i18n::{tr, trf};
use crate::*;
use iced::Size;
use iced::widget::{column, container, text};
use iced::window;

/// One open player window and the watch it shows.
pub(crate) struct Player {
    pub window: window::Id,
    pub session: Arc<playback::Session>,
}

impl App {
    /// What each window draws: the main window, or one stream's player.
    pub(crate) fn window_view(&self, id: window::Id) -> Element<'_, Message> {
        if id == self.main_window {
            return self.view();
        }
        let Some(player) = self.players.iter().find(|player| player.window == id) else {
            return container(text("")).into();
        };
        let session = &player.session;
        column![
            container(playback::view_polled::<Message>(session.clone()))
                .width(Length::Fill)
                .height(Length::Fill),
            container(column![
                text(session.description()).size(12),
                call_ui::stream_audio_controls(&session.key, session)
            ])
            .padding(6)
        ]
        .into()
    }

    /// The streamer's name for a player window, `fastcord` for the main one.
    pub(crate) fn window_title(&self, id: window::Id) -> String {
        let Some(player) = self.players.iter().find(|player| player.window == id) else {
            return window_title().to_owned();
        };
        let owner = call_layout::stream_owner(&player.session.key)
            .zip(self.phase.gateway())
            .map(|(owner, gateway)| gateway.controls.voice_controls().display_name(owner));
        match owner {
            Some(name) => format!("{} · {}", trf("{}'s screen", &[&name]), window_title()),
            None => format!("{} · {}", tr("A stream"), window_title()),
        }
    }

    /// Opens a window for each watched stream that has none and closes the
    /// windows of streams no longer watched.
    pub(crate) fn sync_players(&mut self) -> Task<Message> {
        let wanted = self
            .phase
            .gateway()
            .map(|gateway| gateway.controls.watched())
            .unwrap_or_default();
        if wanted.is_empty() && self.players.is_empty() {
            return Task::none();
        }
        let mut tasks = Vec::new();
        self.players.retain(|player| {
            let kept = wanted
                .iter()
                .any(|session| Arc::ptr_eq(session, &player.session));
            if !kept {
                tasks.push(window::close(player.window));
            }
            kept
        });
        for session in wanted {
            if self
                .players
                .iter()
                .any(|player| Arc::ptr_eq(&player.session, &session))
            {
                continue;
            }
            let (id, open) = window::open(window::Settings {
                size: Size::new(960.0, 600.0),
                min_size: Some(Size::new(320.0, 240.0)),
                icon: icons::window_icon(),
                // `Message::Window` decides: closing stops the watch.
                exit_on_close_request: false,
                ..window::Settings::default()
            });
            session.set_own_window(true);
            session.set_visible(true);
            self.players.push(Player {
                window: id,
                session,
            });
            tasks.push(open.discard());
        }
        Task::batch(tasks)
    }

    /// The close button of a player window: stop watching that stream.
    pub(crate) fn player_closed(&mut self, id: window::Id) -> Task<Message> {
        let Some(index) = self.players.iter().position(|player| player.window == id) else {
            return Task::none();
        };
        let player = self.players.remove(index);
        let stop = self.handle(Message::UnwatchStream(player.session.key.clone()));
        Task::batch([stop, window::close(id)])
    }
}
