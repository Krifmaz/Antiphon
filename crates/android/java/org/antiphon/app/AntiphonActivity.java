package org.antiphon.app;

import android.Manifest;
import android.app.NativeActivity;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.PowerManager;
import android.provider.OpenableColumns;
import android.provider.Settings;
import android.util.Log;

/**
 * The platform's NativeActivity, plus the few callbacks only an Activity
 * subclass receives: a picked document comes back through
 * onActivityResult, and a notification button through onNewIntent. All
 * the app itself is still Rust, started by NativeActivity as before.
 *
 * Rust calls the public methods here through JNI, from its own thread, so
 * anything that touches the UI is posted to the UI thread.
 */
public class AntiphonActivity extends NativeActivity {
    /** The logcat tag for the Java side, the same as Rust's. */
    static final String TAG = "antiphon";

    /** Opens the activity and answers the ringing call. */
    static final String ACTION_ANSWER = "org.antiphon.app.ANSWER";
    /** Opens the activity over the lock screen to show a ringing call. */
    static final String ACTION_RINGING = "org.antiphon.app.RINGING";

    /**
     * The request code for the document picker, so onActivityResult can
     * tell its answer apart. 1 is the microphone request Rust makes.
     */
    private static final int PICK_FILE = 2;
    /** The request code for the notification permission prompt. */
    private static final int NOTIFICATION_PERMISSION = 3;

    /**
     * Whether the app is on screen, where its own UI is the ring. Written
     * on the main thread, read on Rust's, hence volatile.
     */
    static volatile boolean inFront;

    /** The network watcher, kept so it can be unregistered; null when none. */
    private ConnectivityManager.NetworkCallback networkCallback;

    /** The activity is in front: from now on, a call rings in the app itself. */
    @Override
    protected void onResume() {
        super.onResume();
        inFront = true;
    }

    /** The activity is leaving the front: calls ring as a notification again. */
    @Override
    protected void onPause() {
        inFront = false;
        super.onPause();
    }

    /**
     * Starts the Rust app (through NativeActivity), and before anything can
     * need one, the notification channels and the network watcher. Also
     * handles the intent that launched it, which may be a call's Answer.
     */
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        CallService.createChannels(this);
        watchNetwork();
        handle(getIntent());
    }

    /** Stops watching the network, which would otherwise leak this activity. */
    @Override
    protected void onDestroy() {
        if (networkCallback != null) {
            try {
                ((ConnectivityManager) getSystemService(Context.CONNECTIVITY_SERVICE))
                        .unregisterNetworkCallback(networkCallback);
            } catch (RuntimeException e) {
                Log.w(TAG, "could not stop watching the network", e);
            }
            networkCallback = null;
        }
        super.onDestroy();
    }

    /**
     * singleTask in the manifest, so a notification's intent arrives here
     * instead of starting a second copy of the activity -- and a second
     * copy of the Rust app.
     */
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        handle(intent);
    }

    /**
     * Acts on the intent that opened the activity: Answer from the
     * incoming-call alert answers the call and shows it over the lock
     * screen; a tap on the alert itself only shows it. Main thread.
     */
    private void handle(Intent intent) {
        if (intent == null) return;
        String action = intent.getAction();
        if (ACTION_ANSWER.equals(action)) {
            CallService.ring(this, null);
            showWhenLocked(true);
            Native.callAction(Native.ANSWER, false);
        } else if (ACTION_RINGING.equals(action)) {
            showWhenLocked(true);
        }
    }

    /**
     * Lets the activity appear over the lock screen and wake the screen.
     * Only while a call is ringing or running: at any other time the app
     * holds an identity and a recovery phrase, which do not belong on a
     * locked phone. CallService turns it off again when the call ends.
     */
    void showWhenLocked(boolean on) {
        if (Build.VERSION.SDK_INT < 27) return;
        runOnUiThread(new Runnable() {
            /** Window flags may only be changed on the UI thread. */
            @Override
            public void run() {
                setShowWhenLocked(on);
                setTurnScreenOn(on);
            }
        });
    }

    /**
     * Opens the system's document picker. The answer comes to
     * onActivityResult. Called through JNI from Rust's thread, so the
     * picker is started from the UI thread.
     */
    public void pickFile() {
        runOnUiThread(new Runnable() {
            /** Starts the picker; a failure counts as a cancelled pick. */
            @Override
            public void run() {
                Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                intent.addCategory(Intent.CATEGORY_OPENABLE);
                intent.setType("*/*");
                intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION
                        | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
                try {
                    startActivityForResult(intent, PICK_FILE);
                } catch (RuntimeException e) {
                    // No document provider at all, which some stripped-down
                    // builds manage.
                    Log.w(TAG, "could not open the file picker", e);
                    Native.filePickCancelled();
                }
            }
        });
    }

    /**
     * The document picker's answer, on the main thread. Keeps read access
     * to the document across restarts, looks up its display name and size
     * in bytes, and hands them to Rust; anything but a document is a
     * cancel.
     */
    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != PICK_FILE) return;
        Uri uri = data == null ? null : data.getData();
        if (resultCode != RESULT_OK || uri == null) {
            Native.filePickCancelled();
            return;
        }
        // Without this the grant ends with the activity, and a file shared
        // or seeded yesterday could not be read after a restart.
        try {
            getContentResolver().takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION);
        } catch (SecurityException e) {
            Log.w(TAG, "the picked document cannot be kept across restarts", e);
        }
        String name = uri.getLastPathSegment();
        long size = -1;
        try (Cursor cursor = getContentResolver().query(uri,
                new String[] {OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE}, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                if (!cursor.isNull(0)) name = cursor.getString(0);
                if (!cursor.isNull(1)) size = cursor.getLong(1);
            }
        } catch (RuntimeException e) {
            Log.w(TAG, "could not read the picked document's name and size", e);
        }
        Native.filePicked(uri.toString(), name == null ? "file" : name, size);
    }

    /**
     * Android 13 and later hide every notification, the ongoing call
     * included, until the user allows them. Asked when something first
     * needs one rather than at launch. Callable from any thread.
     */
    public void requestNotificationPermission() {
        if (Build.VERSION.SDK_INT < 33) return;
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) return;
        runOnUiThread(new Runnable() {
            /** Permission prompts must be started from the UI thread. */
            @Override
            public void run() {
                requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, NOTIFICATION_PERMISSION);
            }
        });
    }

    /**
     * Asks to be left out of battery optimisation, so Doze does not cut the
     * network while the app listens for calls. The system shows its own
     * yes/no dialog; nothing happens if the app is already exempt. Called
     * through JNI from Rust's thread.
     */
    public void requestBatteryExemption() {
        PowerManager power = (PowerManager) getSystemService(Context.POWER_SERVICE);
        if (power == null || power.isIgnoringBatteryOptimizations(getPackageName())) return;
        runOnUiThread(new Runnable() {
            /** Opens the system's dialog, from the UI thread. */
            @Override
            public void run() {
                Intent intent = new Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                        Uri.parse("package:" + getPackageName()));
                try {
                    startActivity(intent);
                } catch (RuntimeException e) {
                    Log.w(TAG, "could not ask for a battery exemption", e);
                }
            }
        });
    }

    /**
     * Tells Rust whenever the default network changes, so "only on Wi-Fi"
     * limits follow the phone from Wi-Fi to mobile data and back. A
     * NetworkCallback is an abstract class, which native code cannot
     * subclass.
     */
    private void watchNetwork() {
        ConnectivityManager connectivity = (ConnectivityManager) getSystemService(Context.CONNECTIVITY_SERVICE);
        if (connectivity == null) return;
        networkCallback = new ConnectivityManager.NetworkCallback() {
            /**
             * Any change to the default network, signal strength included,
             * on ConnectivityManager's own thread; Rust filters it.
             */
            @Override
            public void onCapabilitiesChanged(Network network, NetworkCapabilities capabilities) {
                Native.networkChanged();
            }

            /** The default network is gone, on the same thread as above. */
            @Override
            public void onLost(Network network) {
                Native.networkChanged();
            }
        };
        try {
            connectivity.registerDefaultNetworkCallback(networkCallback);
        } catch (RuntimeException e) {
            Log.w(TAG, "could not watch the network", e);
            networkCallback = null;
        }
    }
}
