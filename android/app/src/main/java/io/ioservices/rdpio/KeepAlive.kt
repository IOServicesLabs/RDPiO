package io.ioservices.rdpio

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder

/**
 * Keeps the app's process (and so the open sessions) alive while it's in the
 * background: switch away and back without reconnecting. Swiping the app
 * away in Recents closes every session.
 */
class KeepAlive : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "Open sessions", NotificationManager.IMPORTANCE_LOW))
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, StreamActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val kinds = Native.sessions().split(',').filter { it.isNotEmpty() }
            .joinToString(" and ") { if (it == "w365") "Windows 365" else "Hyper" }
        val n = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.mipmap.ic_launcher)
            .setContentTitle("Connected")
            .setContentText(if (kinds.isEmpty()) "rdpio" else "$kinds — tap to return; swipe the app away to disconnect")
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        return START_NOT_STICKY
    }

    /** The app was swiped away: it's closed, so are its sessions. */
    override fun onTaskRemoved(rootIntent: Intent?) {
        closeAll()
        stopSelf()
        super.onTaskRemoved(rootIntent)
    }

    companion object {
        private const val CHANNEL = "sessions"

        fun start(ctx: Context) {
            try {
                ctx.startForegroundService(Intent(ctx, KeepAlive::class.java))
            } catch (e: Exception) {
                // (Not allowed from the background: the stream is in front when this runs.)
            }
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, KeepAlive::class.java))
        }

        fun closeAll() {
            // Each disconnect ends the current session; the next becomes current.
            repeat(2) { if (Native.sessions().isNotEmpty()) Native.disconnect() }
        }
    }
}
