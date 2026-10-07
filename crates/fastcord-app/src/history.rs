//! Account-worker history coordination: which page of the selected channel to
//! fetch, how its messages reach the layout index, and the bounded snapshot the
//! timeline view receives. Everything here runs in the account worker; `view`
//! only reads the snapshot. Nothing is fetched until a readable channel is
//! selected, and a page is requested only for a reason: opening the channel,
//! scrolling near the end of what is retained, an explicit button, or retry.
use std::pin::Pin;
use std::sync::Arc;

use fastcord_discord::gateway::Dispatch;
use fastcord_discord::history::HistoryCursor;
use fastcord_discord::message_store::{
    MAX_MESSAGE_BYTES, MessageStore, PageMerge, PageToken, message_bytes,
};
use fastcord_discord::state::navigation::{ChannelKind, NavigationSnapshot};
use fastcord_discord::{RestClient, RestError};
use fastcord_model::{Message, Snowflake};

use crate::timeline::{Row, Snapshot};
use crate::variable_list::{Item, Measurement, VariableList, Viewport};

/// Message bodies one snapshot may hold. Together with the latest slot and the
/// copy the UI displays this stays inside the 2 MiB UI-delta budget (SPEC §4.5).
pub const SNAPSHOT_BODY_BUDGET: usize = MAX_MESSAGE_BYTES;
/// Measured heights waiting for the worker; the widget reports at most one per
/// built row, so more than this are stale duplicates.
const MAX_MEASUREMENTS: usize = 128;

const OVERSIZE: &str = "A message is too large to display and was left out.";

type PageFuture = Pin<Box<dyn Future<Output = Result<Vec<Message>, RestError>> + Send>>;

/// An explicit user action from the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    Older(Snowflake),
    Latest(Snowflake),
    Retry(Snowflake),
}

/// The timeline's one-shot inputs to the worker, coalesced.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub revision: u64,
    pub intent: Option<Intent>,
    pub viewport: Option<(Snowflake, Viewport)>,
    pub measurements: Vec<(Snowflake, Measurement)>,
}

impl Request {
    /// Keeps the newest height per row and width bucket.
    pub fn measure(&mut self, channel: Snowflake, measurement: Measurement) {
        if let Some(slot) = self.measurements.iter_mut().find(|(id, old)| {
            *id == channel
                && old.id == measurement.id
                && old.width_bucket == measurement.width_bucket
        }) {
            *slot = (channel, measurement);
            return;
        }
        if self.measurements.len() == MAX_MEASUREMENTS {
            self.measurements.remove(0);
        }
        self.measurements.push((channel, measurement));
    }
}

#[derive(Default)]
pub struct History {
    store: MessageStore,
    list: VariableList,
    channel: Option<Snowflake>,
    /// Revision of the last explicit intent handled.
    revision: u64,
    error: Option<&'static str>,
    /// The page whose failure the Retry action repeats.
    failed: Option<HistoryCursor>,
    pending: Option<(PageToken, PageFuture)>,
    /// A page just landed: the list may still be short of the viewport.
    check_paging: bool,
    dirty: bool,
}

impl History {
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    fn cancel(&mut self) {
        if let Some((token, _)) = self.pending.take() {
            self.store.cancel_page(token);
        }
    }

    /// Releases everything: a new session (READY) or a closed channel.
    fn reset(&mut self) {
        self.cancel();
        self.store.clear();
        self.list = VariableList::default();
        self.channel = None;
        self.error = None;
        self.failed = None;
        self.check_paging = false;
        self.dirty = true;
    }

    /// Applies an ordered Gateway event before the metadata store sees it.
    pub fn apply(&mut self, event: &Dispatch) {
        if matches!(event, Dispatch::Ready(_)) {
            self.reset();
            return;
        }
        let change = self.store.apply_dispatch(event);
        if change.changed {
            self.dirty = true;
        }
        if change.rejected > 0 {
            self.error = Some(OVERSIZE);
            self.dirty = true;
        }
    }

    /// Mirrors the retained rows into the layout index and aims the store's
    /// eviction at what the reader is looking at.
    fn synchronize(&mut self) {
        let Some(channel) = self.channel else {
            return;
        };
        let items: Vec<Item> = self
            .store
            .items(channel)
            .map(|(id, revision)| Item { id, revision })
            .collect();
        self.dirty |= self.list.set_items(&items);
        let live = self
            .store
            .window(channel)
            .is_none_or(|window| window.newer.is_none());
        self.list.set_live(live);
        self.focus();
    }

    /// `None` is the live edge: the newest rows are the ones to keep.
    fn focus(&mut self) {
        let Some(channel) = self.channel else {
            return;
        };
        let focus = if self.list.window().pinned {
            None
        } else {
            self.list.anchor().map(|anchor| anchor.id)
        };
        self.store.set_focus(channel, focus);
    }

    fn start(&mut self, rest: &RestClient, cursor: HistoryCursor) {
        let Some(channel) = self.channel else {
            return;
        };
        if self.pending.is_some() {
            return;
        }
        self.focus();
        let token = self.store.begin_page(channel, cursor);
        let rest = rest.clone();
        self.pending = Some((
            token,
            Box::pin(async move { rest.channel_messages(channel, cursor).await }),
        ));
        self.error = None;
        self.failed = None;
        self.dirty = true;
    }

    /// Brings history in line with the validated selection and applies the
    /// timeline's one-shot inputs. Permission loss or deselection closes it.
    pub fn refresh(
        &mut self,
        rest: &RestClient,
        navigation: &NavigationSnapshot,
        request: &Request,
    ) {
        let selected = navigation
            .selection
            .as_ref()
            .filter(|selection| {
                matches!(
                    selection.kind,
                    ChannelKind::Text | ChannelKind::Announcement
                ) && selection.permissions.read_history
            })
            .map(|selection| selection.channel_id);
        if selected != self.channel {
            // Revisits fetch a fresh page instead of keeping hidden histories.
            self.reset();
            self.channel = selected;
            if selected.is_some() {
                self.start(rest, HistoryCursor::Latest);
            }
        }
        self.synchronize();
        if let Some((channel, viewport)) = request.viewport
            && Some(channel) == self.channel
        {
            self.dirty |= self.list.viewport(viewport);
            self.focus();
        }
        for (channel, measurement) in &request.measurements {
            if Some(*channel) == self.channel {
                self.dirty |= self.list.measure(*measurement);
            }
        }
        if request.revision != self.revision {
            self.revision = request.revision;
            self.intent(rest, request.intent);
        }
        if request.viewport.is_some() || self.check_paging {
            self.page_for_position(rest);
        }
    }

    fn intent(&mut self, rest: &RestClient, intent: Option<Intent>) {
        let Some(channel) = self.channel else {
            return;
        };
        match intent {
            Some(Intent::Older(target)) if target == channel => {
                if let Some(cursor) = self.store.window(channel).and_then(|w| w.older) {
                    self.start(rest, cursor);
                }
            }
            Some(Intent::Latest(target)) if target == channel => {
                let live = self
                    .store
                    .window(channel)
                    .is_some_and(|window| window.newer.is_none());
                if live {
                    self.list.jump_latest();
                    self.dirty = true;
                } else {
                    self.cancel();
                    self.start(rest, HistoryCursor::Latest);
                }
            }
            Some(Intent::Retry(target)) if target == channel => {
                self.error = None;
                self.dirty = true;
                if let Some(cursor) = self.failed.take() {
                    self.start(rest, cursor);
                }
            }
            _ => {}
        }
    }

    /// Scrolling toward the end of the retained rows asks for the adjacent
    /// page. Opening a server or channel list never reaches here.
    fn page_for_position(&mut self, rest: &RestClient) {
        self.check_paging = false;
        let Some(channel) = self.channel else {
            return;
        };
        if self.pending.is_some() || self.failed.is_some() {
            return;
        }
        let Some(window) = self.store.window(channel) else {
            return;
        };
        let position = self.list.window();
        if position.offset <= position.height
            && let Some(cursor) = window.older
        {
            self.start(rest, cursor);
        } else if position.at_bottom
            && let Some(cursor) = window.newer
        {
            self.start(rest, cursor);
        }
    }

    /// Resolves when the in-flight page does; never resolves with none.
    pub async fn next_page(&mut self) -> Result<Vec<Message>, RestError> {
        match &mut self.pending {
            Some((_, future)) => future.await,
            None => std::future::pending().await,
        }
    }

    /// Applies a finished page. Returns whether Discord rejected the login.
    pub fn finish(&mut self, result: Result<Vec<Message>, RestError>) -> bool {
        let Some((token, _)) = self.pending.take() else {
            return false;
        };
        let cursor = token.cursor();
        self.dirty = true;
        match result {
            Ok(page) => match self.store.merge_page(token, page) {
                PageMerge::Applied { rejected, .. } => {
                    if rejected > 0 {
                        self.error = Some(OVERSIZE);
                    }
                    self.synchronize();
                    if cursor == HistoryCursor::Latest {
                        self.list.jump_latest();
                    }
                    self.check_paging = true;
                    false
                }
                PageMerge::Stale => {
                    // Nothing changed; the same request is still available.
                    self.failed = Some(cursor);
                    self.error =
                        Some("The channel changed while loading. Retry to load a current page.");
                    false
                }
            },
            Err(error) => {
                self.store.cancel_page(token);
                self.failed = Some(cursor);
                self.error = Some(match error {
                    RestError::PermissionDenied => {
                        "Discord denied access to this history. Check your permissions here."
                    }
                    RestError::ResourceGone => "This channel is no longer available.",
                    RestError::AuthenticationRequired => "Discord no longer accepts this login.",
                    RestError::ResponseBodyTooLarge | RestError::InvalidJson => {
                        "Discord sent a history page fastcord could not accept."
                    }
                    _ => "Could not load messages. Retry when the connection is back.",
                });
                error == RestError::AuthenticationRequired
            }
        }
    }

    /// The bounded read model: only the built window's messages, trimmed to
    /// the byte budget, with spacers recomputed for whatever was trimmed.
    pub fn snapshot(&mut self) -> Snapshot {
        self.dirty = false;
        let Some(channel) = self.channel else {
            return Snapshot::default();
        };
        let mut window = self.list.window();
        let ids: Vec<Snowflake> = self
            .store
            .ids(channel)
            .skip(window.range.start)
            .take(window.range.len())
            .collect();
        let mut messages: Vec<(Arc<Message>, usize)> = ids
            .iter()
            .filter_map(|id| self.store.get(channel, *id))
            .map(|message| {
                let cost = message_bytes(&message);
                (message, cost)
            })
            .collect();
        if messages.len() != ids.len() {
            // The index and the store disagree for an instant (an event not
            // yet synchronized); show nothing rather than misplace rows.
            messages.clear();
            window = self.list.window_for_range(0..0);
        }
        let total: usize = messages.iter().map(|(_, cost)| cost).sum();
        if total > SNAPSHOT_BODY_BUDGET {
            // Keep the visible rows, then what fits after them.
            let first = self
                .list
                .index_at(window.offset)
                .clamp(window.range.start, window.range.end.saturating_sub(1));
            messages.drain(..first - window.range.start);
            let mut used = 0;
            let mut keep = 0;
            for (_, cost) in &messages {
                if keep > 0 && used + cost > SNAPSHOT_BODY_BUDGET {
                    break;
                }
                used += cost;
                keep += 1;
            }
            messages.truncate(keep);
            window = self.list.window_for_range(first..first + keep);
        }
        let rows = messages
            .into_iter()
            .map(|(message, _)| {
                let revision = self.store.revision(channel, message.id).unwrap_or(0);
                Row { message, revision }
            })
            .collect();
        let bounds = self.store.window(channel);
        Snapshot {
            channel_id: Some(channel),
            rows,
            window,
            scroll: self.list.scroll_request(),
            loading: self.pending.is_some(),
            has_older: bounds.is_some_and(|window| window.older.is_some()),
            has_newer: bounds.is_some_and(|window| window.newer.is_some()),
            error: self.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use fastcord_discord::UserToken;
    use fastcord_discord::gateway::{MessageDelete, MessageDeleteBulk};
    use fastcord_discord::message_store::{MAX_MESSAGES_PER_CHANNEL, MESSAGE_BUDGET};
    use fastcord_discord::state::navigation::{ChannelSelection, PermissionSummary};
    use fastcord_model::{MessageUpdate, User};

    use super::*;
    use crate::variable_list::{Anchor, ScrollTarget};

    const CHANNEL: Snowflake = Snowflake(777);
    /// Message `n` is the `n`-th message of the fixture channel; IDs grow with time.
    const NEWEST: u64 = 10_000;

    fn rest() -> RestClient {
        RestClient::new(UserToken::new("offline-fixture-credential".to_owned())).unwrap()
    }

    fn message_with(n: u64, content: String) -> Message {
        Message {
            id: Snowflake(n),
            channel_id: CHANNEL,
            guild_id: None,
            author: User {
                id: Snowflake(1 + n % 7),
                username: format!("author{}", n % 7),
                global_name: None,
                avatar: None,
                bot: false,
            },
            content,
            timestamp: "2026-10-07T12:00:00.000000+00:00".to_owned(),
            edited_timestamp: None,
            kind: 0,
            flags: 0,
            pinned: false,
            attachments: Vec::new(),
            reactions: Vec::new(),
            message_reference: None,
        }
    }

    fn message(n: u64) -> Message {
        message_with(n, format!("fixture message {n}"))
    }

    /// What Discord answers for a cursor over messages `1..=newest`, newest first.
    fn serve(cursor: HistoryCursor, newest: u64) -> Vec<Message> {
        let (low, high) = match cursor {
            HistoryCursor::Latest => (newest.saturating_sub(49).max(1), newest),
            HistoryCursor::Before(id) => (id.0.saturating_sub(50).max(1), id.0.saturating_sub(1)),
            HistoryCursor::After(id) => (id.0 + 1, (id.0 + 50).min(newest)),
        };
        (low..=high).rev().map(message).collect()
    }

    fn navigation(readable: bool) -> NavigationSnapshot {
        NavigationSnapshot {
            selection: Some(ChannelSelection {
                guild_id: Snowflake(5),
                channel_id: CHANNEL,
                name: "general".to_owned(),
                kind: ChannelKind::Text,
                permissions: PermissionSummary {
                    view_channel: true,
                    read_history: readable,
                    send_messages: true,
                    ..PermissionSummary::default()
                },
            }),
            ..NavigationSnapshot::default()
        }
    }

    /// The widget reporting that the reader is at `anchor`, `at_bottom` or not.
    fn viewport(anchor: Snowflake, in_row: f32, at_bottom: bool) -> Request {
        Request {
            viewport: Some((
                CHANNEL,
                Viewport {
                    offset: 0.0,
                    height: 600.0,
                    width: 640.0,
                    anchor: Some(Anchor { id: anchor, in_row }),
                    at_bottom,
                    applied: u64::MAX,
                },
            )),
            ..Request::default()
        }
    }

    fn cursor(history: &History) -> Option<HistoryCursor> {
        history.pending.as_ref().map(|(token, _)| token.cursor())
    }

    fn assert_bounded(history: &mut History) -> Snapshot {
        assert!(history.store.messages_held() <= MAX_MESSAGES_PER_CHANNEL);
        assert!(history.store.tracked_bytes() <= MESSAGE_BUDGET);
        let snapshot = history.snapshot();
        let bytes: usize = snapshot
            .rows
            .iter()
            .map(|row| message_bytes(&row.message))
            .sum();
        assert!(bytes <= SNAPSHOT_BODY_BUDGET);
        assert_eq!(snapshot.rows.len(), snapshot.window.range.len());
        assert!(snapshot.rows.len() <= 128);
        snapshot
    }

    fn open_latest(history: &mut History) {
        history.refresh(&rest(), &navigation(true), &Request::default());
        assert_eq!(cursor(history), Some(HistoryCursor::Latest));
        assert!(!history.finish(Ok(serve(HistoryCursor::Latest, NEWEST))));
    }

    #[test]
    fn ten_thousand_messages_page_through_without_being_retained() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        open_latest(&mut history);
        let mut seen = std::collections::BTreeSet::new();
        seen.extend(history.store.ids(CHANNEL));
        let mut pages = 1;
        // The reader keeps scrolling to the oldest retained message.
        loop {
            let first = history.store.ids(CHANNEL).next().unwrap();
            history.refresh(&rest, &navigation, &viewport(first, 0.0, false));
            let Some(next) = cursor(&history) else { break };
            assert_eq!(next, HistoryCursor::Before(first), "pages walk backward");
            assert!(!history.finish(Ok(serve(next, NEWEST))));
            seen.extend(history.store.ids(CHANNEL));
            pages += 1;
            assert_bounded(&mut history);
            assert!(pages <= 202, "never more than one request per page");
        }
        // The newest page, 199 full older pages, and one empty page: a full
        // page cannot prove the start of history, an empty one does.
        assert_eq!(pages, 201);
        assert_eq!(history.store.ids(CHANNEL).next(), Some(Snowflake(1)));
        assert!(history.store.window(CHANNEL).unwrap().older.is_none());
        // Every message was shown at some point, yet only a bounded window of
        // them is held now, and the newest end was the part let go.
        assert!(seen.len() >= 9_500, "{} distinct messages", seen.len());
        assert!(history.store.messages_held() <= MAX_MESSAGES_PER_CHANNEL);
        assert!(history.store.window(CHANNEL).unwrap().newer.is_some());
        let snapshot = assert_bounded(&mut history);
        assert!(snapshot.has_newer && !snapshot.has_older);
    }

    #[test]
    fn prepending_a_page_keeps_the_readers_anchor() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        open_latest(&mut history);
        // Reading message 9_952, 12 px into its row, near the top of the page.
        history.refresh(&rest, &navigation, &viewport(Snowflake(9_952), 12.0, false));
        let before = history.list.anchor().unwrap();
        assert_eq!(before.id, Snowflake(9_952));
        let older = cursor(&history).expect("near the oldest retained row");
        assert_eq!(older, HistoryCursor::Before(Snowflake(9_951)));
        history.finish(Ok(serve(older, NEWEST)));
        let snapshot = history.snapshot();
        let anchor = history.list.anchor().unwrap();
        assert_eq!(anchor.id, before.id);
        assert!((anchor.in_row - before.in_row).abs() < 0.01);
        // Fifty rows were added above it. The widget keeps the reader in place
        // itself in the same frame; the worker asks for no correction that could
        // snap back a reader who has already scrolled on.
        assert_eq!(history.store.channel_len(CHANNEL), 100);
        assert!(snapshot.scroll.is_none());
        assert!(
            snapshot
                .rows
                .iter()
                .any(|row| row.message.id == Snowflake(9_952))
        );
    }

    #[test]
    fn gateway_events_update_and_delete_rows_while_paging_state_is_kept() {
        let mut history = History::default();
        open_latest(&mut history);
        let revision_of =
            |history: &History, n: u64| history.store.revision(CHANNEL, Snowflake(n)).unwrap();
        let before = revision_of(&history, 9_999);
        history.apply(&Dispatch::MessageUpdate(Box::new(MessageUpdate {
            id: Snowflake(9_999),
            channel_id: CHANNEL,
            content: Some("edited".to_owned()),
            edited_timestamp: Some(Some("2026-10-07T12:01:00.000000+00:00".to_owned())),
            flags: None,
            pinned: None,
            attachments: None,
        })));
        assert!(history.dirty());
        let snapshot = history.snapshot();
        let row = snapshot
            .rows
            .iter()
            .find(|row| row.message.id == Snowflake(9_999))
            .unwrap();
        assert_eq!(row.message.content, "edited");
        assert_ne!(
            row.revision, before,
            "a changed body invalidates its layout"
        );
        history.apply(&Dispatch::MessageDelete(MessageDelete {
            id: Snowflake(9_998),
            channel_id: CHANNEL,
            guild_id: None,
        }));
        history.apply(&Dispatch::MessageDeleteBulk(MessageDeleteBulk {
            ids: vec![Snowflake(9_997), Snowflake(9_996)],
            channel_id: CHANNEL,
            guild_id: None,
        }));
        history.apply(&Dispatch::MessageCreate(Box::new(message(10_001))));
        let snapshot = history.snapshot();
        let shown: Vec<u64> = snapshot.rows.iter().map(|row| row.message.id.0).collect();
        assert!(!shown.contains(&9_998) && !shown.contains(&9_997) && !shown.contains(&9_996));
        assert!(shown.contains(&10_001) && shown.contains(&9_999));
        assert_eq!(history.store.channel_len(CHANNEL), 50 - 3 + 1);
    }

    #[test]
    fn only_a_readable_selected_channel_is_fetched_and_leaving_it_releases_everything() {
        let rest = rest();
        let mut history = History::default();
        history.refresh(&rest, &navigation(false), &Request::default());
        assert!(cursor(&history).is_none(), "no read-history permission");
        assert_eq!(history.snapshot(), Snapshot::default());
        history.refresh(&rest, &NavigationSnapshot::default(), &Request::default());
        assert!(cursor(&history).is_none(), "nothing selected");

        open_latest(&mut history);
        assert_eq!(history.store.channel_len(CHANNEL), 50);
        // Permission loss closes the channel and drops its bodies at once.
        history.refresh(&rest, &navigation(false), &Request::default());
        assert_eq!(history.store.messages_held(), 0);
        assert!(history.snapshot().rows.is_empty());
        // A page still in flight for a closed channel is not applied.
        history.refresh(&rest, &navigation(true), &Request::default());
        assert_eq!(cursor(&history), Some(HistoryCursor::Latest));
        history.refresh(&rest, &NavigationSnapshot::default(), &Request::default());
        assert!(history.pending.is_none());
        assert!(!history.finish(Ok(serve(HistoryCursor::Latest, NEWEST))));
        assert_eq!(history.store.messages_held(), 0);
    }

    #[test]
    fn failures_are_visible_retried_only_on_request_and_auth_loss_is_reported() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        history.refresh(&rest, &navigation, &Request::default());
        assert!(!history.finish(Err(RestError::PermissionDenied)));
        let snapshot = history.snapshot();
        assert!(snapshot.rows.is_empty() && !snapshot.loading);
        assert!(snapshot.error.is_some_and(|text| text.contains("denied")));
        // Scrolling or measuring never repeats a failed page by itself.
        history.refresh(&rest, &navigation, &viewport(Snowflake(1), 0.0, true));
        assert!(cursor(&history).is_none());
        // Retry repeats exactly the failed request.
        let retry = Request {
            revision: 1,
            intent: Some(Intent::Retry(CHANNEL)),
            ..Request::default()
        };
        history.refresh(&rest, &navigation, &retry);
        assert_eq!(cursor(&history), Some(HistoryCursor::Latest));
        assert!(history.snapshot().error.is_none());
        assert!(history.finish(Err(RestError::AuthenticationRequired)));
    }

    #[test]
    fn jump_to_latest_reloads_the_newest_page_when_it_was_not_retained() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        open_latest(&mut history);
        // Page back until the newest rows are no longer retained.
        for _ in 0..12 {
            let first = history.store.ids(CHANNEL).next().unwrap();
            history.refresh(&rest, &navigation, &viewport(first, 0.0, false));
            let next = cursor(&history).unwrap();
            history.finish(Ok(serve(next, NEWEST)));
        }
        assert!(history.snapshot().has_newer);
        let jump = Request {
            revision: 1,
            intent: Some(Intent::Latest(CHANNEL)),
            ..Request::default()
        };
        history.refresh(&rest, &navigation, &jump);
        assert_eq!(cursor(&history), Some(HistoryCursor::Latest));
        history.finish(Ok(serve(HistoryCursor::Latest, NEWEST)));
        let snapshot = assert_bounded(&mut history);
        assert!(!snapshot.has_newer);
        assert_eq!(history.store.ids(CHANNEL).last(), Some(Snowflake(NEWEST)));
        assert!(matches!(
            snapshot.scroll.map(|request| request.target),
            Some(ScrollTarget::Bottom)
        ));
    }

    #[test]
    fn oversized_messages_are_reported_and_a_snapshot_stays_within_its_budget() {
        let rest = rest();
        let navigation = navigation(true);
        let mut history = History::default();
        history.refresh(&rest, &navigation, &Request::default());
        let big = "x".repeat(200 * 1024);
        let page: Vec<Message> = (1..=50)
            .rev()
            .map(|n| match n {
                // Larger than the largest body the store accepts.
                7 => message_with(n, "y".repeat(MAX_MESSAGE_BYTES)),
                _ => message_with(n, big.clone()),
            })
            .collect();
        history.finish(Ok(page));
        let snapshot = assert_bounded(&mut history);
        assert!(
            snapshot
                .error
                .is_some_and(|text| text.contains("too large"))
        );
        assert!(history.store.get(CHANNEL, Snowflake(7)).is_none());
        assert!(!snapshot.rows.is_empty());
        assert!(snapshot.rows.len() < 49, "bodies bound the rows built");
    }

    #[test]
    fn measurements_coalesce_per_row_and_stay_capped() {
        use crate::variable_list::Measurement;
        let mut request = Request::default();
        let measure = |id, bucket, height| Measurement {
            id: Snowflake(id),
            revision: 1,
            width_bucket: bucket,
            height,
        };
        for height in 0..1_000 {
            request.measure(CHANNEL, measure(1, 3, height as f32));
        }
        assert_eq!(request.measurements.len(), 1);
        assert_eq!(request.measurements[0].1.height, 999.0);
        request.measure(CHANNEL, measure(1, 4, 5.0));
        assert_eq!(request.measurements.len(), 2, "another width bucket");
        for id in 0..1_000 {
            request.measure(CHANNEL, measure(id, 3, 10.0));
        }
        assert!(request.measurements.len() <= MAX_MEASUREMENTS);
    }
}
