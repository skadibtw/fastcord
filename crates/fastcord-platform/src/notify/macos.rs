//! macOS notifications through `UNUserNotificationCenter`.
//!
//! The framework needs a bundled application (it raises an exception for a
//! bare executable), so outside a `.app` with a bundle identifier this is
//! [`NotifyError::Unsupported`]. The first toast asks for permission; a refusal
//! makes every toast fail with [`NotifyError::Failed`]. A delegate, kept for
//! the life of the process, receives the click and also lets a toast show
//! while fastcord is frontmost but not focused on a conversation.
//!
//! Unsafe code is confined to this module: it is the Objective-C FFI.
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AnyThread, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSSet, NSString, NSUUID,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
    UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use super::{Action, NotifyError, OnClick, Pending};

/// How long the notification center may take to accept a toast.
const ANSWER: Duration = Duration::from_secs(5);

static PENDING: Mutex<Pending<String>> = Mutex::new(Pending::new());
static NEXT: AtomicU64 = AtomicU64::new(0);
static PROCESS_ID: OnceLock<String> = OnceLock::new();

fn request_id() -> String {
    let process = PROCESS_ID.get_or_init(|| NSUUID::UUID().UUIDString().to_string());
    format!(
        "fastcord-{process}-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
/// Whether this process can use the notification center at all.
static READY: OnceLock<bool> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Delegate` does not
    // implement `Drop`.
    #[unsafe(super(NSObject))]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // The caller decides when a toast is due; show it even when the
            // application is frontmost.
            completion
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            // SAFETY: an extern static string of the framework.
            let default = unsafe { UNNotificationDefaultActionIdentifier };
            let identifier = response.actionIdentifier();
            let action = if identifier.isEqualToString(default) {
                Some(Action::Open)
            } else if identifier.isEqualToString(&NSString::from_str("mark-read")) {
                Some(Action::MarkRead)
            } else {
                None
            };
            if let Some(action) = action {
                let id = response.notification().request().identifier().to_string();
                let click = PENDING
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.take(&id));
                if let Some(click) = click {
                    click(action);
                }
            }
            completion.call(());
        }
    }
);

impl Delegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Installs the delegate and asks for permission, once per process.
fn ready(mark_read_label: &str) -> bool {
    *READY.get_or_init(|| {
        if NSBundle::mainBundle().bundleIdentifier().is_none() {
            return false;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.setNotificationCategories(&NSSet::from_retained_slice(&[category(mark_read_label)]));
        let delegate = Delegate::new();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // The center holds its delegate weakly; this one lives as long as the process.
        std::mem::forget(delegate);
        let asked = RcBlock::new(|_granted: Bool, _error: *mut NSError| {});
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &asked,
        );
        true
    })
}

pub(super) fn show(
    title: &str,
    body: &str,
    _open_label: &str,
    mark_read_label: &str,
    click: OnClick,
) -> Result<(), NotifyError> {
    if !ready(mark_read_label) {
        return Err(NotifyError::Unsupported);
    }
    let id = request_id();
    let content = UNMutableNotificationContent::new();
    content.setCategoryIdentifier(&NSString::from_str("fastcord-message"));
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&id),
        &content,
        None,
    );
    if let Ok(mut pending) = PENDING.lock() {
        pending.insert(id.clone(), click);
    }
    let (sender, answer) = mpsc::sync_channel(1);
    let added = RcBlock::new(move |error: *mut NSError| {
        let _ = sender.send(error.is_null());
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&added));
    if answer.recv_timeout(ANSWER).unwrap_or(false) {
        Ok(())
    } else {
        // Refused permission, or the center did not answer.
        let _ = PENDING.lock().map(|mut pending| pending.take(&id));
        Err(NotifyError::Failed)
    }
}

fn category(mark_read_label: &str) -> Retained<UNNotificationCategory> {
    let action = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str("mark-read"),
        &NSString::from_str(mark_read_label),
        UNNotificationActionOptions::empty(),
    );
    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str("fastcord-message"),
        &NSArray::from_retained_slice(&[action]),
        &NSArray::new(),
        UNNotificationCategoryOptions::empty(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_category_declares_one_explicit_read_action_without_launching_or_asking_permission() {
        let first = request_id();
        let second = request_id();
        assert_ne!(first, second);
        assert!(first.starts_with("fastcord-") && first.len() < 80);
        assert!(first.contains(PROCESS_ID.get().unwrap()));
        let category = category("Mark as read");
        assert_eq!(category.identifier().to_string(), "fastcord-message");
        let actions = category.actions();
        assert_eq!(actions.len(), 1);
        let action = actions.objectAtIndex(0);
        assert_eq!(action.identifier().to_string(), "mark-read");
        assert_eq!(action.title().to_string(), "Mark as read");
        assert!(action.options().is_empty());
    }
}
