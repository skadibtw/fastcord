//! Authenticated Discord control-plane I/O. One [`RestClient`] belongs to one account.
//! All clones share its rate limits and permanent authentication-stop signal.
//! The main user [`Gateway`] connection lifecycle lives in [`gateway`]; the
//! bounded reducer state it feeds lives in [`state`]. Channel history is read
//! through [`history`], the bounded cache of message bodies the timeline
//! reads from is [`message_store`], and messages are created, never re-posted
//! by the transport, through [`RestClient::create_message`]. The account's own
//! messages are edited and deleted through [`RestClient::edit_message`] and
//! [`RestClient::delete_message`] (see [`is_own_message`]); reactions are added,
//! taken back, and listed through [`RestClient::add_reaction`],
//! [`RestClient::remove_own_reaction`], and [`RestClient::reaction_users`].

#[cfg(all(feature = "fake-discord", not(debug_assertions)))]
compile_error!("fake-discord is a debug-only test mode");

mod account_security;
mod ack;
mod announce;
mod apps;
mod assets;
mod attachment;
mod automod;
mod call;
mod clock;
mod community_admin;
mod edit;
mod error;
#[cfg(feature = "fake-discord")]
pub mod fake_origin;
pub mod gateway;
mod gif;
mod group_dm;
mod guild_admin;
pub mod history;
mod image_data;
mod inbox;
mod interaction;
mod invite;
mod member;
pub mod message_store;
mod moderation;
mod onboarding;
mod password_auth;
mod pins;
mod poll;
mod presence;
mod privacy;
mod private_channel;
mod profile_edit;
mod rate_limit;
mod reaction;
mod relationship;
mod remote_auth;
mod report;
mod rest;
mod role_admin;
mod route;
mod scheduled_event;
mod search;
mod secret;
mod send;
mod soundboard;
mod stage;
pub mod state;
mod sticker;
mod thread;
mod typing;
mod upload;
pub mod user_settings;
mod write_group;
mod ws;

pub use account_security::{
    AuthSession, MAX_SESSIONS, MFA_REQUIRED, SecurityOverview, is_mfa_required, logout_body,
};
pub use announce::{Follower, MAX_FOLLOWERS, parse_followers};
pub use apps::{
    AuthorizedApp, Integration, MAX_WEBHOOK_NAME, Webhook, WebhookToken, authorize_url,
    parse_client_id, parse_webhooks,
};
pub use assets::{
    EmojiImage, ExpressionLimits, MAX_EMOJI_NAME_CHARS, MAX_SOUND_BYTES, MAX_SOUND_MS,
    MAX_STICKER_BYTES, MAX_STICKER_EDIT_NAME_CHARS, MIN_EMOJI_NAME_CHARS, MIN_STICKER_NAME_CHARS,
    MIN_TAGS_CHARS, SoundFile, StickerFile, emoji_name, expression_limits,
};
pub use attachment::{RefreshedAttachmentUrl, decode_waveform};
pub use automod::{
    AutoModAction, AutoModChange, AutoModFeed, AutoModRequestError, AutoModRule, CREATABLE,
    MAX_ALLOW_KEYWORD, MAX_ALLOW_PRESET, MAX_BLOCK_MESSAGE_CHARS, MAX_EXEMPT_CHANNELS,
    MAX_EXEMPT_ROLES, MAX_KEYWORD_CHARS, MAX_KEYWORDS, MAX_MENTION_LIMIT, MAX_REGEX,
    MAX_REGEX_CHARS, MAX_RULE_NAME_CHARS, MAX_RULES, MAX_TIMEOUT_SECONDS, PRESETS, RuleError,
    TRIGGER_KEYWORD, TRIGGER_MENTION, TRIGGER_PRESET, TRIGGER_PROFILE, TRIGGER_SPAM,
    decode_change as decode_automod_change, has_keywords, trigger_limit, trigger_name,
};
pub use call::CallSignal;
pub use clock::{Clock, MonotonicClock};
pub use community_admin::{
    CommunityError, CommunityRequestError, Emoji as OnboardingEmoji, GuideDoc, GuideEntry, Known,
    MAX_ENTRY_DESCRIPTION_CHARS, MAX_ENTRY_TITLE_CHARS, MAX_NEW_MEMBER_ACTIONS,
    MAX_OPTION_DESCRIPTION_CHARS, MAX_OPTION_TITLE_CHARS, MAX_OPTIONS_EDITED,
    MAX_PROMPT_TITLE_CHARS, MAX_PROMPTS_EDITED, MAX_RESOURCE_CHANNELS, MAX_RULE_CHARS,
    MAX_RULES_DESCRIPTION_CHARS, MAX_RULES_EDITED, MAX_WELCOME_CHARS, ONBOARDING_MODES,
    OnboardingDoc, OptionDoc, PromptDoc, RulesEdit, VERIFICATION_LEVELS, disable_community_body,
    enable_community_body, fresh_id,
};
pub use edit::{EditedMessage, edit_response_update, is_own_message};
pub use error::{NetworkFailure, Refusal, RestError, RetryableFailure, WRITE_NOT_VERIFIED};
pub use gateway::{ConnectionState, Gateway, GatewayEvent};
pub use gif::{GIF_RESULTS, GifResult, MAX_GIF_QUERY_CHARS};
pub use group_dm::{
    GroupEdit, GroupError, GroupRequestError, IconImage, MAX_GROUP_NAME_CHARS, MAX_ICON_BYTES,
};
pub use guild_admin::{
    AFK_TIMEOUTS, AdminError, AdminRequestError, BASE_BITRATE, CONTENT_FILTERS, ChannelEdit,
    GuildEdit, GuildIcon, MAX_CHANNEL_NAME_CHARS, MAX_CHANNEL_TOPIC_CHARS,
    MAX_GUILD_DESCRIPTION_CHARS, MAX_GUILD_NAME_CHARS, MAX_PLACEMENTS, MAX_VOICE_USER_LIMIT,
    MIN_BITRATE, MIN_GUILD_NAME_CHARS, NOTIFICATION_LEVELS, NewChannel, Placement, SLOWMODE,
    VERIFICATION_CHOICES, VoiceRegion, bitrate_ceiling, needs_community, parse_regions,
};
pub use inbox::{
    MENTIONS_PAGE_SIZE, MentionFilter, MentionsPage, MuteFor, NOTIFICATION_WRITES_ENABLED,
    NotifySetting,
};
pub use interaction::{
    AppCommand, ArgError, CommandArg, CommandIndex, CommandOption, IndexScope, InteractionContext,
    InteractionModal, InteractionOutcome, InteractionRequest, MAX_COMMAND_TEXT_CHARS, MAX_COMMANDS,
    MAX_INDEX_BYTES, ModalInput, OPTION_ATTACHMENT, OPTION_BOOLEAN, OPTION_CHANNEL, OPTION_INTEGER,
    OPTION_MENTIONABLE, OPTION_NUMBER, OPTION_ROLE, OPTION_STRING, OPTION_SUB_COMMAND, OPTION_USER,
    OptionChoice, parse_index as parse_command_index, parse_modal as parse_interaction_modal,
    parse_outcome as parse_interaction_outcome,
};
pub use invite::{
    CREATE_AGES, CREATE_USES, CREATED_MAX_AGE_SECONDS, DISCOVERY_PAGE, DiscoveredGuild, Expiry,
    Invite, InviteScope, MAX_CODES_PER_MESSAGE, MAX_LISTED, ManagedInvite, created_code,
    extract_codes, owner_cannot_leave,
};
pub use member::{BAN_DELETE, MemberAction, MemberPermissions, TIMEOUTS};
pub use moderation::{
    AUDIT_FILTERS, AUDIT_PAGE, AuditEntry, AuditPage, AuditQuery, BANS_PAGE, BULK_MAX, BULK_MIN,
    BanEntry, BansPage, BulkError, BulkRequestError, action_name, bulk_deletable, cut_reason,
};
pub use onboarding::{
    ApplicationStatus, COMPLETED_ONBOARDING, Guide, GuideItem, Onboarding, OnboardingOption,
    OnboardingPrompt, STARTED_ONBOARDING, Screening, ScreeningField, onboarding_body,
    onboarding_due,
};
pub use password_auth::{
    CaptchaChallenge, CaptchaSolution, LoginRefusal, MAX_CREDENTIAL_BYTES, MfaChallenge,
    PasswordAuth, PasswordAuthError, PasswordLogin,
};
pub use pins::{PINS_PAGE_SIZE, PinnedMessage, PinsPage};
pub use poll::{POLL_MAX_HOURS, PollDraft, PollError};
pub use presence::StatusChange;
pub use privacy::{
    FriendSources, MAX_RESTRICTED_GUILDS, PREVIEW_MESSAGES, PrivacyEdit, PrivacyError, PrivacyLive,
    PrivacyRequestError, PrivacySettings,
};
pub use private_channel::{MAX_PRIVATE_RECIPIENTS, PrivateChannelError};
pub use profile_edit::{
    AccountEdit, AvatarImage, Connection, MAX_BIO_CHARS, MAX_DISPLAY_NAME_CHARS, MAX_NICK_CHARS,
    MAX_PRONOUNS_CHARS, ProfileError, ProfileRequestError, ProfileText, parse_connections,
};
pub use reaction::{ReactionEmoji, ReactionEmojiError, ReactionUser};
pub use relationship::{Profile, RelationshipAction, UsernameError, parse_profile, parse_username};
pub use remote_auth::{QrLink, RemoteAuth, RemoteAuthError, RemoteAuthEvent, RemoteUser};
pub use report::{
    MAX_CHOICES, MAX_DEPTH, MAX_MENU_NODES, MenuNode, NodeKind, ReportError, ReportMenu,
    ReportReceipt, ReportRequestError, ReportTarget,
};
pub use reqwest::{Method, StatusCode};
pub use rest::{Priority, RestClient, RestRequest, RestResponse};
pub use role_admin::{MAX_OVERWRITES, MAX_ROLE_COLOR, MAX_ROLE_NAME_CHARS, RoleEdit};
pub use route::{MajorParameter, Route, RouteKey};
pub use scheduled_event::{DraftError, EventAction, EventDraft, EventPermissions};
pub use search::{
    MAX_SEARCH_CHARS, MAX_SEARCH_OFFSET, SEARCH_PAGE_SIZE, SearchHas, SearchOutcome, SearchPage,
    SearchQuery, SearchScope,
};
pub use secret::UserToken;
pub use send::{
    ComposeError, ForwardSource, MAX_CONTENT_CHARS, MentionPolicy, Nonce, NonceGenerator,
    OutgoingMessage, ReplyTo, SendError, strip_tts,
};
pub use soundboard::{GUILD_SOUNDS, MAX_SOUND_NAME_CHARS, SoundboardSound};
pub use stage::{MAX_TOPIC_CHARS, StageAction, StagePermissions, iso8601, topic as stage_topic};
pub use sticker::{
    GUILD_STICKERS, GuildSticker, MAX_STICKER_DESCRIPTION_CHARS, MAX_STICKER_TAGS_CHARS,
};
pub use thread::{
    ARCHIVED_PAGE_SIZE, ArchivedThreads, ForumPost, MAX_THREAD_NAME_CHARS, PostError, PostInvalid,
    PostRules, ThreadError, thread_name,
};
pub use typing::{TYPING_LIFETIME, TypingSend, TypingTable};
pub use upload::{
    CHUNK as UPLOAD_CHUNK, MAX_FILENAME_CHARS, MAX_UPLOAD_BYTES, MAX_UPLOAD_FILES, UploadError,
    UploadFile, UploadProgress, UploadedFile, Uploader,
};
pub use write_group::WriteGroup;
