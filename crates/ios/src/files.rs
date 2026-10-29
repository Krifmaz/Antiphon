//! Picking files to send, and reading them later by handle.
//!
//! The document picker opens files in place rather than copying them in:
//! a large video copied into the app's container would double its space
//! for the length of a transfer. A file opened in place is reachable only
//! through a security-scoped URL, which stops working when the app exits.
//! So the handle given to the shared code is a bookmark for that URL,
//! base64-encoded; [`read`] resolves it and asks for access each time,
//! which survives a restart and so lets a transfer resume.

use std::cell::RefCell;
use std::io;

use antiphon_mobile::platform::{self, PickedFile, PlatformEvent};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSArray, NSData, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions, NSURL};
use objc2_ui_kit::{UIDocumentPickerDelegate, UIDocumentPickerViewController};
use objc2_uniform_type_identifiers::UTTypeItem;

thread_local! {
    /// The picker holds its delegate weakly, so it lives here while the
    /// picker is up.
    static DELEGATE: RefCell<Option<Retained<PickerDelegate>>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AntiphonDocumentPickerDelegate"]
    struct PickerDelegate;

    unsafe impl NSObjectProtocol for PickerDelegate {}

    unsafe impl UIDocumentPickerDelegate for PickerDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _controller: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            let event = match urls.firstObject().map(|url| picked(&url)) {
                Some(Ok(file)) => PlatformEvent::FilePicked(file),
                Some(Err(e)) => {
                    log::warn!("could not keep access to the picked file: {e}");
                    PlatformEvent::FilePickCancelled
                }
                None => PlatformEvent::FilePickCancelled,
            };
            finish(event);
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn was_cancelled(&self, _controller: &UIDocumentPickerViewController) {
            finish(PlatformEvent::FilePickCancelled);
        }
    }
);

impl PickerDelegate {
    /// A fresh delegate. Main thread only, as the class is `MainThreadOnly`.
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// Ends a pick: drops the delegate the picker held weakly, posts the
/// result, and wakes the event loop to take it. Main thread only.
fn finish(event: PlatformEvent) {
    DELEGATE.with(|delegate| delegate.borrow_mut().take());
    platform::post(event);
    crate::shell::wake();
}

/// Stops using a security-scoped URL when dropped, so an early return
/// cannot leak the access (iOS caps how many an app may hold).
///
/// The fields are the URL and whether access was granted, since only a
/// granted access may be stopped.
struct Access<'a>(&'a NSURL, bool);

impl<'a> Access<'a> {
    /// Starts accessing `url`. Not every URL needs it (one inside the app's
    /// own container does not), so a refusal is not an error: the read
    /// that follows is what fails, if anything does.
    fn start(url: &'a NSURL) -> Self {
        // SAFETY: a plain call on a valid URL.
        Self(url, unsafe { url.startAccessingSecurityScopedResource() })
    }
}

impl Drop for Access<'_> {
    fn drop(&mut self) {
        if self.1 {
            // SAFETY: balances the successful start above.
            unsafe { self.0.stopAccessingSecurityScopedResource() };
        }
    }
}

/// The file-system path of a file URL, as std::fs wants it.
fn path_of(url: &NSURL) -> io::Result<std::path::PathBuf> {
    url.path().map(|p| p.to_string().into()).ok_or_else(|| io::Error::other("the file has no path"))
}

/// The bookmark, name and size of a file the picker returned. The
/// bookmark has to be made while the URL's access is held.
fn picked(url: &NSURL) -> io::Result<PickedFile> {
    let _access = Access::start(url);
    let bookmark = url
        .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
            NSURLBookmarkCreationOptions::empty(),
            None,
            None,
        )
        .map_err(|e| io::Error::other(e.to_string()))?;
    let path = path_of(url)?;
    let name = url
        .lastPathComponent()
        .map(|n| n.to_string())
        .unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let size = std::fs::metadata(&path).ok().map(|m| m.len());
    Ok(PickedFile { handle: BASE64.encode(bookmark.to_vec()), name, size })
}

/// Opens the system's document picker over the app.
pub fn pick(mtm: MainThreadMarker) {
    let Some(mut presenter) = crate::screen::key_window(mtm).and_then(|w| w.rootViewController()) else {
        log::warn!("no view controller to show the file picker from");
        finish(PlatformEvent::FilePickCancelled);
        return;
    };
    // Present over whatever is already up, or UIKit refuses.
    while let Some(next) = presenter.presentedViewController() {
        presenter = next;
    }
    // SAFETY: a constant from UniformTypeIdentifiers.
    let types = NSArray::from_slice(&[unsafe { UTTypeItem }]);
    let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(
        UIDocumentPickerViewController::alloc(mtm),
        &types,
        false,
    );
    picker.setAllowsMultipleSelection(false);
    let delegate = PickerDelegate::new(mtm);
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|slot| *slot.borrow_mut() = Some(delegate));
    presenter.presentViewController_animated_completion(&picker, true, None);
}

/// Reads a picked file by its handle. Safe from any thread: NSURL and
/// bookmark resolution are.
pub fn read(handle: &str) -> io::Result<Vec<u8>> {
    let bookmark = BASE64.decode(handle).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let data = NSData::with_bytes(&bookmark);
    let mut stale = Bool::NO;
    // SAFETY: `stale` is a valid out-pointer for the call's duration.
    let url = unsafe {
        NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
            &data,
            NSURLBookmarkResolutionOptions::empty(),
            None,
            &mut stale,
        )
    }
    .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e.to_string()))?;
    if stale.as_bool() {
        // Still resolves this time; the file was moved or renamed. The
        // handle cannot be replaced from here, so it may fail next time.
        log::info!("a picked file's bookmark is stale");
    }
    let _access = Access::start(&url);
    std::fs::read(path_of(&url)?)
}
