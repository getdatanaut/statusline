//! What a click on the tray asks for, and the router that delivers it.
//!
//! Every control the tray builds is bound with an [`Act`] through [`bind`];
//! the controller sets the one handler that answers them all. The router is
//! per thread, and the tray's controls all live on the main thread: a control
//! built before the controller exists, or in a preview with no controller at
//! all, is bound the same way, and a click on it simply does nothing until
//! there is a handler.

use std::cell::RefCell;
use std::rc::Rc;

use objc2::runtime::AnyObject;
use objc2_app_kit::NSControl;
use sl_macos::action::ActionRouter;

/// One kind of click. Which session, job or stack it is about is its
/// [`Target`]; a segment or a switch is read off the sender (its selected
/// segment, whether it is on), and a meter page or a notification kind off
/// its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
    ClosedWindowCopyId,
    ClosedWindowDismiss,
    ClosedWindowZed,
    CopyClosedId,
    CopyCwd,
    CopyJobLink,
    CopyRcId,
    CopyReview,
    DismissClosed,
    FilterSessions,
    OpenClosedZed,
    OpenLinear,
    OpenPr,
    OpenReviewInZed,
    OpenStack,
    OpenStackPr,
    OpenZed,
    Quit,
    RetryJob,
    SearchClosed,
    SettingsBarWindow,
    SettingsCacheLead,
    SettingsCold,
    SettingsDensity,
    SettingsLayout,
    SettingsMaxSessions,
    SettingsMeters,
    SettingsNeedsYou,
    SettingsPr,
    SettingsRing,
    SettingsSort,
    SettingsUsage,
    ShowClosedWindow,
    ShowLogs,
    ShowMeterPage,
    ShowSettings,
    ShowStats,
    StatsProvider,
    ToggleAutoAwake,
    ToggleBarBadge,
    ToggleCard,
    ToggleClosed,
    ToggleClosedGroup,
    ToggleCold,
    ToggleLinear,
    ToggleNotify,
    TogglePopover,
    ToggleRcAwake,
    ToggleSpend,
}

/// What a click is about, by identity rather than by position: the handler
/// looks it up in what is current, so a list that moved between the paint
/// and the click cannot send it to the wrong session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Target {
    /// Nothing in particular: a footer button, a setting.
    #[default]
    None,
    /// An open session, by id.
    Session(String),
    /// One of an open session's failed CI jobs, by name.
    Job { session: String, job: String },
    /// A closed session, by id.
    Closed(String),
    /// A stack, by [`crate::stacks::StackView::key`].
    Stack(String),
    /// One PR of a stack.
    StackPr { stack: String, pr: i64 },
}

/// A target a row is repainted with: the menu keeps its rows and reuses them
/// for whatever they show next, so each row's controls share a slot that its
/// paint fills in.
pub(crate) type Slot = Rc<RefCell<Target>>;

/// A click as the router carries it.
#[derive(Clone)]
struct Click {
    act: Act,
    target: Slot,
}

thread_local! {
    static ROUTER: ActionRouter<Click> = ActionRouter::new();
}

/// A click on `control` asks for `act`, about nothing in particular.
pub(crate) fn bind(control: &NSControl, act: Act) {
    bind_to(control, act, Target::None);
}

/// A click on `control` asks for `act` about `target`.
pub(crate) fn bind_to(control: &NSControl, act: Act, target: Target) {
    bind_slot(control, act, &Rc::new(RefCell::new(target)));
}

/// A click on `control` asks for `act` about whatever `slot` holds then.
pub(crate) fn bind_slot(control: &NSControl, act: Act, slot: &Slot) {
    ROUTER.with(|router| {
        router.bind(
            control,
            Click {
                act,
                target: Rc::clone(slot),
            },
        );
    });
}

/// Answer every click on this thread with `handler`.
pub(crate) fn set_handler(handler: impl Fn(Act, &Target, &AnyObject) + 'static) {
    ROUTER.with(|router| {
        router.set_handler(move |click: Click, sender| {
            // Read out first: the handler repaints, which refills the slot.
            let target = click.target.borrow().clone();
            handler(click.act, &target, sender);
        });
    });
}
