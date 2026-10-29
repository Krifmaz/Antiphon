package org.antiphon.app;

import android.Manifest;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.media.AudioAttributes;
import android.media.RingtoneManager;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.util.Log;

import java.util.concurrent.atomic.AtomicInteger;

/**
 * Keeps the process alive, with its call or its listening socket, while
 * the app is not on screen. Android kills a background app within minutes
 * and cuts its microphone at once; a foreground service with an ongoing
 * notification is the one sanctioned way to say "a call is running".
 *
 * The call itself stays in Rust on the NativeActivity's thread. This only
 * holds the process up, shows the notification, and passes its Hang up and
 * Mute buttons back. Rust drives it through {@link #update}, called with
 * the whole of what has to keep running every time that changes.
 *
 * It also owns the app's notifications in general -- the incoming-call
 * alert and plain ones from {@link #notify} -- since it already owns their
 * channels.
 */
public class CallService extends Service {
    /** The logcat tag, shared with the rest of the app. */
    private static final String TAG = AntiphonActivity.TAG;

    /** Starts the service, or brings it up to date with what Rust said. */
    static final String ACTION_UPDATE = "org.antiphon.app.UPDATE";
    /** The Hang up button on the ongoing-call notification. */
    static final String ACTION_HANG_UP = "org.antiphon.app.HANG_UP";
    /** The Mute/Unmute button on the ongoing-call notification. */
    static final String ACTION_MUTE = "org.antiphon.app.MUTE";
    /** The Decline button on the incoming-call alert. */
    static final String ACTION_DECLINE = "org.antiphon.app.DECLINE";

    /** Notification channel for the ongoing call; low importance, silent. */
    private static final String CHANNEL_CALL = "call";
    /** Notification channel for the incoming-call alert; rings. */
    private static final String CHANNEL_INCOMING = "incoming";
    /** Notification channel for listening and transfers; minimal. */
    private static final String CHANNEL_BACKGROUND = "background";
    /** Notification channel for plain notifications from {@link #notify}. */
    private static final String CHANNEL_MESSAGES = "messages";

    /** The foreground service's own notification, replaced on each update. */
    private static final int ID_ONGOING = 1;
    /** The incoming-call alert; one at a time, withdrawn by id. */
    private static final int ID_RINGING = 2;
    /**
     * The next id for a plain notification, so each is added rather than
     * replacing the last. Starts well clear of the fixed ids above, and is
     * atomic because notify may be called from several threads.
     */
    private static final AtomicInteger nextId = new AtomicInteger(100);

    /** The incoming-call alert gives up after this long, as a phone call does. */
    private static final long RING_TIMEOUT_MS = 60_000;

    /**
     * Guards the fields below: what has to keep running, as Rust last said.
     * Written on Rust's thread, read on the main thread.
     */
    private static final Object lock = new Object();
    /** Who the call is with ("" when unknown), or null when there is none. */
    private static String call;
    /** Whether the app is listening for incoming calls. */
    private static boolean listening;
    /** How many file transfers are running. */
    private static int transfers;
    /**
     * Whether the notification's Mute button has the call muted. The
     * service flips it itself and tells Rust; reset when a call starts.
     */
    private static boolean muted;
    /** When the call started, in milliseconds since the epoch, for the chronometer. */
    private static long callStarted;

    /** Posts to the main thread, where the service is started and updated. */
    private static final Handler main = new Handler(Looper.getMainLooper());
    /** The running service, if any. Main thread only. */
    private static CallService running;

    /** Keeps the CPU awake during a call; null when not held. */
    private PowerManager.WakeLock wakeLock;
    /** Keeps Wi-Fi out of power save during a call; null when not held. */
    private WifiManager.WifiLock wifiLock;

    /**
     * Rust's set_background_work. {@code callWith} is who a call is with,
     * or null when there is none. Starts the service when something has to
     * keep running, updates its notification while it runs, and stops it
     * when nothing does.
     *
     * Called through JNI on Rust's thread. The state is stored under
     * {@code lock} at once; starting or updating the service is posted to
     * the main thread, where Service calls belong.
     */
    public static void update(Context context, String callWith, boolean listen, int transferCount) {
        final Context app = context.getApplicationContext();
        boolean idle;
        synchronized (lock) {
            if (callWith != null && call == null) {
                muted = false;
                callStarted = System.currentTimeMillis();
            }
            call = callWith;
            listening = listen;
            transfers = transferCount;
            idle = isIdle();
        }
        if (context instanceof AntiphonActivity) {
            AntiphonActivity activity = (AntiphonActivity) context;
            if (callWith == null) activity.showWhenLocked(false);
            if (!idle) activity.requestNotificationPermission();
        }
        main.post(new Runnable() {
            /**
             * On the main thread: updates the running service, or starts one
             * if there is something to keep running and none is up.
             */
            @Override
            public void run() {
                if (running != null) {
                    running.apply();
                    return;
                }
                synchronized (lock) {
                    if (isIdle()) return;
                }
                createChannels(app);
                try {
                    app.startForegroundService(new Intent(app, CallService.class).setAction(ACTION_UPDATE));
                } catch (RuntimeException e) {
                    // Android 12 and later refuse to start one from the
                    // background. Calls and listening start from a tap, so this
                    // is a transfer that began while the app was out of sight.
                    Log.w(TAG, "could not start the background service", e);
                }
            }
        });
    }

    /** Whether nothing has to keep running. Callers must hold {@code lock}. */
    private static boolean isIdle() {
        return call == null && !listening && transfers == 0;
    }

    /** Records this as the running service. Main thread, as all lifecycle calls are. */
    @Override
    public void onCreate() {
        super.onCreate();
        running = this;
    }

    /** Lets go of the wake and Wi-Fi locks, and forgets this service. */
    @Override
    public void onDestroy() {
        releaseLocks();
        if (running == this) running = null;
        super.onDestroy();
    }

    /**
     * The user swiped the app away from Recents while this service was
     * running. The activity, and the Rust app's event loop with it, are
     * gone and cannot be started again in this process, so the process
     * ends here rather than lingering in a state no new activity can use.
     * A call in progress ends with it, as swiping the app away has always
     * ended one (the call lives on the activity's thread).
     */
    @Override
    public void onTaskRemoved(Intent rootIntent) {
        Log.i(TAG, "task removed; ending the process");
        stopForeground(STOP_FOREGROUND_REMOVE);
        stopSelf();
        android.os.Process.killProcess(android.os.Process.myPid());
    }

    /** Not a bound service: started and stopped only. */
    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    /**
     * A start request, on the main thread: an update from Rust, or a button
     * on one of the notifications, passed on to Rust. Every path ends by
     * bringing the service in line with the latest state.
     */
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? null : intent.getAction();
        if (ACTION_HANG_UP.equals(action)) {
            Native.callAction(Native.HANG_UP, false);
        } else if (ACTION_MUTE.equals(action)) {
            boolean on;
            synchronized (lock) {
                muted = !muted;
                on = muted;
            }
            Native.callAction(Native.MUTE, on);
        } else if (ACTION_DECLINE.equals(action)) {
            ring(this, null);
            Native.callAction(Native.DECLINE, false);
        } else if (ACTION_UPDATE.equals(action)) {
            // Started with startForegroundService, which is a promise to
            // call startForeground within seconds even if, by now, there is
            // nothing left to do; breaking it crashes the app.
            boolean idle;
            synchronized (lock) {
                idle = isIdle();
            }
            if (idle) startForegroundCompat(build(null, false, 0, false), dataSyncType());
        }
        apply();
        // Not sticky: restarted without the app, there would be no call
        // to keep alive.
        return START_NOT_STICKY;
    }

    /**
     * Android 15 stops a dataSync service after six hours a day. It must
     * stop itself here, or the system crashes the app.
     */
    @Override
    public void onTimeout(int startId, int fgsType) {
        Log.w(TAG, "the system ended the background service after its time limit");
        stopForeground(STOP_FOREGROUND_REMOVE);
        stopSelf();
    }

    /**
     * Brings the notification and the service in line with what Rust last
     * said. Main thread only.
     */
    private void apply() {
        String c;
        boolean l;
        int t;
        boolean m;
        synchronized (lock) {
            c = call;
            l = listening;
            t = transfers;
            m = muted;
        }
        if (c == null && !l && t == 0) {
            // Forgotten now rather than in onDestroy: stopSelf takes effect
            // later, and an update arriving in between must start a new
            // service rather than revive this one on its way out.
            if (running == this) running = null;
            releaseLocks();
            stopForeground(STOP_FOREGROUND_REMOVE);
            stopSelf();
            return;
        }
        startForegroundCompat(build(c, l, t, m), types(c, l));
        if (c != null) acquireLocks();
        else releaseLocks();
    }

    /**
     * The service types for what is running. Android 14 requires each one
     * to be declared in the manifest and checks a permission for each.
     *
     * - A call is phoneCall (allowed by MANAGE_OWN_CALLS) and microphone.
     * - Listening is microphone, so a call that comes in while the app is
     *   out of sight can still use it -- Android grants the microphone to a
     *   background service only if the service claimed it while the app
     *   was in front -- plus specialUse, since "waiting for a call" is none
     *   of the listed uses.
     * - Transfers alone are dataSync.
     *
     * The microphone is only claimed once the user has allowed it; claiming
     * it without the permission throws.
     */
    private int types(String c, boolean l) {
        if (Build.VERSION.SDK_INT < 29) return 0;
        int types = 0;
        if ((c != null || l) && Build.VERSION.SDK_INT >= 30
                && checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
            types |= ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE;
        }
        if (c != null) {
            types |= ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL;
        } else if (l) {
            types |= Build.VERSION.SDK_INT >= 34
                    ? ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
                    : ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC;
        } else {
            types |= ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC;
        }
        return types;
    }

    /**
     * The dataSync service type, or 0 before Android 10, where types do not
     * exist. The type of last resort, which needs no extra permission.
     */
    private static int dataSyncType() {
        return Build.VERSION.SDK_INT >= 29 ? ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC : 0;
    }

    /**
     * startForeground, falling back step by step: the microphone type is
     * refused when the app is not in front, and any type can be refused by
     * a vendor's own rules. Losing the service type is better than losing
     * the notification, and that better than crashing.
     */
    private void startForegroundCompat(Notification notification, int types) {
        if (Build.VERSION.SDK_INT < 29) {
            startForeground(ID_ONGOING, notification);
            return;
        }
        try {
            startForeground(ID_ONGOING, notification, types);
            return;
        } catch (RuntimeException e) {
            Log.w(TAG, "foreground service types " + types + " refused", e);
        }
        int fallback = types & ~ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE;
        if (fallback == 0) fallback = dataSyncType();
        try {
            startForeground(ID_ONGOING, notification, fallback);
        } catch (RuntimeException e) {
            Log.w(TAG, "foreground service types " + fallback + " refused too", e);
            NotificationManager manager = getSystemService(NotificationManager.class);
            if (manager != null) manager.notify(ID_ONGOING, notification);
        }
    }

    /**
     * The ongoing notification for a call with {@code c} (null for none),
     * listening {@code l}, {@code t} transfers and mute state {@code m}. A
     * call shows a running timer and Mute and Hang up buttons; otherwise a
     * quiet line saying what is going on.
     */
    private Notification build(String c, boolean l, int t, boolean m) {
        Notification.Builder b = new Notification.Builder(this, c != null ? CHANNEL_CALL : CHANNEL_BACKGROUND)
                .setSmallIcon(R.drawable.ic_stat_antiphon)
                .setOngoing(true)
                .setOnlyAlertOnce(true)
                .setContentIntent(openApp(this));
        if (Build.VERSION.SDK_INT >= 31) {
            // Otherwise Android may hold the notification back for ten
            // seconds, and a short call would never show one.
            b.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        }
        if (c != null) {
            long started;
            synchronized (lock) {
                started = callStarted;
            }
            b.setCategory(Notification.CATEGORY_CALL)
                    .setContentTitle(c.isEmpty() ? "In a call" : "In a call with " + c)
                    .setContentText(m ? "Muted" : "Stoop")
                    .setWhen(started)
                    .setShowWhen(true)
                    .setUsesChronometer(true)
                    .addAction(action(this, m ? "Unmute" : "Mute", service(this, ACTION_MUTE)))
                    .addAction(action(this, "Hang up", service(this, ACTION_HANG_UP)));
        } else {
            b.setCategory(Notification.CATEGORY_SERVICE)
                    .setShowWhen(false)
                    .setContentTitle(l ? "Listening for calls" : "Transferring files");
            if (t > 0) b.setContentText(t == 1 ? "1 file transfer" : t + " file transfers");
        }
        return b.build();
    }

    /**
     * Rust's ring: shows the incoming-call alert, or withdraws it when
     * {@code from} is null. With the phone locked or the app out of sight
     * it opens the app full screen, as a phone call does; with the app in
     * front the app's own screen is the alert, so nothing is shown.
     * Callable from any thread: NotificationManager is thread-safe.
     */
    public static void ring(Context context, String from) {
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        if (manager == null) return;
        if (from == null || AntiphonActivity.inFront) {
            manager.cancel(ID_RINGING);
            return;
        }
        createChannels(context);
        PendingIntent show = activity(context, AntiphonActivity.ACTION_RINGING);
        Notification notification = new Notification.Builder(context, CHANNEL_INCOMING)
                .setSmallIcon(R.drawable.ic_stat_antiphon)
                .setCategory(Notification.CATEGORY_CALL)
                .setContentTitle("Incoming call")
                .setContentText(from.isEmpty() ? "Stoop" : from)
                .setOngoing(true)
                .setTimeoutAfter(RING_TIMEOUT_MS)
                .setContentIntent(show)
                // Android 14 grants this only to calling apps, and asks the
                // user otherwise; without it the alert is a heads-up banner.
                .setFullScreenIntent(show, true)
                .addAction(action(context, "Decline", service(context, ACTION_DECLINE)))
                .addAction(action(context, "Answer", activity(context, AntiphonActivity.ACTION_ANSWER)))
                .build();
        // Ring until answered, as a phone does, not once.
        notification.flags |= Notification.FLAG_INSISTENT;
        try {
            manager.notify(ID_RINGING, notification);
        } catch (RuntimeException e) {
            Log.w(TAG, "could not show the incoming call", e);
        }
    }

    /**
     * Rust's notify: a plain notification that opens the app when tapped.
     * Also used by PushReceiver when a push arrives and the app is not
     * running. Callable from any thread.
     */
    public static void notify(Context context, String title, String body) {
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        if (manager == null) return;
        if (context instanceof AntiphonActivity) {
            ((AntiphonActivity) context).requestNotificationPermission();
        }
        createChannels(context);
        Notification notification = new Notification.Builder(context, CHANNEL_MESSAGES)
                .setSmallIcon(R.drawable.ic_stat_antiphon)
                .setContentTitle(title)
                .setContentText(body)
                .setStyle(new Notification.BigTextStyle().bigText(body))
                .setAutoCancel(true)
                .setContentIntent(openApp(context))
                .build();
        try {
            manager.notify(nextId.getAndIncrement(), notification);
        } catch (RuntimeException e) {
            Log.w(TAG, "could not show a notification", e);
        }
    }

    /**
     * The notification channels, which the user can tune separately in
     * the system settings. Creating one that exists changes nothing, so
     * this runs whenever a notification might be needed.
     */
    static void createChannels(Context context) {
        NotificationManager manager = context.getSystemService(NotificationManager.class);
        if (manager == null) return;

        NotificationChannel call = new NotificationChannel(CHANNEL_CALL, "Ongoing calls", NotificationManager.IMPORTANCE_LOW);
        call.setDescription("Shown while a call is running, with Mute and Hang up");
        call.setShowBadge(false);

        NotificationChannel incoming = new NotificationChannel(CHANNEL_INCOMING, "Incoming calls", NotificationManager.IMPORTANCE_HIGH);
        incoming.setDescription("Rings when someone calls");
        incoming.setSound(RingtoneManager.getDefaultUri(RingtoneManager.TYPE_RINGTONE),
                new AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                        .build());
        incoming.enableVibration(true);
        incoming.setLockscreenVisibility(Notification.VISIBILITY_PUBLIC);

        NotificationChannel background = new NotificationChannel(CHANNEL_BACKGROUND, "Listening and transfers", NotificationManager.IMPORTANCE_MIN);
        background.setDescription("Shown while the app listens for calls or moves files in the background");
        background.setShowBadge(false);

        NotificationChannel messages = new NotificationChannel(CHANNEL_MESSAGES, "Messages and missed calls", NotificationManager.IMPORTANCE_DEFAULT);

        try {
            manager.createNotificationChannel(call);
            manager.createNotificationChannel(incoming);
            manager.createNotificationChannel(background);
            manager.createNotificationChannel(messages);
        } catch (RuntimeException e) {
            Log.w(TAG, "could not create notification channels", e);
        }
    }

    /**
     * Keeps the CPU running and Wi-Fi out of its power-saving mode for the
     * length of a call. The audio stream alone keeps the CPU up, but not
     * the radio: in power save, Wi-Fi batches incoming packets every few
     * hundred milliseconds, which a call hears as jitter.
     */
    private void acquireLocks() {
        if (wakeLock == null) {
            PowerManager power = getSystemService(PowerManager.class);
            if (power != null) {
                wakeLock = power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "antiphon:call");
                wakeLock.setReferenceCounted(false);
                wakeLock.acquire();
            }
        }
        if (wifiLock == null) {
            WifiManager wifi = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            if (wifi != null) {
                @SuppressWarnings("deprecation")
                int mode = Build.VERSION.SDK_INT >= 29
                        ? WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                        : WifiManager.WIFI_MODE_FULL_HIGH_PERF;
                try {
                    wifiLock = wifi.createWifiLock(mode, "antiphon:call");
                    wifiLock.setReferenceCounted(false);
                    wifiLock.acquire();
                } catch (RuntimeException e) {
                    Log.w(TAG, "could not hold Wi-Fi awake", e);
                    wifiLock = null;
                }
            }
        }
    }

    /** Lets go of whichever of the two locks is held. Safe to call twice. */
    private void releaseLocks() {
        if (wakeLock != null) {
            wakeLock.release();
            wakeLock = null;
        }
        if (wifiLock != null) {
            wifiLock.release();
            wifiLock = null;
        }
    }

    /** A notification button titled {@code title} that fires {@code intent}. */
    private static Notification.Action action(Context context, String title, PendingIntent intent) {
        return new Notification.Action.Builder(
                Icon.createWithResource(context, R.drawable.ic_stat_antiphon), title, intent).build();
    }

    /**
     * A PendingIntent that delivers {@code action} to this service. The
     * action's hash is the request code, so each button gets its own
     * PendingIntent rather than all sharing the last one made.
     */
    private static PendingIntent service(Context context, String action) {
        Intent intent = new Intent(context, CallService.class).setAction(action);
        return PendingIntent.getService(context, action.hashCode(), intent,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    /**
     * A PendingIntent that opens AntiphonActivity with {@code action}, for
     * the incoming-call alert. Request codes as in {@link #service}.
     */
    private static PendingIntent activity(Context context, String action) {
        Intent intent = new Intent(context, AntiphonActivity.class)
                .setAction(action)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return PendingIntent.getActivity(context, action.hashCode(), intent,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }

    /**
     * A PendingIntent that brings the app to the front as the launcher icon
     * would, without acting on anything.
     */
    static PendingIntent openApp(Context context) {
        Intent intent = new Intent(context, AntiphonActivity.class)
                .setAction(Intent.ACTION_MAIN)
                .addCategory(Intent.CATEGORY_LAUNCHER)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return PendingIntent.getActivity(context, 0, intent,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
    }
}
