package io.github.pablosoage.glidedesk.server

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.hardware.display.DisplayManager
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.DisplayMetrics
import android.util.Log
import android.view.Display
import androidx.core.content.IntentCompat
import io.github.pablosoage.glidedesk.R
import io.github.pablosoage.glidedesk.shell.ShellClient
import io.github.pablosoage.glidedesk.shell.ShellConnection
import java.io.File

/**
 * The phone server while it runs: a foreground service (type mediaProjection for screen
 * capture, specialUse with the privileged helper) that owns the [NativeServer], the
 * [VideoEncoder] and the [ScreenSource], and carries out the core's events on a thread of
 * its own.
 */
class ScreenServerService : Service() {
    private val main = Handler(Looper.getMainLooper())
    private var worker: Thread? = null
    @Volatile private var running = false
    /** Guarded by `this`: the main thread reads it while the worker may close it. */
    private var server: NativeServer? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var displayListener: DisplayManager.DisplayListener? = null

    // The phone's screen (rotated as shown) and the video, to scale positions.
    @Volatile private var screenW = 1
    @Volatile private var screenH = 1
    @Volatile private var screenDpi = 160
    private var videoW = 1
    private var videoH = 1

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        val capture = intent?.getIntExtra(EXTRA_RESULT_CODE, 0) == Activity.RESULT_OK
        foreground(capture)
        if (running) return START_NOT_STICKY
        running = true
        lastError = ""
        // After startForeground: Android 10 on hands out projections only to a running
        // mediaProjection service.
        val projection = if (capture && intent != null) {
            try {
                projection(intent)
            } catch (e: Exception) {
                fail("Screen capture was not allowed: ${e.message}")
                return START_NOT_STICKY
            }
        } else {
            null
        }
        worker = Thread({ work(projection) }, "glidedesk-server").also { it.start() }
        return START_NOT_STICKY
    }

    private fun projection(intent: Intent): MediaProjection {
        val data = IntentCompat.getParcelableExtra(intent, EXTRA_RESULT_DATA, Intent::class.java)
            ?: throw IllegalStateException("no consent")
        val mpm = getSystemService(MediaProjectionManager::class.java)
        val p = mpm.getMediaProjection(Activity.RESULT_OK, data) ?: throw IllegalStateException("no projection")
        // Required before capturing (Android 14): the user or the system can end it.
        p.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                main.post { stopSelf() }
            }
        }, main)
        return p
    }

    private fun work(projection: MediaProjection?) {
        val settings = ServerSettings(this)
        var source: ScreenSource? = null
        var encoder: VideoEncoder? = null
        try {
            source = if (projection != null) {
                ProjectionSource(projection)
            } else {
                if (settings.way == ServerSettings.Way.SHIZUKU) ShellConnection.bindShizuku()
                val shell = waitForShell() ?: throw IllegalStateException(
                    if (settings.way == ServerSettings.Way.SHIZUKU) {
                        "Shizuku did not start the helper"
                    } else {
                        "The helper is not running: run the adb command first"
                    },
                )
                ShellSource(shell, settings.ownDisplay)
            }
            readScreen()
            val srv = NativeServer.start(
                settings.port, settings.user, settings.password, File(filesDir, "server").path,
                VideoEncoder.hasHevc(), source.ownDisplay, settings.maxSize,
                settings.maxBitrateMbps.coerceIn(1, 200) * 1_000_000, screenW, screenH,
            )
            synchronized(this) { server = srv }
            encoder = VideoEncoder(srv)
            main.post { watchDisplay(); lockWifi() }
            val ev = IntArray(7)
            while (running) {
                if (srv.nextEvent(ev, 250)) handle(srv, source, encoder, ev)
            }
        } catch (e: Exception) {
            Log.e(TAG, "Server failed", e)
            fail(e.message ?: e.toString())
        } finally {
            runCatching { source?.stop() }
            encoder?.stop()
            runCatching { source?.release() }
            synchronized(this) {
                server?.close()
                server = null
            }
        }
    }

    private fun waitForShell(): ShellClient? {
        repeat(50) {
            ShellConnection.client?.let { return it }
            Thread.sleep(100)
        }
        return null
    }

    private fun handle(srv: NativeServer, source: ScreenSource, encoder: VideoEncoder, ev: IntArray) {
        when (ev[0]) {
            NativeServer.START -> restart(srv, source, encoder, ev[1] == 1, ev[2], ev[3], ev[4], ev[5])
            NativeServer.STOP -> {
                runCatching { source.stop() }
                encoder.stop()
            }
            NativeServer.KEYFRAME -> encoder.requestKeyframe()
            NativeServer.BITRATE -> encoder.setBitrate(ev[1])
            NativeServer.POINTER -> source.pointer(ev[1], x(source, ev[2]), y(source, ev[3]))
            NativeServer.SCROLL -> source.scroll(x(source, ev[1]), y(source, ev[2]), ev[3] / 120f, ev[4] / 120f)
            NativeServer.KEY -> source.key(ev[2] != 0, ev[1], ev[3])
            NativeServer.TEXT -> source.text(ev[1])
        }
    }

    /** Video pixels to pixels of the display input goes to (the middle of the pixel). */
    private fun x(source: ScreenSource, x: Int) =
        if (source.ownDisplay) x.toFloat() else (x + 0.5f) * screenW / videoW

    private fun y(source: ScreenSource, y: Int) =
        if (source.ownDisplay) y.toFloat() else (y + 0.5f) * screenH / videoH

    private fun restart(
        srv: NativeServer, source: ScreenSource, encoder: VideoEncoder,
        hevc: Boolean, width: Int, height: Int, bitrate: Int, fps: Int,
    ) {
        runCatching { source.stop() }
        encoder.stop()
        val mime = if (hevc) MediaFormat.MIMETYPE_VIDEO_HEVC else MediaFormat.MIMETYPE_VIDEO_AVC
        val surface = try {
            encoder.start(mime, width, height, bitrate, fps)
        } catch (e: Exception) {
            Log.w(TAG, "The encoder refused $mime ${width}x$height", e)
            srv.encoderFailed()
            return
        }
        try {
            source.start(surface, width, height, dpi(width, height, source.ownDisplay))
            videoW = width
            videoH = height
            lastError = ""
        } catch (e: Exception) {
            Log.e(TAG, "Cannot capture the screen", e)
            lastError = "Cannot capture the screen: ${e.message}"
            encoder.stop()
        }
    }

    /** A display of its own gets the phone's density scaled to its size (as scrcpy does). */
    private fun dpi(width: Int, height: Int, ownDisplay: Boolean): Int {
        if (!ownDisplay) return screenDpi
        val phone = minOf(screenW, screenH).coerceAtLeast(1)
        return (screenDpi.toLong() * minOf(width, height) / phone).toInt().coerceIn(120, 640)
    }

    @Suppress("DEPRECATION")
    private fun readScreen(): Boolean {
        val d = getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY) ?: return false
        val m = DisplayMetrics()
        d.getRealMetrics(m)
        val changed = m.widthPixels != screenW || m.heightPixels != screenH
        screenW = m.widthPixels.coerceAtLeast(1)
        screenH = m.heightPixels.coerceAtLeast(1)
        screenDpi = m.densityDpi
        return changed
    }

    /** Rotation changes the screen's size: the core restarts the video when it must. */
    private fun watchDisplay() {
        if (!running) return
        val listener = object : DisplayManager.DisplayListener {
            override fun onDisplayChanged(displayId: Int) {
                if (displayId == Display.DEFAULT_DISPLAY && readScreen()) {
                    synchronized(this@ScreenServerService) { server?.display(screenW, screenH) }
                }
            }

            override fun onDisplayAdded(displayId: Int) {}
            override fun onDisplayRemoved(displayId: Int) {}
        }
        getSystemService(DisplayManager::class.java).registerDisplayListener(listener, main)
        displayListener = listener
    }

    /** Wi-Fi power saving holds packets back for tens of milliseconds. */
    private fun lockWifi() {
        if (!running) return
        val wifi = applicationContext.getSystemService(WifiManager::class.java) ?: return
        wifiLock = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "glidedesk:server").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun fail(message: String) {
        lastError = message
        main.post { stopSelf() }
    }

    private fun foreground(capture: Boolean) {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.server_notification_channel), NotificationManager.IMPORTANCE_LOW),
        )
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, ServerActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this, 1, Intent(this, ScreenServerService::class.java).setAction(ACTION_STOP), PendingIntent.FLAG_IMMUTABLE,
        )
        val icon = Icon.createWithResource(this, R.drawable.ic_launcher_foreground)
        val notification = Notification.Builder(this, CHANNEL)
            .setSmallIcon(icon)
            .setContentTitle(getString(R.string.server_notification_title))
            .setContentText(getString(R.string.server_notification_text))
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(icon, getString(R.string.server_stop), stop).build())
            .build()
        val type = when {
            capture -> ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
            Build.VERSION.SDK_INT >= 34 -> ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
            // Before Android 14 the types are only checked against the manifest.
            else -> 0
        }
        if (type != 0) startForeground(NOTIFICATION_ID, notification, type) else startForeground(NOTIFICATION_ID, notification)
    }

    override fun onDestroy() {
        running = false
        worker?.join(3000)
        worker = null
        displayListener?.let { getSystemService(DisplayManager::class.java).unregisterDisplayListener(it) }
        displayListener = null
        wifiLock?.release()
        wifiLock = null
        if (instance === this) instance = null
        super.onDestroy()
    }

    /** The core's status line, or null before it started. */
    fun status(): String? = synchronized(this) { server?.status }

    fun fingerprint(): String? = synchronized(this) { server?.fingerprint }

    companion object {
        private const val TAG = "glidedesk-server"
        private const val CHANNEL = "server"
        private const val NOTIFICATION_ID = 1
        const val ACTION_STOP = "io.github.pablosoage.glidedesk.server.STOP"
        /** The screen capture consent (MediaProjection) as the activity got it. */
        const val EXTRA_RESULT_CODE = "resultCode"
        const val EXTRA_RESULT_DATA = "resultData"

        @Volatile
        var instance: ScreenServerService? = null
            private set

        /** Why the server stopped or failed last (empty when it did not). */
        @Volatile
        var lastError: String = ""
            private set

        /** Starts the server with the helper, or with [consent] (the projection's result data) to capture the screen. */
        fun start(context: Context, consent: Intent? = null) {
            val intent = Intent(context, ScreenServerService::class.java)
            if (consent != null) {
                intent.putExtra(EXTRA_RESULT_CODE, Activity.RESULT_OK).putExtra(EXTRA_RESULT_DATA, consent)
            }
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.startService(Intent(context, ScreenServerService::class.java).setAction(ACTION_STOP))
        }
    }
}
