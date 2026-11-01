//! On-demand channel browsing over held, permission-filtered channel metadata.
use super::*;

pub const BROWSE_PAGE: usize = 50;
pub const BROWSE_QUERY_CHARS: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseRow {
    pub id: Snowflake,
    pub name: String,
    pub kind: ChannelKind,
    pub stage: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseChannels {
    pub guild: Snowflake,
    pub query: String,
    /// Requested offset, used to reject stale query responses in the UI.
    pub requested: usize,
    pub window: Window<BrowseRow>,
}

impl Navigation {
    /// Does not apply local category folding or hide-muted preferences. It
    /// never obtains permissions from onboarding choices or performs REST I/O.
    pub fn browse_channels(
        &self,
        store: &Store,
        guild: Snowflake,
        query: &str,
        offset: usize,
    ) -> BrowseChannels {
        let query: String = query.chars().take(BROWSE_QUERY_CHARS).collect();
        let needle = query.trim().to_lowercase();
        let mut visible = store
            .guild(guild)
            .filter(|_| self.account == store.current_user().map(|user| user.id))
            .map_or_else(Vec::new, |guild| visible_channels(store, guild, None));
        visible.retain(|row| {
            row.kind != ChannelKind::Category
                && row.permissions.enables(row.kind)
                && row
                    .channel
                    .name
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&needle)
        });
        let total = visible.len();
        let start = offset.min(total.saturating_sub(1) / BROWSE_PAGE * BROWSE_PAGE);
        let rows = visible
            .into_iter()
            .skip(start)
            .take(BROWSE_PAGE)
            .map(|row| BrowseRow {
                id: row.channel.id,
                name: capped_name(row.channel.name.as_deref(), "Unnamed channel"),
                kind: row.kind,
                stage: row.channel.kind == ModelChannelKind::GuildStageVoice,
            })
            .collect();
        BrowseChannels {
            guild,
            query,
            requested: offset,
            window: Window {
                offset: start,
                total,
                rows,
            },
        }
    }
}
