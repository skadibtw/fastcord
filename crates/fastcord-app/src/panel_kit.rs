//! The request lifecycle the feature panels share (#190): one busy marker,
//! one stale-answer serial, one notice, one two-step confirm and one 401 test.
//! A panel keeps its own data and events; this only replaces the hand-rolled
//! `busy`/`serial`/`notice`/`confirm` fields and their `clear()` bookkeeping.

use fastcord_discord::RestError;

use crate::i18n::tr;

/// What every panel says to a second request while one is in flight.
pub const WAIT: &str = "Wait for the previous change to finish.";

/// Busy marker (the request in flight, `()` when the kind does not matter),
/// serial that stamps events, and the one-line notice.
#[derive(Debug)]
pub struct Gate<A = ()> {
    serial: u64,
    busy: Option<A>,
    pub notice: Option<String>,
}

impl<A> Default for Gate<A> {
    fn default() -> Self {
        Self {
            serial: 0,
            busy: None,
            notice: None,
        }
    }
}

impl<A: Copy + PartialEq> Gate<A> {
    /// The stamp for events of the request being started now.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    pub fn busy(&self) -> Option<A> {
        self.busy
    }

    /// Marks `a` in flight and clears the notice; returns the stamp to put
    /// on the answer event.
    pub fn begin(&mut self, a: A) -> u64 {
        self.busy = Some(a);
        self.notice = None;
        self.serial
    }

    /// Whether an answer stamped `serial` belongs to the current panel. A
    /// late answer for a panel that was reset is ignored (returns false).
    pub fn is_current(&self, serial: u64) -> bool {
        serial == self.serial
    }

    /// Takes an answer: false for a stale one, otherwise the busy marker is
    /// released.
    pub fn accept(&mut self, serial: u64) -> bool {
        let current = self.is_current(serial);
        if current {
            self.busy = None;
        }
        current
    }

    /// Refuses a second request: sets the shared notice and returns true
    /// when one is already in flight.
    pub fn refuse_if_busy(&mut self) -> bool {
        let busy = self.busy.is_some();
        if busy {
            self.notice = Some(tr(WAIT).to_owned());
        }
        busy
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
    }

    /// A new generation for a reload that keeps `busy` and the notice: the
    /// answers of every earlier request are stale. Returns the new stamp.
    pub fn bump(&mut self) -> u64 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    /// Closing or switching account: drops busy and notice and invalidates
    /// every answer still on its way.
    pub fn reset(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        self.busy = None;
        self.notice = None;
    }
}

/// The destructive step waiting for its second click.
#[derive(Debug)]
pub struct Confirm<T>(Option<T>);

impl<T> Default for Confirm<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T: Clone + PartialEq> Confirm<T> {
    pub fn ask(&mut self, t: T) {
        self.0 = Some(t);
    }

    pub fn cancel(&mut self) {
        self.0 = None;
    }

    pub fn pending(&self) -> Option<T> {
        self.0.clone()
    }

    pub fn get(&self) -> Option<&T> {
        self.0.as_ref()
    }

    pub fn is(&self, t: T) -> bool {
        self.0.as_ref() == Some(&t)
    }

    pub fn take(&mut self) -> Option<T> {
        self.0.take()
    }
}

/// A 401 from Discord: the caller stops the account like any other.
pub fn auth_rejected<T>(result: &Result<T, RestError>) -> bool {
    matches!(result, Err(RestError::AuthenticationRequired))
}

/// The shared image pick: the native file dialog, then `read` on a blocking
/// thread (each caller keeps its own size and type limits). `Ok(None)` is a
/// cancelled dialog.
pub async fn pick_image<T: Send + 'static>(
    title: &'static str,
    extensions: &[&str],
    read: fn(&std::path::Path) -> Result<T, &'static str>,
) -> Result<Option<T>, &'static str> {
    let Some(file) = rfd::AsyncFileDialog::new()
        .set_title(title)
        .add_filter(tr("Image"), extensions)
        .pick_file()
        .await
    else {
        return Ok(None);
    };
    let path = file.path().to_owned();
    tokio::task::spawn_blocking(move || read(&path).map(Some))
        .await
        .unwrap_or(Err("The image could not be read."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_answer_for_a_reset_panel_is_ignored() {
        let mut gate = Gate::<u8>::default();
        let stamp = gate.begin(1);
        assert!(gate.is_busy());
        gate.reset();
        assert!(!gate.is_busy());
        assert!(!gate.accept(stamp), "stale");
        let fresh = gate.begin(2);
        assert!(!gate.accept(stamp), "still stale, busy kept");
        assert_eq!(gate.busy(), Some(2));
        assert!(gate.accept(fresh));
        assert!(!gate.is_busy());
    }

    #[test]
    fn begin_clears_the_notice_and_a_second_request_is_refused() {
        let mut gate = Gate::<()>::default();
        gate.say("old");
        gate.begin(());
        assert!(gate.notice.is_none());
        assert!(gate.refuse_if_busy());
        assert!(gate.notice.is_some());
        let s = gate.serial();
        gate.accept(s);
        gate.notice = None;
        assert!(!gate.refuse_if_busy() && gate.notice.is_none());
    }

    #[test]
    fn a_restamp_keeps_busy_and_the_notice_but_drops_old_answers() {
        let mut gate = Gate::<u8>::default();
        let old = gate.begin(1);
        gate.say("kept");
        let new = gate.bump();
        assert_ne!(old, new);
        assert!(gate.is_busy() && gate.notice.is_some());
        assert!(!gate.is_current(old) && gate.is_current(new));
    }

    #[test]
    fn confirm_holds_a_non_copy_value() {
        let mut c = Confirm::<String>::default();
        c.ask("a".into());
        assert!(c.is("a".into()) && c.get().map(String::as_str) == Some("a"));
        assert_eq!(c.take().as_deref(), Some("a"));
    }

    #[test]
    fn confirm_is_consumed_once() {
        let mut c = Confirm::<u8>::default();
        c.ask(3);
        assert!(c.is(3) && !c.is(4));
        assert_eq!(c.take(), Some(3));
        assert_eq!(c.take(), None, "consumed once");
        c.ask(3);
        c.cancel();
        assert_eq!(c.pending(), None);
    }

    #[test]
    fn only_a_401_is_an_auth_rejection() {
        assert!(auth_rejected::<()>(&Err(RestError::AuthenticationRequired)));
        assert!(!auth_rejected::<()>(&Err(RestError::PermissionDenied)));
        assert!(!auth_rejected(&Ok(())));
    }
}
