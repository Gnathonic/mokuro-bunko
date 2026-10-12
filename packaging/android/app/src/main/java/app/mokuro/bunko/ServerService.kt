package app.mokuro.bunko

import android.app.ForegroundServiceStartNotAllowedException
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import java.util.concurrent.CopyOnWriteArraySet
import java.util.concurrent.Executors

/**
 * Keeps the Rust server alive: a foreground service (type specialUse, see
 * docs/rust-port/MOBILE.md) with an ongoing notification, a partial wake lock and a
 * Wi-Fi lock while serving. The server itself runs on its own native thread; this
 * class only starts and stops it on a single background executor.
 */
class ServerService : Service() {

    sealed class State {
        object Stopped : State()
        object Starting : State()
        data class Running(val url: String, val lan: Boolean, val port: Int) : State()
        data class Failed(val message: String) : State()
    }

    private val worker = Executors.newSingleThreadExecutor { r -> Thread(r, "bunko-service") }
    private val main = Handler(Looper.getMainLooper())
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                Prefs(this).autoStart = false
                stopServer()
                return START_NOT_STICKY
            }
            ACTION_RESTART -> {
                if (!enterForeground()) return START_NOT_STICKY
                worker.execute {
                    BunkoNative.stop()
                    startNative()
                }
            }
            // ACTION_START, or a null intent when Android restarts a killed sticky service.
            else -> {
                if (!enterForeground()) return START_NOT_STICKY
                worker.execute { startNative() }
            }
        }
        return START_STICKY
    }

    /** startForeground with the Android 14+ type. False when Android refuses it. */
    private fun enterForeground(): Boolean {
        createChannel(this)
        val n = notification(getString(R.string.notif_starting))
        return try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
            } else {
                startForeground(NOTIFICATION_ID, n)
            }
            acquireLocks()
            true
        } catch (e: Exception) {
            // Android 12+ refuses foreground starts from the background (e.g. a sticky
            // restart without a battery-optimisation exemption).
            val refused = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S && e is ForegroundServiceStartNotAllowedException
            Log.w(TAG, "startForeground refused (background start: $refused)", e)
            setState(State.Failed(e.message ?: e.toString()))
            stopSelf()
            false
        }
    }

    private fun startNative() {
        val prefs = Prefs(this)
        if (BunkoNative.isRunning() && state is State.Running) {
            setState(state)
            return
        }
        setState(State.Starting)
        val storage = prefs.storageDir()
        val result = BunkoNative.start(storage.absolutePath, prefs.configFile().absolutePath, prefs.port, prefs.lan)
        if (result.startsWith("ok:")) {
            prefs.autoStart = true
            setState(State.Running(result.removePrefix("ok:"), prefs.lan, prefs.port))
        } else {
            setState(State.Failed(result.removePrefix("error:")))
            main.post {
                releaseLocks()
                stopForeground(STOP_FOREGROUND_DETACH)
            }
        }
    }

    private fun stopServer() {
        worker.execute {
            BunkoNative.stop()
            setState(State.Stopped)
            main.post {
                releaseLocks()
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
            }
        }
    }

    override fun onDestroy() {
        // Stopped by the system or by stopSelf: make sure the native server goes too.
        worker.execute {
            if (BunkoNative.stop()) setState(State.Stopped)
        }
        worker.shutdown()
        releaseLocks()
        super.onDestroy()
    }

    private fun setState(s: State) {
        state = s
        val text = when (s) {
            is State.Running -> if (s.lan) {
                val ip = Net.lanAddresses().firstOrNull()
                getString(R.string.notif_running_lan, if (ip != null) Net.url(ip, s.port) else s.url)
            } else {
                getString(R.string.notif_running_local, s.url)
            }
            is State.Failed -> getString(R.string.notif_failed, s.message)
            State.Starting -> getString(R.string.notif_starting)
            State.Stopped -> null
        }
        if (text != null) {
            getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(text))
        }
        main.post { listeners.forEach { it() } }
    }

    private fun notification(text: String): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val stop = PendingIntent.getService(
            this, 1, Intent(this, ServerService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
            .addAction(Notification.Action.Builder(null, getString(R.string.stop), stop).build())
            .build()
    }

    private fun acquireLocks() {
        if (wakeLock == null) {
            wakeLock = getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "MokuroBunko:server")
                .apply { setReferenceCounted(false); acquire() }
        }
        if (wifiLock == null) {
            val wm = applicationContext.getSystemService(WifiManager::class.java)
            @Suppress("DEPRECATION")
            val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                WifiManager.WIFI_MODE_FULL_LOW_LATENCY
            } else {
                WifiManager.WIFI_MODE_FULL_HIGH_PERF
            }
            wifiLock = wm?.createWifiLock(mode, "MokuroBunko:server")?.apply { setReferenceCounted(false); acquire() }
        }
    }

    private fun releaseLocks() {
        wakeLock?.takeIf { it.isHeld }?.release()
        wakeLock = null
        wifiLock?.takeIf { it.isHeld }?.release()
        wifiLock = null
    }

    companion object {
        private const val TAG = "MokuroBunko"
        const val CHANNEL_ID = "server"
        const val NOTIFICATION_ID = 1
        const val ACTION_START = "app.mokuro.bunko.START"
        const val ACTION_STOP = "app.mokuro.bunko.STOP"
        const val ACTION_RESTART = "app.mokuro.bunko.RESTART"

        @Volatile
        var state: State = State.Stopped
            private set

        /** Called on the main thread after every state change. */
        val listeners = CopyOnWriteArraySet<() -> Unit>()

        fun start(context: Context) {
            context.startForegroundService(Intent(context, ServerService::class.java).setAction(ACTION_START))
        }

        fun restart(context: Context) {
            context.startForegroundService(Intent(context, ServerService::class.java).setAction(ACTION_RESTART))
        }

        fun stop(context: Context) {
            context.startService(Intent(context, ServerService::class.java).setAction(ACTION_STOP))
        }

        fun createChannel(context: Context) {
            val nm = context.getSystemService(NotificationManager::class.java)
            if (nm.getNotificationChannel(CHANNEL_ID) == null) {
                nm.createNotificationChannel(
                    NotificationChannel(CHANNEL_ID, context.getString(R.string.channel_name), NotificationManager.IMPORTANCE_LOW)
                        .apply { description = context.getString(R.string.channel_description); setShowBadge(false) },
                )
            }
        }
    }
}
