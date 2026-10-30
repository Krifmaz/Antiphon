package org.antiphon.app;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageInstaller;
import android.util.Log;

/**
 * Android's answer to an update install session that
 * AntiphonActivity.installPackage committed. The first answer is nearly
 * always "pending user action": the intent it carries opens the system's
 * confirmation screen (which, the first time, also asks to let Stoop
 * install apps), and is started here. Success replaces the app, which
 * Android restarts; a failure or a cancelled confirmation is logged, and
 * the person can look for the update again from Settings > Updates.
 *
 * Not exported: only the system's installer, through the PendingIntent the
 * activity made, sends to it.
 */
public class InstallReceiver extends BroadcastReceiver {
    /** Shows the confirmation, or logs how the install ended. */
    @Override
    public void onReceive(Context context, Intent intent) {
        int status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE);
        if (status == PackageInstaller.STATUS_PENDING_USER_ACTION) {
            Intent confirm = intent.getParcelableExtra(Intent.EXTRA_INTENT);
            if (confirm == null) {
                Log.w(AntiphonActivity.TAG, "the installer asked for a confirmation it did not include");
                return;
            }
            confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            try {
                context.startActivity(confirm);
            } catch (RuntimeException e) {
                Log.w(AntiphonActivity.TAG, "could not show the update's confirmation", e);
            }
        } else if (status != PackageInstaller.STATUS_SUCCESS) {
            Log.w(AntiphonActivity.TAG, "the update was not installed: "
                    + intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE));
        }
    }
}
