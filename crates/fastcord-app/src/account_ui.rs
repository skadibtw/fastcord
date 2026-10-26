//! The interface state that belongs to one signed-in account: its panels,
//! dialogs and menus (#169). One struct, so leaving or entering an account
//! resets all of it in one place, and a new panel cannot be forgotten: the
//! reset destructures every field.

#[derive(Default)]
pub struct AccountUi {
    /// A popover menu of the main window.
    pub menu: Option<crate::Menu>,
    pub new_dm: crate::new_dm::State,
    /// Message requests replace the conversation in direct messages.
    pub requests_open: bool,
    /// The unverified-writes list in the chat settings is expanded.
    pub unverified_open: bool,
    /// The per-person volume sliders are open.
    pub voice_details: bool,
    pub search: crate::search::State,
    pub pins: crate::pins::State,
    pub inbox: crate::inbox::State,
    pub notify_settings: crate::notify_settings::State,
    pub group: crate::group_dm::State,
    pub invite: crate::invite::State,
    pub guild_admin: crate::guild_admin::State,
    pub announce: crate::announce::State,
    pub moderation: crate::moderation::State,
    pub apps: crate::apps::State,
    pub community: crate::community::State,
    pub assets: crate::assets::State,
    pub automod: crate::automod::State,
    pub discover: crate::discover::State,
    pub onboarding: crate::onboarding::State,
    pub events: crate::scheduled_events::State,
    pub identity: crate::profile_edit::State,
    pub safety: crate::safety::State,
    pub account_panel: crate::account_panel::State,
    pub report: crate::report::State,
    pub gif: crate::gif::State,
    pub sticker: crate::sticker::State,
    pub soundboard: crate::soundboard::State,
    pub stage: crate::stage::State,
    pub forward: crate::forward::State,
    pub poll: crate::poll_create::State,
    pub slash: crate::slash::State,
    pub voice: crate::voice_record::State,
    pub friends: crate::friends::State,
    pub member_menu: crate::member_menu::State,
    pub status: crate::status::State,
    pub threads: crate::threads::State,
    pub forum: crate::forum::State,
    /// The quick switcher and the shortcuts help.
    pub switcher: crate::switcher::State,
    /// A keyboard or switcher navigation is on its way: when the open
    /// conversation changes, focus returns to the composer.
    pub nav_pending: bool,
}

impl AccountUi {
    /// Forgets everything of the previous account. Panels with requests in
    /// flight move their serials on instead of restarting them, so a late
    /// answer for the old account is dropped.
    pub fn reset(&mut self) {
        let Self {
            menu,
            new_dm,
            requests_open,
            unverified_open,
            voice_details,
            search,
            pins,
            inbox,
            notify_settings,
            group,
            invite,
            guild_admin,
            announce,
            moderation,
            apps,
            community,
            assets,
            automod,
            discover,
            onboarding,
            events,
            identity,
            safety,
            account_panel,
            report,
            gif,
            sticker,
            soundboard,
            stage,
            forward,
            poll,
            slash,
            voice,
            friends,
            member_menu,
            status,
            threads,
            forum,
            switcher,
            nav_pending,
        } = self;
        *menu = None;
        *new_dm = crate::new_dm::State::default();
        *requests_open = false;
        *unverified_open = false;
        *voice_details = false;
        search.clear();
        pins.clear();
        inbox.clear();
        notify_settings.clear();
        group.clear();
        invite.clear();
        guild_admin.clear();
        announce.clear();
        moderation.clear();
        apps.clear();
        community.clear();
        assets.clear();
        automod.clear();
        discover.clear();
        onboarding.clear();
        events.clear();
        identity.clear();
        safety.clear();
        account_panel.clear();
        report.clear();
        gif.clear();
        sticker.clear();
        soundboard.reset();
        stage.reset();
        forward.clear();
        poll.clear();
        slash.reset();
        voice.reset();
        friends.reset();
        member_menu.reset();
        status.reset();
        threads.drop_all();
        forum.drop_all();
        *switcher = crate::switcher::State::default();
        *nav_pending = false;
    }
}
