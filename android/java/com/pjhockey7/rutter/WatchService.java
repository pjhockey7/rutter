package com.pjhockey7.rutter;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.os.Build;
import android.os.IBinder;

/**
 * Keeps rutter's process alive while an alarm is armed.
 *
 * <p>This is the only Java in the project, and it exists because Android
 * gives no alternative. {@code android.app.Service} is abstract — there is
 * no {@code NativeService} the way there is a {@code NativeActivity} — so a
 * foreground service cannot be had without bytecode. Adding it ends the
 * zero-dex property the rest of the app keeps, deliberately and with the
 * cost understood.
 *
 * <p>It does <em>not</em> reintroduce the problem that killed the OpenCPN
 * attempt. That was 58 native calls into a Java tree that was never
 * published, 37 of whose methods existed nowhere. This is eighty lines
 * sitting in the repository next to everything else.
 *
 * <p><b>What it does and does not do.</b> It runs no navigation. The anchor
 * watch, the depth watch, the NMEA reader and the siren are all Rust
 * threads in this same process, and they keep running for exactly as long
 * as the process does. The service's whole job is to make that "as long as
 * the process does" mean something when the screen is off: a backgrounded
 * Android process is killable at any moment, and a foreground service with
 * an ongoing notification is the only sanctioned way to say "not this one".
 *
 * <p>The notification is not decoration either — the system requires one,
 * and it is the honest signal that something is still watching and still
 * using the GPS.
 */
public final class WatchService extends Service {

    /** Channel id. Visible to the user in the system's notification settings. */
    private static final String CHANNEL = "rutter.watch";

    /** Any non-zero id; the service is a singleton so one is enough. */
    private static final int NOTIFICATION = 1;

    /** What the notification says, passed in by the native side. */
    public static final String EXTRA_TEXT = "text";

    @Override
    public IBinder onBind(Intent intent) {
        // Nothing binds to this. It is started and stopped, and the Rust
        // side talks to itself in the same process rather than through
        // any interface here.
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String text = null;
        if (intent != null) {
            text = intent.getStringExtra(EXTRA_TEXT);
        }
        if (text == null) {
            text = "Anchor watch running";
        }

        createChannel();

        try {
            startForeground(NOTIFICATION, buildNotification(text));
        } catch (RuntimeException e) {
            // This can genuinely fail, and it must not take the app with
            // it. A service declared with foregroundServiceType="location"
            // throws SecurityException on API 34 and later if the location
            // permission is not granted at this moment, and API 31 and
            // later throw if the start was not allowed from the
            // background. Either way the watch itself is unaffected — it
            // is a Rust thread in this process — so the right response is
            // to stand down quietly and let the panel say that nothing is
            // holding the process open.
            //
            // Not catching it would be worse than not having the service:
            // an uncaught exception here crashes the app at the exact
            // moment someone sets an anchor watch.
            android.util.Log.e("rutter", "startForeground refused", e);
            stopSelf();
            return START_NOT_STICKY;
        }

        // START_NOT_STICKY, which looks like the wrong answer and is not.
        //
        // START_STICKY restarts the *service* after a kill — it does not
        // restart the NativeActivity, so `android_main` never runs again
        // and there is no NMEA reader, no watch and no siren. The result
        // would be an ongoing notification saying "Anchor watch standing"
        // over a process watching nothing at all, which is the precise
        // silent failure this whole feature exists to prevent. A missing
        // notification is a worse night's sleep; a lying one is a boat on
        // the rocks.
        return START_NOT_STICKY;
    }

    private void createChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return;
        }
        NotificationManager manager = getSystemService(NotificationManager.class);
        if (manager == null || manager.getNotificationChannel(CHANNEL) != null) {
            return;
        }
        // Low importance: this notification is a statement that the watch
        // is running, not the alarm itself. The alarm is a noise made by
        // the app. A high-importance channel here would buzz every time
        // the text changed.
        NotificationChannel channel = new NotificationChannel(
                CHANNEL, "Anchor and depth watch", NotificationManager.IMPORTANCE_LOW);
        channel.setDescription("Shown while rutter is watching for a dragging anchor or shoaling water.");
        channel.setShowBadge(false);
        manager.createNotificationChannel(channel);
    }

    private Notification buildNotification(String text) {
        // Tapping it returns to the chart, which is what someone woken by
        // an alarm wants.
        Intent open = new Intent(this, android.app.NativeActivity.class);
        open.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TOP);
        int pendingFlags = PendingIntent.FLAG_UPDATE_CURRENT;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            pendingFlags |= PendingIntent.FLAG_IMMUTABLE;
        }
        PendingIntent tap = PendingIntent.getActivity(this, 0, open, pendingFlags);

        Notification.Builder builder;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            builder = new Notification.Builder(this, CHANNEL);
        } else {
            builder = new Notification.Builder(this);
        }

        return builder
                .setContentTitle("rutter")
                .setContentText(text)
                // A framework icon rather than one of ours: the app ships
                // no drawables, and a notification with no icon is
                // refused outright on some versions.
                .setSmallIcon(android.R.drawable.ic_menu_compass)
                .setContentIntent(tap)
                // Not dismissable: it is the thing keeping the process
                // alive, and letting it be swiped away would silently
                // stop the watch.
                .setOngoing(true)
                .build();
    }

    // There are deliberately no static begin/end helpers here.
    //
    // They existed once and were called over JNI, which failed on
    // hardware: `FindClass` from a natively-attached thread resolves
    // against the system class loader and cannot see this APK's classes
    // at all. The native side now builds the Intent itself from a
    // `ComponentName`, so nothing has to resolve this class by name —
    // the system finds it from the manifest, which is the supported
    // route. See `platform/android/src/watch.rs`.
}
