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

    /** Not instantiable: only a holder for the native methods. */
    private Native() {}

    /**
     * Values for {@link #callAction}, matching the constants of the same
     * names in native.rs; a change on one side must be made on both. This
     * one is the Answer button on the incoming-call alert.
     */
    static final int ANSWER = 0;
    /** The Decline button on the incoming-call alert. */
    static final int DECLINE = 1;
    /** The Hang up button on the ongoing-call notification. */
    static final int HANG_UP = 2;
    /** The Mute or Unmute button; the new state goes in {@code on}. */
    static final int MUTE = 3;

    /**
     * A document the user picked. {@code uri} is its content URI, which
     * Rust keeps as the file's handle; {@code size} is in bytes, or -1 when
     * the provider does not say.
     */
    static native void filePicked(String uri, String name, long size);

    /** The picker was dismissed without a choice, or could not open. */
    static native void filePickCancelled();

    /** A call button pressed outside the app; {@code on} is for MUTE. */
    static native void callAction(int action, boolean on);

    /**
     * The default network changed; Rust looks up what it is now, and passes
     * it on only if its kind (Wi-Fi, metered, none) changed.
     */
    static native void networkChanged();

    /**
     * The UnifiedPush distributor gave this app an endpoint URL, which Rust
     * hands to contacts so they can wake this device.
     */
    static native void pushRegistered(String endpoint);

    /**
     * A sealed wake-up arrived. Returns whether the app is running to act on
     * it; when it is not, the caller tells the user instead.
     */
    static native boolean push(byte[] sealed);
}
