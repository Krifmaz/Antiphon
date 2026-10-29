package org.antiphon.app;

import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.util.Log;

import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.UUID;

/**
 * Push wake-ups through UnifiedPush: whichever distributor app the user has
 * installed (ntfy, NextPush, or one that relays FCM) holds the connection
 * to its server and hands each message to this receiver. That keeps
 * Google's SDK, and a Firebase project, out of the APK. The wire protocol
 * is a handful of broadcasts, written out here rather than pulled in as a
 * library: https://unifiedpush.org/developers/spec/android/
 *
 * Every message is a sealed wake-up (crates/crypto/src/push.rs). This class
 * never looks inside; it hands the bytes to Rust.
 */
public class PushReceiver extends BroadcastReceiver {
    /** The logcat tag, shared with the rest of the app. */
    private static final String TAG = AntiphonActivity.TAG;

    /** Sent to the distributor: asks it for an endpoint for this app. */
    private static final String REGISTER = "org.unifiedpush.android.distributor.REGISTER";
    /** Sent to the distributor: a message was received and need not be resent. */
    private static final String MESSAGE_ACK = "org.unifiedpush.android.distributor.MESSAGE_ACK";
    /**
     * The feature asked for at registration: messages delivered as raw
     * bytes, since a sealed wake-up is binary and not valid text.
     */
    private static final String FEATURE_BYTES = "org.unifiedpush.android.distributor.feature.BYTES_MESSAGE";

    /** From the distributor: the endpoint to give out, sent on every registration. */
    private static final String NEW_ENDPOINT = "org.unifiedpush.android.connector.NEW_ENDPOINT";
    /** From the distributor: it refused the registration, with a reason. */
    private static final String REGISTRATION_FAILED = "org.unifiedpush.android.connector.REGISTRATION_FAILED";
    /** From the distributor: this app's registration was removed. */
    private static final String UNREGISTERED = "org.unifiedpush.android.connector.UNREGISTERED";
    /** From the distributor: a push message for this app. */
    private static final String MESSAGE = "org.unifiedpush.android.connector.MESSAGE";

    /** The SharedPreferences file holding the registration below. */
    private static final String PREFS = "unifiedpush";
    /** This registration's secret token; see {@link #register}. */
    private static final String KEY_TOKEN = "token";
    /** The package name of the distributor registered with. */
    private static final String KEY_DISTRIBUTOR = "distributor";

    /**
     * Rust's register_push. Asks the distributor for an endpoint; it
     * answers with NEW_ENDPOINT, every time, even when it already gave one.
     * Returns false when no distributor is installed.
     *
     * Called through JNI on Rust's thread, not the main thread; it only
     * reads preferences and sends a broadcast, which are safe there.
     */
    public static boolean register(Context context) {
        SharedPreferences prefs = prefs(context);
        String distributor = distributor(context, prefs.getString(KEY_DISTRIBUTOR, null));
        if (distributor == null) {
            Log.i(TAG, "no UnifiedPush distributor installed; push wake-ups are off");
            return false;
        }
        // The token is the secret that ties the distributor's messages to
        // this registration: anything can send this receiver a broadcast,
        // but only the distributor knows the token.
        String token = prefs.getString(KEY_TOKEN, null);
        if (token == null) token = UUID.randomUUID().toString();
        prefs.edit().putString(KEY_TOKEN, token).putString(KEY_DISTRIBUTOR, distributor).apply();

        Intent intent = new Intent(REGISTER)
                .setPackage(distributor)
                .putExtra("token", token)
                .putExtra("application", context.getPackageName())
                .putExtra("message", "Wakes Stoop for calls and messages")
                .putExtra("features", new String[] {FEATURE_BYTES})
                // Newer distributors identify the app by who created this
                // PendingIntent, which a spoofed "application" cannot fake.
                .putExtra("pi", PendingIntent.getBroadcast(context, 0,
                        new Intent("org.unifiedpush.dummy_app"), PendingIntent.FLAG_IMMUTABLE));
        try {
            context.sendBroadcast(intent);
            return true;
        } catch (RuntimeException e) {
            Log.w(TAG, "could not reach the UnifiedPush distributor", e);
            return false;
        }
    }

    /**
     * The distributor to use: the one used before if it is still
     * installed, otherwise the first one found. Choosing between several
     * is the user's call, but rare enough to leave to a later version.
     */
    private static String distributor(Context context, String previous) {
        List<ResolveInfo> found;
        try {
            found = context.getPackageManager().queryBroadcastReceivers(new Intent(REGISTER), 0);
        } catch (RuntimeException e) {
            Log.w(TAG, "could not look for UnifiedPush distributors", e);
            return null;
        }
        String first = null;
        for (ResolveInfo info : found) {
            if (info.activityInfo == null) continue;
            String name = info.activityInfo.packageName;
            if (name.equals(context.getPackageName())) continue;
            if (name.equals(previous)) return name;
            if (first == null) first = name;
        }
        return first;
    }

    /** The registration's preferences, private to this app. */
    private static SharedPreferences prefs(Context context) {
        return context.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    /**
     * Every broadcast from the distributor, on the main thread. Anything
     * without this registration's token is ignored, since any app can send
     * this receiver a broadcast. The work is kept short -- the bytes go
     * straight to Rust's queue -- because Android allows a receiver about
     * ten seconds before it treats the app as not responding.
     */
    @Override
    public void onReceive(Context context, Intent intent) {
        String action = intent.getAction();
        SharedPreferences prefs = prefs(context);
        String token = prefs.getString(KEY_TOKEN, null);
        if (action == null || token == null || !token.equals(intent.getStringExtra("token"))) {
            return;
        }
        switch (action) {
            case NEW_ENDPOINT: {
                String endpoint = intent.getStringExtra("endpoint");
                if (endpoint != null) Native.pushRegistered(endpoint);
                break;
            }
            case REGISTRATION_FAILED:
                Log.w(TAG, "UnifiedPush registration failed: " + intent.getStringExtra("reason"));
                break;
            case UNREGISTERED:
                Log.i(TAG, "UnifiedPush distributor dropped this app");
                prefs.edit().remove(KEY_TOKEN).remove(KEY_DISTRIBUTOR).apply();
                break;
            case MESSAGE: {
                byte[] sealed = intent.getByteArrayExtra("bytesMessage");
                if (sealed == null) {
                    String text = intent.getStringExtra("message");
                    if (text == null) break;
                    sealed = text.getBytes(StandardCharsets.UTF_8);
                }
                acknowledge(context, prefs.getString(KEY_DISTRIBUTOR, null), token, intent.getStringExtra("id"));
                if (!Native.push(sealed)) {
                    // Nothing to open the seal with until the app runs, so
                    // say only that something came. The wake-up waits in
                    // the queue for the app.
                    CallService.notify(context, "Stoop", "Someone may be trying to reach you. Open Stoop to answer.");
                }
                break;
            }
            default:
                break;
        }
    }

    /** Tells the distributor the message arrived, so it is not resent. */
    private static void acknowledge(Context context, String distributor, String token, String id) {
        if (distributor == null || id == null) return;
        try {
            context.sendBroadcast(new Intent(MESSAGE_ACK)
                    .setPackage(distributor)
                    .putExtra("token", token)
                    .putExtra("id", id));
        } catch (RuntimeException e) {
            Log.w(TAG, "could not acknowledge a push", e);
        }
    }
}
