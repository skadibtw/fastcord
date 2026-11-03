use super::*;
use fastcord_discord::state::navigation::{AdminChannel, AdminRole, EVERYONE_ROLE};
use fastcord_discord::{Refusal, StatusCode};
use fastcord_model::Permissions;

fn client() -> RestClient {
    RestClient::new(
        fastcord_net::NetworkContext::off(),
        fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
        fastcord_discord::gateway::ClientProperties::bundled(
            fastcord_discord::gateway::HostOs::Linux,
            "en-US",
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
            fastcord_discord::NetworkFailure::Timeout,
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
    assert!(state.is_rejected(&Event::Saved(
        state.serial,
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
    let target = state.emoji_target().unwrap();
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionEmoji(target, "🎲".into())),
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

fn emoji_nav() -> NavigationSnapshot {
    let mut nav = make_nav(admin(true, false));
    nav.admin
        .as_mut()
        .unwrap()
        .roles
        .extend((403..=405).map(|id| role(id, None)));
    nav.assets = Some(GuildAssets {
        id: Snowflake(1),
        name: "Home".into(),
        me: ME,
        can_create: false,
        can_manage: false,
        tier: None,
        emojis: vec![
            AssetEmoji {
                id: Snowflake(900),
                name: "stone".into(),
                animated: false,
                usable: true,
                creator: None,
                creator_name: None,
            },
            AssetEmoji {
                id: Snowflake(901),
                name: "wave".into(),
                animated: true,
                usable: true,
                creator: None,
                creator_name: None,
            },
        ],
    });
    nav
}

fn select_answer(state: &mut State, nav: &NavigationSnapshot, option: usize) -> EmojiTarget {
    step(state, nav, Event::Tab(Tab::Onboarding));
    step(state, nav, Event::Onboarding(OnbMsg::SelectPrompt(0)));
    step(state, nav, Event::Onboarding(OnbMsg::SelectOption(option)));
    state.emoji_target().unwrap()
}

#[test]
fn custom_selection_clear_unicode_and_discard_use_the_actual_editor() {
    let nav = emoji_nav();
    let mut state = open(&nav);
    let target = select_answer(&mut state, &nav, 0);
    assert_eq!(
        step(
            &mut state,
            &nav,
            Event::Onboarding(OnbMsg::OptionCustom(target, Snowflake(901)))
        ),
        0
    );
    let ed = onboarding_of(&state);
    assert_eq!(
        ed.doc.prompts[0].options[0].emoji,
        OnboardingEmoji::Custom {
            id: Snowflake(901),
            name: "wave".into(),
            animated: true
        }
    );
    let body = ed
        .doc
        .body(&ed.baseline, &Refs::of(nav.admin.as_ref().unwrap()).known())
        .unwrap();
    let emoji = &body["prompts"][0]["options"][0];
    assert_eq!(
        emoji["emoji"],
        serde_json::json!({"id":"901","name":"wave","animated":true})
    );
    assert_eq!(emoji["emoji_id"], "901");
    assert_eq!(emoji["emoji_animated"], true);
    step(&mut state, &nav, Event::Discard);
    assert_eq!(onboarding_of(&state).doc, onboarding_of(&state).baseline);
    let target = select_answer(&mut state, &nav, 1);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::ClearEmoji(target)),
    );
    let ed = onboarding_of(&state);
    let body = ed
        .doc
        .body(&ed.baseline, &Refs::of(nav.admin.as_ref().unwrap()).known())
        .unwrap();
    let cleared = &body["prompts"][0]["options"][1];
    assert!(
        cleared["emoji"].is_null()
            && cleared["emoji_id"].is_null()
            && cleared["emoji_name"].is_null()
    );
    assert_eq!(cleared["emoji_animated"], false);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionEmoji(target, "🌊".into())),
    );
    assert_eq!(
        onboarding_of(&state).doc.prompts[0].options[1].emoji,
        OnboardingEmoji::Unicode("🌊".into())
    );
}

#[test]
fn custom_actions_recheck_guild_option_generation_and_manage_server() {
    let mut nav = emoji_nav();
    let mut state = open(&nav);
    let target = select_answer(&mut state, &nav, 0);
    let before = onboarding_of(&state).doc.clone();
    nav.assets.as_mut().unwrap().id = Snowflake(2);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionCustom(target, Snowflake(901))),
    );
    assert_eq!(onboarding_of(&state).doc, before);
    nav.assets.as_mut().unwrap().id = Snowflake(1);
    nav.assets.as_mut().unwrap().emojis[1].usable = false;
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionCustom(target, Snowflake(901))),
    );
    assert_eq!(
        onboarding_of(&state).doc,
        before,
        "known unavailable/role-restricted emoji cannot be introduced"
    );
    assert!(
        emoji_choices(nav.assets.as_ref(), Snowflake(1), "", 0)
            .items
            .iter()
            .all(|item| item.0 != Snowflake(901))
    );
    nav.assets.as_mut().unwrap().emojis[1].usable = true;
    select_answer(&mut state, &nav, 1);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::ClearEmoji(target)),
    );
    assert_eq!(
        onboarding_of(&state).doc,
        before,
        "old answer control must not edit new answer"
    );
    let current = state.emoji_target().unwrap();
    nav.admin.as_mut().unwrap().can_manage_guild = false;
    for msg in [
        OnbMsg::OptionCustom(current, Snowflake(901)),
        OnbMsg::ClearEmoji(current),
        OnbMsg::OptionEmoji(current, "🙂".into()),
    ] {
        step(&mut state, &nav, Event::Onboarding(msg));
    }
    assert_eq!(onboarding_of(&state).doc, before);
    nav.admin.as_mut().unwrap().can_manage_guild = true;
    state.clear();
    step(&mut state, &nav, Event::Open(Snowflake(1)));
    let serial = state.serial;
    step(
        &mut state,
        &nav,
        Event::OnboardingLoaded(serial, Ok(onboarding())),
    );
    select_answer(&mut state, &nav, 1);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::ClearEmoji(current)),
    );
    assert_eq!(
        onboarding_of(&state).doc,
        before,
        "previous account's same IDs are stale"
    );
    step(&mut state, &nav, Event::Open(Snowflake(2)));
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionCustom(target, Snowflake(901))),
    );
    assert_eq!(state.page(), Some(Snowflake(2)));
}

#[test]
fn save_rejects_deleted_or_changed_new_emoji_but_preserves_unavailable_baseline() {
    let mut nav = emoji_nav();
    let mut state = open(&nav);
    let target = select_answer(&mut state, &nav, 0);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionCustom(target, Snowflake(901))),
    );
    nav.assets.as_mut().unwrap().emojis[1].name = "renamed".into();
    assert_eq!(step(&mut state, &nav, Event::Save), 0);
    assert_eq!(state.notice.as_deref(), Some(EMOJI_UNAVAILABLE));
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionCustom(target, Snowflake(901))),
    );
    assert_eq!(
        onboarding_of(&state).doc.prompts[0].options[0].emoji,
        OnboardingEmoji::Custom {
            id: Snowflake(901),
            name: "renamed".into(),
            animated: true
        }
    );
    nav.assets.as_mut().unwrap().emojis[1].usable = false;
    assert_eq!(
        step(&mut state, &nav, Event::Save),
        0,
        "newly chosen emoji became unavailable"
    );
    nav.assets = None;
    assert_eq!(step(&mut state, &nav, Event::Save), 0);
    assert!(state.busy.is_none());
    step(&mut state, &nav, Event::Discard);
    let target = select_answer(&mut state, &nav, 1);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionTitle("Changed title".into())),
    );
    assert!(onboarding_of(&state).custom_emojis_valid(Snowflake(1), None));
    nav.assets = emoji_nav().assets;
    nav.assets.as_mut().unwrap().emojis[0].usable = false;
    assert!(onboarding_of(&state).custom_emojis_valid(Snowflake(1), nav.assets.as_ref()));
    assert_eq!(
        step(&mut state, &nav, Event::Save),
        1,
        "unchanged baseline custom emoji may remain missing"
    );
    assert_eq!(state.busy, Some(Tab::Onboarding));
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::ClearEmoji(target)),
    );
    assert!(
        onboarding_of(&state).doc.prompts[0].options[1]
            .emoji
            .is_custom(),
        "in-flight form is locked"
    );
}

#[test]
fn custom_picker_is_paged_unicode_searchable_and_never_keeps_asset_copies() {
    let mut nav = emoji_nav();
    let assets = nav.assets.as_mut().unwrap();
    assets.emojis = (1..=600)
        .map(|id| AssetEmoji {
            id: Snowflake(id),
            name: format!("эмодзи-{id}"),
            animated: id % 2 == 0,
            usable: true,
            creator: None,
            creator_name: None,
        })
        .collect();
    let mut state = open(&nav);
    assert_eq!(state.wanted_assets(), None);
    let target = select_answer(&mut state, &nav, 0);
    assert_eq!(state.wanted_assets(), Some(Snowflake(1)));
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::EmojiPage(target, true)),
    );
    assert_eq!(onboarding_of(&state).emoji_page, 1);
    let page = emoji_choices(nav.assets.as_ref(), Snowflake(1), "", usize::MAX);
    assert_eq!((page.page, page.pages, page.items.len()), (20, 21, 20));
    assert!(page.items.iter().all(|item| item.0.0 <= 500));
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::EmojiQuery(target, " ЭМОДЗИ-500 ".into())),
    );
    assert_eq!(
        emoji_choices(
            nav.assets.as_ref(),
            Snowflake(1),
            &onboarding_of(&state).emoji_query,
            0
        )
        .items[0]
            .0,
        Snowflake(500)
    );
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::EmojiQuery(target, "🦀".repeat(10000))),
    );
    assert_eq!(
        onboarding_of(&state).emoji_query.chars().count(),
        EMOJI_QUERY_CHARS
    );
    assert!(onboarding_of(&state).emoji_query.len() <= 4 * EMOJI_QUERY_CHARS);
    step(&mut state, &nav, Event::Tab(Tab::Guide));
    assert_eq!(state.wanted_assets(), None);
    step(&mut state, &nav, Event::Close);
    assert_eq!(state.wanted_assets(), None);
    state.clear();
    assert!(state.panel.is_none());
}

#[test]
fn emoji_widget_emits_scoped_selection_and_clear_and_disables_after_permission_loss() {
    use crate::variable_list::harness::Ui;
    let nav = emoji_nav();
    let mut state = open(&nav);
    let target = select_answer(&mut state, &nav, 1);
    for edit in [true, false] {
        let ed = onboarding_of(&state);
        let option = &ed.doc.prompts[0].options[1];
        let mut ui = Ui::new(
            420.0,
            350.0,
            emoji_view(ed, option, nav.assets.as_ref(), edit, target),
        );
        ui.draw();
        let mut selected = false;
        let mut cleared = false;
        for y in (0..300).step_by(8) {
            for x in [20.0, 160.0, 360.0] {
                for event in ui.click(x, y as f32) {
                    match event {
                        Event::Onboarding(OnbMsg::OptionCustom(found, _)) => {
                            assert_eq!(found, target);
                            selected = true;
                        }
                        Event::Onboarding(OnbMsg::ClearEmoji(found)) => {
                            assert_eq!(found, target);
                            cleared = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        assert_eq!((selected, cleared), (edit, edit));
    }
}

#[tokio::test]
async fn selecting_custom_emoji_does_not_enable_the_community_write_group() {
    let nav = emoji_nav();
    let mut state = open(&nav);
    let target = select_answer(&mut state, &nav, 0);
    step(
        &mut state,
        &nav,
        Event::Onboarding(OnbMsg::OptionCustom(target, Snowflake(901))),
    );
    let ed = onboarding_of(&state);
    let refs = Refs::of(nav.admin.as_ref().unwrap());
    let result = client()
        .save_onboarding(Snowflake(1), &ed.doc, &ed.baseline, &refs.known())
        .await;
    assert!(matches!(
        result,
        Err(CommunityRequestError::Rest(RestError::WriteNotVerified(
            fastcord_discord::WriteGroup::CommunityConfig
        )))
    ));
}

#[derive(Default)]
struct ListGeometry {
    scrolls: Vec<(iced::Rectangle, iced::Rectangle)>,
    labels: Vec<(String, iced::Rectangle)>,
}

impl iced::advanced::widget::Operation for ListGeometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        _id: Option<&iced::advanced::widget::Id>,
        bounds: iced::Rectangle,
        content: iced::Rectangle,
        _translation: iced::Vector,
        _state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        self.scrolls.push((bounds, content));
    }
    fn text(
        &mut self,
        _id: Option<&iced::advanced::widget::Id>,
        bounds: iced::Rectangle,
        text: &str,
    ) {
        self.labels.push((text.to_owned(), bounds));
    }
}

#[test]
fn community_checkbox_lists_fill_real_form_and_reserve_space_beside_labels() {
    use crate::variable_list::harness::Ui;
    let mut admin = admin(true, true);
    admin.roles = (0..8)
        .map(|n| {
            let mut role = role(401 + n, None);
            role.name = ["Admin", "Moderator", "Builder"][n as usize % 3].into();
            role
        })
        .collect();
    admin.channels = (0..8)
        .map(|n| channel(201 + n, AdminKind::Text, "general", true))
        .collect();
    let ed = OnbEd::new(onboarding());
    for width in [260.0, 700.0] {
        let mut ui = Ui::new(
            width,
            900.0,
            option_view(
                &ed,
                &ed.doc.prompts[0].options[0],
                &admin,
                None,
                true,
                EmojiTarget {
                    serial: 0,
                    guild: admin.id,
                    prompt: ed.doc.prompts[0].id,
                    option: ed.doc.prompts[0].options[0].id,
                },
            ),
        );
        ui.draw();
        let mut geometry = ListGeometry::default();
        ui.inspect(&mut geometry);
        assert_eq!(geometry.scrolls.len(), 2);
        for (bounds, content) in &geometry.scrolls {
            assert!(
                (bounds.width - (width - 20.0)).abs() < 1.0,
                "list must use available answer width: {bounds:?}"
            );
            assert!(
                content.height > bounds.height,
                "fixture forces actual vertical scrolling"
            );
            assert!(
                content.x + content.width <= bounds.x + bounds.width - 15.0,
                "content must not live under scrollbar: {bounds:?} {content:?}"
            );
        }
        for (label, bounds) in &geometry.labels {
            let scroll = if ["Admin", "Moderator", "Builder"].contains(&label.as_str()) {
                Some(geometry.scrolls[0].0)
            } else if label == "#general" {
                Some(geometry.scrolls[1].0)
            } else {
                None
            };
            if let Some(scroll) = scroll {
                assert!(
                    bounds.x + bounds.width <= scroll.x + scroll.width - 15.0,
                    "{label} must remain clear of the scrollbar"
                );
            }
        }
        let mut defaults = Ui::new(width, 900.0, onboarding_view(&ed, &admin, None, true, 0));
        defaults.draw();
        let mut geometry = ListGeometry::default();
        defaults.inspect(&mut geometry);
        assert_eq!(geometry.scrolls.len(), 1);
        let (bounds, content) = geometry.scrolls[0];
        assert!(
            (bounds.width - width).abs() < 1.0,
            "default channels use form width"
        );
        assert!(content.height > bounds.height);
        assert!(content.x + content.width <= bounds.x + bounds.width - 15.0);
    }
}

#[test]
fn community_wrapped_checkbox_labels_keep_target_and_disabled_behavior() {
    use crate::variable_list::harness::Ui;
    let long_role = "Moderator for regional support and community questions";
    let long_channel = "regional-support-and-community-questions";
    for edit in [true, false] {
        for locked in [true, false] {
            let role_id = if locked { 499 } else { 401 };
            let mut admin = admin(true, true);
            let mut r = role(
                role_id,
                locked.then_some("This role is not below your highest role."),
            );
            r.name = long_role.into();
            // Embedded gutters only exist while content overflows. One wrapped
            // row can fit on a different platform font, legitimately leaving no
            // scrollbar. Keep the target first and force overflow independently
            // of its wrapping before asserting scrollbar clearance.
            admin.roles = std::iter::once(r)
                .chain((1000..1008).map(|id| role(id, None)))
                .collect();
            admin.channels = std::iter::once(channel(201, AdminKind::Text, long_channel, true))
                .chain((1200..1208).map(|id| channel(id, AdminKind::Text, "overflow", true)))
                .collect();
            let ed = OnbEd::new(onboarding());
            let mut ui = Ui::new(
                260.0,
                900.0,
                option_view(
                    &ed,
                    &ed.doc.prompts[0].options[0],
                    &admin,
                    None,
                    edit,
                    EmojiTarget {
                        serial: 0,
                        guild: admin.id,
                        prompt: ed.doc.prompts[0].id,
                        option: ed.doc.prompts[0].options[0].id,
                    },
                ),
            );
            ui.draw();
            let mut geometry = ListGeometry::default();
            ui.inspect(&mut geometry);
            let role_bounds = geometry
                .labels
                .iter()
                .find(|(label, _)| label.starts_with(long_role))
                .unwrap()
                .1;
            assert!(
                role_bounds.height > 30.0,
                "long role label wraps at narrow form width"
            );
            let (role_scroll, role_content) = geometry.scrolls[0];
            assert!(
                role_content.height > role_scroll.height,
                "role fixture must have a scrollbar: {role_content:?} {role_scroll:?}"
            );
            assert!(
                role_bounds.x + role_bounds.width <= role_scroll.x + role_scroll.width - 15.0,
                "wrapped role overlaps a real scrollbar: {role_bounds:?} {role_scroll:?}"
            );
            let events = ui.click(
                role_bounds.x + role_bounds.width - 2.0,
                role_bounds.y + 10.0,
            );
            if edit && !locked {
                assert!(
                    matches!(events.as_slice(), [Event::Onboarding(OnbMsg::ToggleRole(id))] if *id == Snowflake(401)),
                    "{events:?}"
                );
            } else {
                assert!(
                    events.is_empty(),
                    "disabled role cannot be changed through its label: {events:?}"
                );
            }
            let channel_bounds = geometry
                .labels
                .iter()
                .find(|(label, _)| label.starts_with('#'))
                .unwrap()
                .1;
            let (channel_scroll, channel_content) = geometry.scrolls[1];
            assert!(
                channel_content.height > channel_scroll.height,
                "channel fixture must have a scrollbar: {channel_content:?} {channel_scroll:?}"
            );
            assert!(
                channel_bounds.x + channel_bounds.width
                    <= channel_scroll.x + channel_scroll.width - 15.0,
                "wrapped channel overlaps a real scrollbar: {channel_bounds:?} {channel_scroll:?}"
            );
            let events = ui.click(
                channel_bounds.x + channel_bounds.width - 2.0,
                channel_bounds.y + 10.0,
            );
            if edit {
                assert!(
                    matches!(events.as_slice(), [Event::Onboarding(OnbMsg::ToggleChannel(id))] if *id == Snowflake(201)),
                    "{events:?}"
                );
            } else {
                assert!(events.is_empty());
            }
            let mut defaults = Ui::new(260.0, 900.0, onboarding_view(&ed, &admin, None, edit, 0));
            defaults.draw();
            let mut geometry = ListGeometry::default();
            defaults.inspect(&mut geometry);
            let bounds = geometry
                .labels
                .iter()
                .find(|(label, _)| label.starts_with('#'))
                .unwrap()
                .1;
            assert!(bounds.height > 30.0);
            let (default_scroll, default_content) = geometry.scrolls[0];
            assert!(
                default_content.height > default_scroll.height,
                "default-channel fixture must have a scrollbar: {default_content:?} {default_scroll:?}"
            );
            assert!(
                bounds.x + bounds.width <= default_scroll.x + default_scroll.width - 15.0,
                "wrapped default channel overlaps a real scrollbar: {bounds:?} {default_scroll:?}"
            );
            let events = defaults.click(bounds.x + bounds.width - 2.0, bounds.y + 10.0);
            if edit {
                assert!(
                    matches!(events.as_slice(), [Event::Onboarding(OnbMsg::ToggleDefault(id))] if *id == Snowflake(201)),
                    "{events:?}"
                );
            } else {
                assert!(events.is_empty());
            }
        }
    }
}
