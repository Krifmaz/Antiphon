package org.antiphon.app;

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.os.Bundle;
import android.os.Process;
import android.util.Log;

/**
 * Restarts the app's process. Runs in its own process (":restart" in the
 * manifest), so it outlives the process it was started from: the app's
 * process starts this, kills itself, and this then reopens the app in a
 * new process and ends its own.
 *
 * Needed because winit creates its event loop once per process. When the
 * app's process outlives its activity and a new activity opens in it, the
 * Rust side cannot start again; see AntiphonActivity.nativeStarted.
 */
public class RestartActivity extends Activity {
    /** The extra holding the intent to reopen the app with. */
    private static final String EXTRA_INTENT = "org.antiphon.app.RESTART_INTENT";

    /**
     * Starts the restart and ends the calling process. Does not return in
     * practice. Main thread.
     *
     * @param context the activity that cannot start the Rust app
     * @param launch the intent it was opened with, passed on so an Answer
     *     tapped on a notification still answers after the restart; null
     *     opens the app normally
     */
    static void restart(Context context, Intent launch) {
        Intent reopen = new Intent(context, AntiphonActivity.class);
        if (launch != null) {
            reopen.setAction(launch.getAction());
            reopen.setData(launch.getData());
            if (launch.getExtras() != null) reopen.putExtras(launch.getExtras());
        }
        reopen.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK);
        Intent intent = new Intent(context, RestartActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                .putExtra(EXTRA_INTENT, reopen);
        context.startActivity(intent);
        Process.killProcess(Process.myPid());
    }

    /**
     * Reopens the app, which lands in a fresh process because the old one
     * is gone by now, then ends this helper and its process.
     */
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Intent reopen = getIntent().getParcelableExtra(EXTRA_INTENT);
        if (reopen != null) {
            try {
                startActivity(reopen);
            } catch (RuntimeException e) {
                Log.w(AntiphonActivity.TAG, "could not reopen the app", e);
            }
        }
        finish();
        Process.killProcess(Process.myPid());
    }
}
