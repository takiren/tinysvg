//! Receives "open these files" requests from Finder / Dock / `open -a`.
//!
//! macOS does not pass such files via argv; they arrive as
//! `application:openURLs:` on the NSApplicationDelegate. winit 0.30 installs
//! its own delegate (`WinitApplicationDelegate`) and panics if it is replaced,
//! so instead we add the `application:openURLs:` method to winit's delegate
//! class at runtime. This must happen before the event loop runs, because
//! files opened at launch arrive before `applicationDidFinishLaunching`.

use std::sync::OnceLock;

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSArray, NSURL};

use crate::Opener;

static OPENER: OnceLock<Opener> = OnceLock::new();

unsafe extern "C-unwind" fn application_open_urls(
    _this: &AnyObject,
    _cmd: Sel,
    _application: &NSApplication,
    urls: &NSArray<NSURL>,
) {
    let Some(opener) = OPENER.get() else { return };
    for url in urls.iter() {
        if let Some(path) = url.to_file_path() {
            opener.open(path);
        }
    }
}

/// Must be called on the main thread, after the winit event loop is built
/// (so winit's delegate exists) and before it runs.
pub fn install_open_handler(opener: Opener) {
    if OPENER.set(opener).is_err() {
        return;
    }
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = app.delegate() else {
        eprintln!("tsvg: no NSApplication delegate; Finder open is disabled");
        return;
    };
    let cls: *const AnyClass = AsRef::<AnyObject>::as_ref(&*delegate).class();

    // SAFETY: the function signature matches the `v@:@@` type encoding of
    // `-application:(NSApplication *)app openURLs:(NSArray<NSURL *> *)urls`.
    unsafe {
        let imp: Imp = std::mem::transmute(
            application_open_urls
                as unsafe extern "C-unwind" fn(&AnyObject, Sel, &NSApplication, &NSArray<NSURL>),
        );
        objc2::ffi::class_addMethod(
            cls as *mut AnyClass,
            sel!(application:openURLs:),
            imp,
            c"v@:@@".as_ptr(),
        );
        // AppKit caches which optional delegate methods exist when the
        // delegate is set; set it again so the new method is noticed.
        app.setDelegate(None);
        app.setDelegate(Some(&*delegate));
    }
}
