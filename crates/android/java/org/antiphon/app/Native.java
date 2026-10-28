package org.antiphon.app;

/**
 * The Rust side, as Java sees it. Each method is implemented in
 * crates/android/src/native.rs and does no more than queue an event for the
 * shared app, so all of them are safe to call from any thread.
 *
 * NativeActivity opens the library itself, but with dlopen, which does not
 * make its Java_ functions visible to the JVM. Loading it here as well makes
 * them visible, and also loads it when a push or a notification button
 * starts the process without the activity.
 */
final class Native {
    static {
        System.loadLibrary("antiphon_android");
    }

    private Native() {}

    /** Values for {@link #callAction}, matching native.rs. */
    static final int ANSWER = 0;
    static final int DECLINE = 1;
    static final int HANG_UP = 2;
    static final int MUTE = 3;

    /** A document the user picked. {@code size} is -1 when unknown. */
    static native void filePicked(String uri, String name, long size);

    static native void filePickCancelled();

    /** A call button pressed outside the app; {@code on} is for MUTE. */
    static native void callAction(int action, boolean on);

    /** The default network changed; Rust looks up what it is now. */
    static native void networkChanged();

    static native void pushRegistered(String endpoint);

    /**
     * A sealed wake-up arrived. Returns whether the app is running to act on
     * it; when it is not, the caller tells the user instead.
     */
    static native boolean push(byte[] sealed);
}
