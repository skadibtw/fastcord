use super::*;
use fastcord_discord::state::navigation::{AdminChannel, AdminRole, EVERYONE_ROLE};
use fastcord_discord::{Refusal, StatusCode};
use fastcord_model::Permissions;

fn client() -> RestClient {
    RestClient::new(
        fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
        fastcord_discord::gateway::ClientProperties::web(
            fastcord_discord::gateway::HostOs::Linux,
            "en-US",
            1,
        ),
    )
    .unwrap()
}

const ME: Snowflake = Snowflake(7);

fn channel(id: u64, kind: AdminKind, name: &str, everyone_view: bool) -> AdminChannel {
    AdminChannel {
        id: Snowflake(id),
        name: name.into(),
        kind,
        parent_id: None,
        position: 0,
        topic: String::new(),
        nsfw: false,
        slowmode: 0,
        user_limit: 0,
        bitrate: 0,
        rtc_region: None,
        video_quality: 1,
        can_manage: true,
        overwrites: Vec::new(),
        can_manage_permissions: true,
        mine: Permissions::NONE,
        synced: None,
        everyone_view,
    }
}

fn role(id: u64, lock: Option<&'static str>) -> AdminRole {
    AdminRole {
        id: Snowflake(id),
        name: format!("role-{id}"),
        color: 0,
        hoist: false,
        mentionable: false,
        permissions: Permissions::NONE,
        position: 1,
        managed: false,
        everyone: id == 1,
        edit_lock: lock,
        move_lock: lock,
        assign_lock: lock,
    }
}

fn admin(manage_guild: bool, community: bool) -> GuildAdmin {
    GuildAdmin {
        id: Snowflake(1),
        name: "Home".into(),
        has_icon: false,
        owner: false,
        community,
        description: String::new(),
        system_channel: None,
        afk_channel: None,
        afk_timeout: 0,
        notifications: 0,
        content_filter: 0,
        rules_channel: community.then_some(Snowflake(201)),
        public_updates_channel: community.then_some(Snowflake(202)),
        verification_level: 0,
        mfa_required: false,
        safety_alerts_channel: None,
        raid_alerts_disabled: false,
        features: vec!["NEWS".to_owned()],
        can_manage_guild: manage_guild,
        can_manage_channels: true,
        channels: vec![
            channel(201, AdminKind::Text, "rules", true),
            channel(202, AdminKind::Text, "news", true),
            channel(203, AdminKind::Text, "staff", false),
            channel(204, AdminKind::Voice, "lounge", true),
        ],
        people: vec![(Snowflake(500), "Nell".into())],
        can_manage_roles: true,
        roles: vec![
            role(401, None),
            role(402, Some("This role is not below your highest role.")),
            role(1, Some(EVERYONE_ROLE)),
        ],
        mine: Permissions::NONE,
        grant_all: false,
        top_position: 5,
    }
}

fn make_nav(admin: GuildAdmin) -> NavigationSnapshot {
    NavigationSnapshot {
        admin: Some(admin),
        ..NavigationSnapshot::default()
    }
}

fn screening() -> Screening {
    let all: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../fixtures/rest/member-verification-synthetic.json"
    ))
    .unwrap();
    Screening::from_json(all["terms"].to_string().as_bytes()).unwrap()
}

fn onboarding() -> OnboardingDoc {
    OnboardingDoc::from_json(
        include_bytes!("../../../../fixtures/rest/onboarding-synthetic.json").as_slice(),
    )
    .unwrap()
}

fn guide() -> GuideDoc {
    GuideDoc::from_json(
        include_bytes!("../../../../fixtures/rest/new-member-welcome-synthetic.json").as_slice(),
    )
    .unwrap()
}

fn step(state: &mut State, nav: &NavigationSnapshot, event: Event) -> usize {
    state.update(event, nav, &client(), ME).units()
}

fn open(nav: &NavigationSnapshot) -> State {
    let mut state = State::default();
    assert_eq!(step(&mut state, nav, Event::Open(Snowflake(1))), 3);
    let serial = state.serial;
    step(
        &mut state,
        nav,
        Event::RulesLoaded(serial, Ok(Some(screening()))),
    );
    step(
        &mut state,
        nav,
        Event::OnboardingLoaded(serial, Ok(onboarding())),
    );
    step(&mut state, nav, Event::GuideLoaded(serial, Ok(guide())));
    state
}

fn answer(state: &mut State, tab: Tab, result: Result<Saved, CommunityRequestError>) {
    let serial = state.serial;
    let nav = make_nav(admin(true, false));
    let _ = state.update(Event::Saved(serial, tab, result), &nav, &client(), ME);
}

fn failed(state: &mut State, tab: Tab, error: RestError) -> String {
    answer(state, tab, Err(CommunityRequestError::Rest(error)));
    state.notice.clone().unwrap()
}

fn onboarding_of(state: &State) -> &OnbEd {
    let Some(Slot::Ready(ed)) = state.panel.as_ref().map(|p| &p.onboarding) else {
        panic!("no onboarding")
    };
    ed
}

fn rules_of(state: &State) -> &RulesEd {
    let Some(Slot::Ready(ed)) = state.panel.as_ref().map(|p| &p.rules) else {
        panic!("no rules")
    };
    ed
}

#[test]
fn opening_loads_all_three_and_ignores_answers_of_an_earlier_open() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    let panel = state.panel.as_ref().unwrap();
    assert!(matches!(panel.rules, Slot::Ready(_)));
    assert!(matches!(panel.onboarding, Slot::Ready(_)));
    assert!(matches!(panel.guide, Slot::Ready(_)));
    assert_eq!(state.wanted(), Some(Snowflake(1)));
    // An answer for a panel that was closed and reopened is dropped.
    let old = state.serial;
    step(&mut state, &nav, Event::Close);
    step(&mut state, &nav, Event::Open(Snowflake(1)));
    step(&mut state, &nav, Event::RulesLoaded(old, Ok(None)));
    assert!(matches!(state.panel.as_ref().unwrap().rules, Slot::Loading));
    let serial = state.serial;
    step(
        &mut state,
        &nav,
        Event::OnboardingLoaded(serial, Err(RestError::PermissionDenied)),
    );
    assert!(matches!(
        state.panel.as_ref().unwrap().onboarding,
        Slot::Failed(_)
    ));
    state.clear();
    assert!(state.wanted().is_none());
}

#[test]
fn rules_edits_mark_dirty_discard_and_save_once() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    assert!(!state.dirty(Tab::Rules, nav.admin.as_ref()));
    assert_eq!(step(&mut state, &nav, Event::Save), 0);
    assert_eq!(state.notice.as_deref(), Some("Nothing to change."));
    step(&mut state, &nav, Event::Rules(RulesMsg::Add));
    step(
        &mut state,
        &nav,
        Event::Rules(RulesMsg::Rule(3, "Be nice.".into())),
    );
    step(&mut state, &nav, Event::Rules(RulesMsg::Enabled(false)));
    assert!(state.dirty(Tab::Rules, nav.admin.as_ref()));
    assert!(!state.dirty(Tab::Guide, nav.admin.as_ref()));
    step(&mut state, &nav, Event::Discard);
    assert!(!state.dirty(Tab::Rules, nav.admin.as_ref()));

    step(&mut state, &nav, Event::Rules(RulesMsg::Remove(0)));
    assert_eq!(step(&mut state, &nav, Event::Save), 1);
    assert_eq!(state.busy, Some(Tab::Rules));
    assert_eq!(
        step(&mut state, &nav, Event::Save),
        0,
        "no duplicate in flight"
    );
    assert_eq!(step(&mut state, &nav, Event::Rules(RulesMsg::Add)), 0);
    assert!(state.notice.as_deref().unwrap().contains("Wait"));
    // Discord's answer becomes the baseline.
    answer(&mut state, Tab::Rules, Ok(Saved::Rules(Some(screening()))));
    assert!(state.busy.is_none());
    assert!(!state.dirty(Tab::Rules, nav.admin.as_ref()));
    // An answer that cannot be read is loaded again, not guessed.
    step(&mut state, &nav, Event::Rules(RulesMsg::Remove(0)));
    step(&mut state, &nav, Event::Save);
    let serial = state.serial;
    let task = state.update(
        Event::Saved(serial, Tab::Rules, Ok(Saved::Rules(None))),
        &nav,
        &client(),
        ME,
    );
    assert_eq!(task.units(), 1);
    assert!(matches!(state.panel.as_ref().unwrap().rules, Slot::Loading));
}

#[test]
fn rules_are_bounded_in_the_form() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    for _ in 0..30 {
        step(&mut state, &nav, Event::Rules(RulesMsg::Add));
    }
    assert_eq!(rules_of(&state).edit.rules.len(), MAX_RULES_EDITED);
    step(
        &mut state,
        &nav,
        Event::Rules(RulesMsg::Rule(0, "x".repeat(1_000))),
    );
    assert!(rules_of(&state).edit.rules[0].chars().count() <= MAX_RULE_CHARS + 20);
}

#[test]
fn nothing_is_sent_without_manage_server() {
    let denied = make_nav(admin(false, false));
    let mut state = open(&denied);
    step(&mut state, &denied, Event::Rules(RulesMsg::Add));
    assert_eq!(step(&mut state, &denied, Event::Save), 0);
    assert_eq!(state.notice.as_deref(), Some(NEEDS_MANAGE_GUILD));
    assert!(state.busy.is_none());
    // Permission lost between the edit and the click is caught at the click.
    let granted = make_nav(admin(true, false));
    let mut state = open(&granted);
    step(&mut state, &granted, Event::Rules(RulesMsg::Add));
    assert_eq!(step(&mut state, &denied, Event::Save), 0);
    assert_eq!(step(&mut state, &denied, Event::ConfirmDisable), 0);
}

#[test]
fn failures_keep_the_form_and_use_discords_words() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    step(&mut state, &nav, Event::Rules(RulesMsg::Add));
    assert_eq!(step(&mut state, &nav, Event::Save), 1);
    let note = failed(&mut state, Tab::Rules, RestError::PermissionDenied);
    assert!(note.contains("refused"));
    assert!(
        state.dirty(Tab::Rules, nav.admin.as_ref()),
        "edits are kept"
    );
    step(&mut state, &nav, Event::Save);
    let note = failed(
        &mut state,
        Tab::Rules,
        RestError::Refused(Refusal::new(
            StatusCode::BAD_REQUEST,
            Some(50035),
            "Default channels need at least 7 channels",
        )),
    );
    assert!(note.contains("Default channels need at least 7 channels"));
    step(&mut state, &nav, Event::Save);
    assert!(
        failed(
            &mut state,
            Tab::Rules,
            RestError::Http(StatusCode::TOO_MANY_REQUESTS)
        )
        .contains("rate limiting")
    );
    step(&mut state, &nav, Event::Save);
    let note = failed(
        &mut state,
        Tab::Rules,
        RestError::Retryable(fastcord_discord::RetryableFailure::Network(
            fastcord_discord::NetworkFailure::Connection,
        )),
    );
    assert!(note.contains("may have been applied"));
    assert!(state.dirty(Tab::Rules, nav.admin.as_ref()));
    step(&mut state, &nav, Event::Save);
    answer(
        &mut state,
        Tab::Rules,
        Err(CommunityRequestError::Invalid(CommunityError::TooManyRules)),
    );
    assert!(state.notice.as_deref().unwrap().contains("rules"));
    assert!(State::is_rejected(&Event::Saved(
        0,
        Tab::Rules,
        Err(CommunityRequestError::Rest(
            RestError::AuthenticationRequired
        ))
    )));
}

#[test]
fn onboarding_prompts_are_added_reordered_and_removed_with_fresh_ids() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    step(&mut state, &nav, Event::Tab(Tab::Onboarding));
    let before = onboarding().prompts.len();
    step(&mut state, &nav, Event::Onboarding(OnbMsg::AddPrompt));
    step(&mut state, &nav, Event::Onboarding(OnbMsg::AddPrompt));
    let ed = onboarding_of(&state);
    assert_eq!(ed.doc.prompts.len(), before + 2);
    let (a, b) = (&ed.doc.prompts[before], &ed.doc.prompts[before + 1]);
    assert_ne!(a.id, b.id);
    assert_ne!(a.id, a.options[0].id);
    let first = ed.doc.prompts[0].id;
    assert!(state.dirty(Tab::Onboarding, nav.admin.as_ref()));
    // Reorder: the first prompt moves down twice.
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::MovePrompt(0, true)),
    );
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::MovePrompt(1, true)),
    );
    assert_eq!(onboarding_of(&state).doc.prompts[2].id, first);
    // Off the ends and out of range nothing moves.
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::MovePrompt(0, false)),
    );
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::RemovePrompt(99)),
    );
    step(&mut state, &nav, Event::Discard);
    assert!(!state.dirty(Tab::Onboarding, nav.admin.as_ref()));
    for _ in 0..30 {
        step(&mut state, &nav, Event::Onboarding(OnbMsg::AddPrompt));
    }
    assert_eq!(onboarding_of(&state).doc.prompts.len(), MAX_PROMPTS_EDITED);
}

#[test]
fn onboarding_options_edit_roles_channels_emoji_and_stay_bounded() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    step(&mut state, &nav, Event::Tab(Tab::Onboarding));
    step(&mut state, &nav, Event::Onboarding(OnbMsg::SelectPrompt(0)));
    step(&mut state, &nav, Event::Onboarding(OnbMsg::SelectOption(0)));
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::ToggleRole(Snowflake(401))),
    );
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::ToggleChannel(Snowflake(204))),
    );
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionEmoji("🎲".into())),
    );
    step(&mut state, &nav, Event::Onboarding(OnbMsg::Single(true)));
    let ed = onboarding_of(&state);
    let option = &ed.doc.prompts[0].options[0];
    assert!(
        !option.role_ids.contains(&Snowflake(401)),
        "it was set, now off"
    );
    assert!(option.channel_ids.contains(&Snowflake(204)));
    assert_eq!(option.emoji.text(), "🎲");
    assert!(ed.doc.prompts[0].single_select);
    for _ in 0..60 {
        step(&mut state, &nav, Event::Onboarding(OnbMsg::AddOption));
    }
    assert_eq!(
        onboarding_of(&state).doc.prompts[0].options.len(),
        MAX_OPTIONS_EDITED
    );
    assert_eq!(step(&mut state, &nav, Event::Save), 1);
    assert_eq!(step(&mut state, &nav, Event::Save), 0);
}

#[test]
fn references_offered_follow_the_account_and_everyone() {
    let refs = Refs::of(&admin(true, false));
    assert!(refs.channels.contains(&Snowflake(203)));
    assert!(refs.default_ok.contains(&Snowflake(201)));
    assert!(
        !refs.default_ok.contains(&Snowflake(203)),
        "@everyone cannot view it"
    );
    assert!(
        !refs.default_ok.contains(&Snowflake(204)),
        "voice is not a default channel"
    );
    assert_eq!(refs.assignable, vec![Snowflake(401)]);
    assert!(refs.roles.contains(&Snowflake(402)));
}

#[test]
fn the_guide_edits_are_bounded_and_saved_once() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    step(&mut state, &nav, Event::Tab(Tab::Guide));
    step(
        &mut state,
        &nav,
        Event::Guide(GuideMsg::Welcome("Hello".into())),
    );
    for _ in 0..10 {
        step(
            &mut state,
            &nav,
            Event::Guide(GuideMsg::Add(List::Actions, Snowflake(201))),
        );
        step(
            &mut state,
            &nav,
            Event::Guide(GuideMsg::Add(List::Resources, Snowflake(202))),
        );
    }
    let Some(Slot::Ready(ed)) = state.panel.as_ref().map(|p| &p.guide) else {
        panic!("no guide")
    };
    assert_eq!(ed.doc.actions.len(), MAX_NEW_MEMBER_ACTIONS);
    assert_eq!(ed.doc.resources.len(), MAX_RESOURCE_CHANNELS);
    step(
        &mut state,
        &nav,
        Event::Guide(GuideMsg::Title(List::Actions, 2, "Say hi".into())),
    );
    step(
        &mut state,
        &nav,
        Event::Guide(GuideMsg::Emoji(2, "👋".into())),
    );
    step(
        &mut state,
        &nav,
        Event::Guide(GuideMsg::Channel(List::Actions, 2, Snowflake(202))),
    );
    step(
        &mut state,
        &nav,
        Event::Guide(GuideMsg::Remove(List::Actions, 99)),
    );
    assert!(state.dirty(Tab::Guide, nav.admin.as_ref()));
    assert_eq!(step(&mut state, &nav, Event::Save), 1);
    assert_eq!(step(&mut state, &nav, Event::Save), 0);
    answer(&mut state, Tab::Guide, Ok(Saved::Guide(Some(guide()))));
    assert!(!state.dirty(Tab::Guide, nav.admin.as_ref()));
}

#[test]
fn community_needs_text_channels_and_a_confirm_to_turn_off() {
    let off = make_nav(admin(true, false));
    let mut state = open(&off);
    step(&mut state, &off, Event::Tab(Tab::Community));
    assert!(!state.dirty(Tab::Community, off.admin.as_ref()));
    step(&mut state, &off, Event::Community(CommMsg::Level(3)));
    assert!(state.dirty(Tab::Community, off.admin.as_ref()));
    // No channels yet.
    assert_eq!(step(&mut state, &off, Event::Save), 0);
    assert!(state.notice.as_deref().unwrap().contains("rules channel"));
    // A voice channel is not accepted.
    step(
        &mut state,
        &off,
        Event::Community(CommMsg::Rules(Snowflake(204))),
    );
    step(
        &mut state,
        &off,
        Event::Community(CommMsg::Updates(Snowflake(202))),
    );
    assert_eq!(step(&mut state, &off, Event::Save), 0);
    step(
        &mut state,
        &off,
        Event::Community(CommMsg::Rules(Snowflake(201))),
    );
    assert_eq!(step(&mut state, &off, Event::Save), 1);
    assert_eq!(
        step(&mut state, &off, Event::Save),
        0,
        "no duplicate in flight"
    );
    answer(&mut state, Tab::Community, Ok(Saved::Community));
    assert!(!state.dirty(Tab::Community, off.admin.as_ref()));

    // The Gateway's update turns the form into the "on" view; turning off
    // needs the Confirm.
    let on = make_nav(admin(true, true));
    state.sync(&on);
    assert!(!state.dirty(Tab::Community, on.admin.as_ref()));
    assert_eq!(
        step(&mut state, &on, Event::ConfirmDisable),
        0,
        "not asked yet"
    );
    step(&mut state, &on, Event::AskDisable);
    step(&mut state, &on, Event::Cancel);
    assert_eq!(step(&mut state, &on, Event::ConfirmDisable), 0);
    step(&mut state, &on, Event::AskDisable);
    assert_eq!(step(&mut state, &on, Event::ConfirmDisable), 1);
    assert_eq!(state.busy, Some(Tab::Community));
    answer(&mut state, Tab::Community, Ok(Saved::Community));
    let form = state.panel.as_ref().unwrap().community.as_ref().unwrap();
    assert!(!form.confirm_disable);
    // Already on: turning on does nothing.
    step(&mut state, &on, Event::Community(CommMsg::Level(2)));
    assert_eq!(step(&mut state, &on, Event::Save), 0);
}

#[test]
fn the_community_form_starts_over_when_the_server_changes() {
    let off = make_nav(admin(true, false));
    let mut state = open(&off);
    state.sync(&off);
    step(&mut state, &off, Event::Community(CommMsg::Level(4)));
    state.sync(&off);
    assert!(
        state
            .panel
            .as_ref()
            .unwrap()
            .community
            .as_ref()
            .unwrap()
            .touched
    );
    state.sync(&make_nav(admin(true, true)));
    let form = state.panel.as_ref().unwrap().community.as_ref().unwrap();
    assert!(!form.touched && form.seen.0);
}

#[test]
fn every_view_builds() {
    let nav = make_nav(admin(true, false));
    let mut state = open(&nav);
    for tab in Tab::ALL {
        step(&mut state, &nav, Event::Tab(tab));
        state.sync(&nav);
        step(&mut state, &nav, Event::Onboarding(OnbMsg::SelectPrompt(0)));
        step(&mut state, &nav, Event::Onboarding(OnbMsg::SelectOption(0)));
        assert!(view(&state, &nav).is_some());
    }
    assert!(view(&state, &make_nav(admin(false, true))).is_some());
}
