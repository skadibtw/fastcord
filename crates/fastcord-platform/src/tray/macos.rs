//! The macOS menu-bar icon: one `NSStatusItem` with the committed app mark and
//! a menu. AppKit objects live only on the main thread (the one running the
//! application's event loop), so every change is dispatched there and nothing
//! runs between clicks. Clicking the icon opens its menu, as menu-bar items do.
//!
//! Unsafe code is confined to this module: it is the Objective-C FFI.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::sync::Mutex;

use dispatch2::{DispatchQueue, run_on_main};
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSObject, NSObjectProtocol, NSSize, NSString};

use super::{Command, TrayError, ico_entry, menu, state, tooltip};

type Sink = Box<dyn FnMut(Command) + Send>;

const ICON: &[u8] = include_bytes!("../../../../assets/icons/app/fastcord.ico");
/// Points the mark is drawn at in the menu bar (the 32 px entry, sharp on Retina).
const ICON_POINTS: f64 = 18.0;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

thread_local! {
    /// The icon and its menus' action target; set and read on the main thread only.
    static ITEM: RefCell<Option<(Retained<NSStatusItem>, Retained<Target>)>> =
        const { RefCell::new(None) };
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `Target` does not
    // implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    impl Target {
        #[unsafe(method(choose:))]
        fn choose(&self, sender: &NSMenuItem) {
            if let Some(command) = usize::try_from(sender.tag()).ok().and_then(Command::from_id)
                && let Ok(mut sink) = SINK.lock()
                && let Some(sink) = sink.as_mut()
            {
                sink(command);
            }
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Handle;

pub fn refresh() {
    DispatchQueue::main().exec_async(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            ITEM.with(|item| {
                if let Some((item, target)) = item.borrow().as_ref() {
                    show(mtm, item, target);
                }
            });
        }
    });
}

pub fn start(sink: Sink) -> Result<Handle, TrayError> {
    {
        let mut slot = SINK.lock().map_err(|_| TrayError::Failed)?;
        if slot.is_some() {
            return Err(TrayError::Failed);
        }
        *slot = Some(sink);
    }
    let created = run_on_main(|mtm| {
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let Some(button) = item.button(mtm) else {
            NSStatusBar::systemStatusBar().removeStatusItem(&item);
            return false;
        };
        let png = ico_entry(ICON, 32).map(NSData::with_bytes);
        if let Some(image) = png.and_then(|data| NSImage::initWithData(NSImage::alloc(), &data)) {
            image.setSize(NSSize::new(ICON_POINTS, ICON_POINTS));
            button.setImage(Some(&image));
        } else {
            button.setTitle(&NSString::from_str("fastcord"));
        }
        let target = Target::new(mtm);
        show(mtm, &item, &target);
        ITEM.with(|slot| *slot.borrow_mut() = Some((item, target)));
        true
    });
    if created {
        Ok(Handle)
    } else {
        let _ = SINK.lock().map(|mut slot| slot.take());
        Err(TrayError::Failed)
    }
}

/// Puts the current tooltip and menu on the icon.
fn show(mtm: MainThreadMarker, item: &NSStatusItem, target: &Target) {
    let current = state();
    if let Some(button) = item.button(mtm) {
        button.setToolTip(Some(&NSString::from_str(&tooltip(current))));
    }
    let bar_menu = NSMenu::new(mtm);
    for (command, label) in menu(current) {
        // SAFETY: `choose:` is a method of `Target`, which takes one NSMenuItem.
        let entry = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(choose:)),
                &NSString::new(),
            )
        };
        entry.setTag(command.id() as isize);
        // SAFETY: the target is a `Target`, kept alive with the item.
        unsafe { entry.setTarget(Some(target)) };
        bar_menu.addItem(&entry);
    }
    item.setMenu(Some(&bar_menu));
}

impl Drop for Handle {
    fn drop(&mut self) {
        run_on_main(|_| {
            if let Some((item, _)) = ITEM.with(|slot| slot.borrow_mut().take()) {
                NSStatusBar::systemStatusBar().removeStatusItem(&item);
            }
        });
        let _ = SINK.lock().map(|mut slot| slot.take());
    }
}
