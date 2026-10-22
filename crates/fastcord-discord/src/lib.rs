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

mod ack;
mod attachment;
mod call;
mod clock;
mod edit;
mod error;
pub mod gateway;
mod gif;
mod group_dm;
mod guild_admin;
pub mod history;
mod image_data;
mod inbox;
mod invite;
mod member;
pub mod message_store;
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
mod ws;

pub use attachment::{RefreshedAttachmentUrl, decode_waveform};
pub use call::CallSignal;
pub use clock::{Clock, MonotonicClock};
pub use edit::{EditedMessage, edit_response_update, is_own_message};
pub use error::{NetworkFailure, Refusal, RestError, RetryableFailure};
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
    VoiceRegion, bitrate_ceiling, needs_community, parse_regions,
};
pub use inbox::{MENTIONS_PAGE_SIZE, MentionFilter, MentionsPage};
pub use invite::{
    CREATE_AGES, CREATE_USES, CREATED_MAX_AGE_SECONDS, DISCOVERY_PAGE, DiscoveredGuild, Expiry,
    Invite, InviteScope, MAX_CODES_PER_MESSAGE, MAX_LISTED, ManagedInvite, created_code,
    extract_codes, owner_cannot_leave,
};
pub use member::{MemberAction, MemberPermissions};
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
pub use route::{MajorParameter, Route, RouteKey};
pub use scheduled_event::{DraftError, EventAction, EventDraft, EventPermissions};
pub use search::{
    MAX_SEARCH_CHARS, MAX_SEARCH_OFFSET, SEARCH_PAGE_SIZE, SearchHas, SearchOutcome, SearchPage,
    SearchQuery, SearchScope,
};
pub use secret::UserToken;
pub use send::{
    ComposeError, ForwardSource, MAX_CONTENT_CHARS, MentionPolicy, Nonce, NonceGenerator,
    OutgoingMessage, ReplyTo, SendError,
};
pub use soundboard::{GUILD_SOUNDS, MAX_SOUND_NAME_CHARS, SoundboardSound};
pub use stage::{MAX_TOPIC_CHARS, StageAction, StagePermissions, iso8601, topic as stage_topic};
pub use sticker::{GUILD_STICKERS, GuildSticker};
pub use thread::{
    ARCHIVED_PAGE_SIZE, ArchivedThreads, ForumPost, MAX_THREAD_NAME_CHARS, PostError, PostInvalid,
    PostRules, ThreadError, thread_name,
};
pub use typing::{TYPING_LIFETIME, TypingSend, TypingTable};
pub use upload::{
    CHUNK as UPLOAD_CHUNK, MAX_FILENAME_CHARS, MAX_UPLOAD_BYTES, MAX_UPLOAD_FILES, UploadError,
    UploadFile, UploadProgress, UploadedFile, Uploader,
};
