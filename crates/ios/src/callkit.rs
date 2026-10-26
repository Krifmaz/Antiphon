//! CallKit and PushKit: calls shown as phone calls, and VoIP pushes that
//! wake the app for them.
//!
//! The two are one module because iOS ties them together: since iOS 13 an
//! app that receives a VoIP push and does not report an incoming CallKit
//! call before the push handler returns is killed, and after a few
//! misses stops receiving VoIP pushes at all. So every push reports a call
//! first and asks questions after.
//!
//! The push payload is end-to-end encrypted and only the shared code can
//! open it, so the call is reported as "Antiphon" and renamed once the
//! shared code says who it is with ([`set_call`]). Pushes that are not
//! calls (messages, files) must come as ordinary alert pushes, not VoIP
//! ones, or each would ring.
//!
//! The payload contract on this side: the APNs JSON carries the sealed
//! bytes base64-encoded under the key `"p"`. They are handed to the shared
//! code as [`PlatformEvent::Push`], still sealed.
//!
//! Everything here runs on the main thread: CallKit's and PushKit's
//! delegate queues are the main queue, and the platform calls hop there.

use std::cell::RefCell;

use antiphon_mobile::platform::{self, CallAction, PlatformEvent};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_call_kit::{
    CXAnswerCallAction, CXCallController, CXCallEndedReason, CXCallUpdate, CXEndCallAction, CXHandle, CXHandleType,
    CXProvider, CXProviderConfiguration, CXProviderDelegate, CXSetMutedCallAction, CXStartCallAction,
};
use objc2_foundation::{ns_string, NSError, NSNumber, NSSet, NSString, NSUUID, NSUserDefaults};
use objc2_push_kit::{PKPushCredentials, PKPushPayload, PKPushRegistry, PKPushRegistryDelegate, PKPushType, PKPushTypeVoIP};

/// The name a call has before the shared code has opened the push.
const UNKNOWN_CALLER: &str = "Antiphon";
/// Set once the user has turned push on, so later launches register with
/// PushKit straight away -- including the launch a push itself causes.
const PUSH_ENABLED_KEY: &str = "antiphon.push.enabled";

/// The one call CallKit knows about.
struct Call {
    uuid: Retained<NSUUID>,
    incoming: bool,
    /// Answered, from CallKit's screen or in the app.
    answered: bool,
}

struct CallKit {
    provider: Retained<CXProvider>,
    controller: Retained<CXCallController>,
    /// The provider holds its delegate weakly.
    _delegate: Retained<ProviderDelegate>,
    call: Option<Call>,
}

struct Push {
    _registry: Retained<PKPushRegistry>,
    _delegate: Retained<PushDelegate>,
}

thread_local! {
    static CALLKIT: RefCell<Option<CallKit>> = const { RefCell::new(None) };
    static PUSH: RefCell<Option<Push>> = const { RefCell::new(None) };
}

fn post(event: PlatformEvent) {
    platform::post(event);
    crate::shell::wake();
}

fn log_error(what: &'static str) -> RcBlock<dyn Fn(*mut NSError)> {
    RcBlock::new(move |error: *mut NSError| {
        // SAFETY: CallKit passes nil or a live error.
        if let Some(error) = unsafe { error.as_ref() } {
            log::warn!("{what}: {error}");
        }
    })
}

fn handle(name: &str) -> Retained<CXHandle> {
    // SAFETY: a designated initialiser with valid arguments.
    unsafe { CXHandle::initWithType_value(CXHandle::alloc(), CXHandleType::Generic, &NSString::from_str(name)) }
}

fn update_for(name: &str) -> Retained<CXCallUpdate> {
    // SAFETY: setters with valid arguments on a fresh object.
    unsafe {
        let update = CXCallUpdate::new();
        update.setRemoteHandle(Some(&handle(name)));
        update.setLocalizedCallerName(Some(&NSString::from_str(name)));
        update.setHasVideo(false);
        update.setSupportsHolding(false);
        update.setSupportsGrouping(false);
        update.setSupportsUngrouping(false);
        update.setSupportsDTMF(false);
        update
    }
}

/// Runs `f` with CallKit set up, creating the provider the first time.
fn with_callkit<T>(mtm: MainThreadMarker, f: impl FnOnce(&mut CallKit) -> T) -> Option<T> {
    CALLKIT.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            log::warn!("CallKit re-entered; ignoring");
            return None;
        };
        if slot.is_none() {
            // SAFETY: setters with valid arguments on fresh objects; a nil
            // queue means the main queue, where the delegate must run.
            let callkit = unsafe {
                let config = CXProviderConfiguration::new();
                config.setSupportsVideo(false);
                config.setMaximumCallGroups(1);
                config.setMaximumCallsPerCallGroup(1);
                // Kept out of the Phone app's Recents, which syncs through
                // iCloud: a list of who called whom is what this app exists
                // not to leave lying around.
                config.setIncludesCallsInRecents(false);
                let generic = NSNumber::new_isize(CXHandleType::Generic.0);
                config.setSupportedHandleTypes(&NSSet::from_retained_slice(&[generic]));
                let provider = CXProvider::initWithConfiguration(CXProvider::alloc(), &config);
                let delegate = ProviderDelegate::new(mtm);
                provider.setDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), None);
                CallKit { provider, controller: CXCallController::new(), _delegate: delegate, call: None }
            };
            *slot = Some(callkit);
        }
        slot.as_mut().map(f)
    })
}

/// Tells CallKit about the app's call: `Some(name)` while one runs.
/// Called with the shared code's background work on every change.
pub fn set_call(mtm: MainThreadMarker, name: Option<&str>) {
    let known = CALLKIT.with(|slot| slot.try_borrow().map(|s| s.as_ref().is_some_and(|c| c.call.is_some())).unwrap_or(false));
    if name.is_none() && !known {
        // Nothing to end, and no reason to create a provider for it.
        return;
    }
    with_callkit(mtm, |callkit| match (name, callkit.call.as_ref()) {
        (Some(name), None) => {
            // A call CallKit has not heard of: one the app placed, or one
            // that reached the open app directly rather than by push.
            // Either way it is registered as started here, through a start
            // action that comes back to the delegate to be fulfilled; the
            // direction only shapes CallKit's own record of it, which is
            // kept out of Recents anyway.
            let uuid = NSUUID::new();
            // SAFETY: valid arguments; the completion only logs.
            unsafe {
                let action = CXStartCallAction::initWithCallUUID_handle(CXStartCallAction::alloc(), &uuid, &handle(name));
                callkit.controller.requestTransactionWithAction_completion(&action, &log_error("starting a CallKit call"));
            }
            callkit.call = Some(Call { uuid, incoming: false, answered: true });
        }
        (Some(name), Some(call)) => {
            // The shared code opened the push and knows who is calling, or
            // the name changed: show it on the call screen.
            // SAFETY: a report about a call CallKit knows.
            unsafe { callkit.provider.reportCallWithUUID_updated(&call.uuid, &update_for(name)) };
        }
        (None, Some(call)) => {
            let reason =
                if call.incoming && !call.answered { CXCallEndedReason::Unanswered } else { CXCallEndedReason::RemoteEnded };
            // SAFETY: as above; a nil date means now.
            unsafe { callkit.provider.reportCallWithUUID_endedAtDate_reason(&call.uuid, None, reason) };
            callkit.call = None;
        }
        (None, None) => {}
    });
}

/// The call is connected: answered in the app, or picked up at the other
/// end. For an outgoing call this starts CallKit's timer; for an incoming
/// one answered on the app's own screen, it asks CallKit to answer too, so
/// its ringing screen goes away.
pub fn call_connected(mtm: MainThreadMarker) {
    with_callkit(mtm, |callkit| {
        let Some(call) = callkit.call.as_mut() else { return };
        if call.incoming {
            if !call.answered {
                call.answered = true;
                // SAFETY: valid arguments; the delegate sees `answered`
                // already set and does not echo the answer back.
                unsafe {
                    let action = CXAnswerCallAction::initWithCallUUID(CXAnswerCallAction::alloc(), &call.uuid);
                    callkit.controller.requestTransactionWithAction_completion(&action, &log_error("answering in CallKit"));
                }
            }
        } else {
            // SAFETY: a report about a call CallKit knows.
            unsafe { callkit.provider.reportOutgoingCallWithUUID_connectedAtDate(&call.uuid, None) };
        }
    });
}

/// Reports the incoming call a VoIP push stands for. Must happen before
/// the push handler returns.
fn report_push_call(mtm: MainThreadMarker) {
    let reported = with_callkit(mtm, |callkit| {
        let uuid = NSUUID::new();
        let busy = callkit.call.is_some();
        let refused_uuid = uuid.clone();
        // If iOS refuses the call (Do Not Disturb, Focus, the caller
        // blocked), it is not shown and must not be kept as the call.
        let completion = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: CallKit passes nil or a live error.
            let Some(error) = (unsafe { error.as_ref() }) else { return };
            log::info!("iOS did not show the incoming call: {error}");
            CALLKIT.with(|slot| {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    if let Some(callkit) = slot.as_mut() {
                        if callkit.call.as_ref().is_some_and(|c| c.uuid == refused_uuid) {
                            callkit.call = None;
                            post(PlatformEvent::CallAction(CallAction::Decline));
                        }
                    }
                }
            });
        });
        // SAFETY: valid arguments; the completion runs later, on the main
        // queue, outside this borrow.
        unsafe {
            callkit.provider.reportNewIncomingCallWithUUID_update_completion(&uuid, &update_for(UNKNOWN_CALLER), &completion);
            if busy {
                // Already in a call. The push still had to be reported;
                // end it at once and let the shared code, which gets the
                // push regardless, decide what a second caller means.
                callkit.provider.reportCallWithUUID_endedAtDate_reason(&uuid, None, CXCallEndedReason::Unanswered);
            }
        }
        if !busy {
            callkit.call = Some(Call { uuid, incoming: true, answered: false });
        }
    });
    if reported.is_none() {
        log::error!("could not report the push's call to CallKit; iOS may stop delivering VoIP pushes");
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AntiphonCallKitDelegate"]
    struct ProviderDelegate;

    unsafe impl NSObjectProtocol for ProviderDelegate {}

    unsafe impl CXProviderDelegate for ProviderDelegate {
        /// CallKit dropped every call (its daemon restarted, say).
        #[unsafe(method(providerDidReset:))]
        fn did_reset(&self, _provider: &CXProvider) {
            let had_call = with_callkit(self.mtm(), |callkit| callkit.call.take().is_some()).unwrap_or(false);
            if had_call {
                post(PlatformEvent::CallAction(CallAction::HangUp));
            }
        }

        #[unsafe(method(provider:performStartCallAction:))]
        fn start_call(&self, provider: &CXProvider, action: &CXStartCallAction) {
            // SAFETY: plain calls on objects CallKit passed in.
            unsafe {
                action.fulfill();
                provider.reportOutgoingCallWithUUID_startedConnectingAtDate(&action.callUUID(), None);
            }
        }

        #[unsafe(method(provider:performAnswerCallAction:))]
        fn answer_call(&self, _provider: &CXProvider, action: &CXAnswerCallAction) {
            let answered_here = with_callkit(self.mtm(), |callkit| match callkit.call.as_mut() {
                Some(call) if !call.answered => {
                    call.answered = true;
                    true
                }
                _ => false,
            })
            .unwrap_or(false);
            if answered_here {
                post(PlatformEvent::CallAction(CallAction::Answer));
            }
            // SAFETY: a plain call on the action CallKit passed in.
            unsafe { action.fulfill() };
        }

        #[unsafe(method(provider:performEndCallAction:))]
        fn end_call(&self, _provider: &CXProvider, action: &CXEndCallAction) {
            let ended = with_callkit(self.mtm(), |callkit| callkit.call.take()).flatten();
            if let Some(call) = ended {
                let what = if call.incoming && !call.answered { CallAction::Decline } else { CallAction::HangUp };
                post(PlatformEvent::CallAction(what));
            }
            // SAFETY: a plain call on the action CallKit passed in.
            unsafe { action.fulfill() };
        }

        #[unsafe(method(provider:performSetMutedCallAction:))]
        fn set_muted(&self, _provider: &CXProvider, action: &CXSetMutedCallAction) {
            // SAFETY: plain calls on the action CallKit passed in.
            unsafe {
                post(PlatformEvent::CallAction(CallAction::Mute(action.isMuted())));
                action.fulfill();
            }
        }
    }
);

impl ProviderDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AntiphonPushKitDelegate"]
    struct PushDelegate;

    unsafe impl NSObjectProtocol for PushDelegate {}

    unsafe impl PKPushRegistryDelegate for PushDelegate {
        #[unsafe(method(pushRegistry:didUpdatePushCredentials:forType:))]
        fn did_update(&self, _registry: &PKPushRegistry, credentials: &PKPushCredentials, _kind: &PKPushType) {
            // SAFETY: a property read.
            let token = unsafe { credentials.token() }.to_vec();
            let hex: String = token.iter().map(|b| format!("{b:02x}")).collect();
            log::info!("PushKit token updated");
            post(PlatformEvent::PushRegistered(hex));
        }

        #[unsafe(method(pushRegistry:didReceiveIncomingPushWithPayload:forType:withCompletionHandler:))]
        fn did_receive(
            &self,
            _registry: &PKPushRegistry,
            payload: &PKPushPayload,
            _kind: &PKPushType,
            completion: &DynBlock<dyn Fn()>,
        ) {
            // The call first, unconditionally: see the module comment.
            report_push_call(self.mtm());
            match sealed_bytes(payload) {
                Some(bytes) => post(PlatformEvent::Push(bytes)),
                None => log::warn!("a VoIP push without a readable \"p\" field"),
            }
            completion.call(());
        }

        #[unsafe(method(pushRegistry:didInvalidatePushTokenForType:))]
        fn did_invalidate(&self, _registry: &PKPushRegistry, _kind: &PKPushType) {
            log::info!("PushKit token invalidated");
        }
    }
);

impl PushDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// The sealed payload: base64 under `"p"` in the push's JSON.
fn sealed_bytes(payload: &PKPushPayload) -> Option<Vec<u8>> {
    // SAFETY: a property read.
    let dictionary = unsafe { payload.dictionaryPayload() };
    let key: &AnyObject = ns_string!("p");
    let value = dictionary.objectForKey(key)?.downcast::<NSString>().ok()?;
    BASE64.decode(value.to_string()).ok()
}

/// Registers for VoIP pushes. The token arrives at the delegate, and with
/// it [`PlatformEvent::PushRegistered`]; if PushKit already has one, it is
/// posted at once.
pub fn register_push(mtm: MainThreadMarker) {
    NSUserDefaults::standardUserDefaults().setBool_forKey(true, &NSString::from_str(PUSH_ENABLED_KEY));
    start_push(mtm);
}

/// At launch: registers again if the user turned push on before, since
/// PushKit only delivers to a registry that exists in this process.
pub fn restore_push(mtm: MainThreadMarker) {
    if NSUserDefaults::standardUserDefaults().boolForKey(&NSString::from_str(PUSH_ENABLED_KEY)) {
        start_push(mtm);
    }
}

fn start_push(mtm: MainThreadMarker) {
    PUSH.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else { return };
        if slot.is_some() {
            return;
        }
        // SAFETY: a nil queue means the main queue, where the delegate must
        // run; the push type is a constant from PushKit.
        unsafe {
            let registry = PKPushRegistry::initWithQueue(PKPushRegistry::alloc(), None);
            let delegate = PushDelegate::new(mtm);
            registry.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            registry.setDesiredPushTypes(Some(&NSSet::from_slice(&[PKPushTypeVoIP])));
            if let Some(token) = registry.pushTokenForType(PKPushTypeVoIP) {
                let hex: String = token.to_vec().iter().map(|b| format!("{b:02x}")).collect();
                post(PlatformEvent::PushRegistered(hex));
            }
            *slot = Some(Push { _registry: registry, _delegate: delegate });
        }
    });
}
