package io.github.pablosoage.glidedesk

import android.view.Surface

/** One remote desktop session, backed by the Rust core (crates/android). */
class NativeSession private constructor(private var handle: Long) {

    enum class State { CONNECTING, CONNECTED, ENDED, FAILED }

    val state: State
        get() = when (nativeState(handle)) {
            0 -> State.CONNECTING
            1 -> State.CONNECTED
            3 -> State.FAILED
            else -> State.ENDED
        }

    val error: String get() = nativeError(handle) ?: ""

    /** A short line about a poor link (low-latency mode), or null when it is good. */
    val warning: String? get() = if (handle == 0L) null else nativeWarning(handle)

    /** Text copied in the session since the last call (low-latency mode), or null. */
    fun takeClipboard(): String? = if (handle == 0L) null else nativeTakeClipboard(handle)

    /** Files copied in the session and fetched into the app's cache since the last call, or null. */
    fun takeFiles(): Array<String>? = if (handle == 0L) null else nativeTakeFiles(handle)

    /** The apps the server can open instead of the desktop (empty until logged in). */
    val apps: List<String> get() = if (handle == 0L) emptyList() else nativeApps(handle)?.toList() ?: emptyList()

    /** Text copied on the phone. */
    fun clipboard(text: String) { if (handle != 0L) nativeClipboard(handle, text) }

    /** Files copied on the phone (paths of copies in the app's cache). */
    fun files(paths: Array<String>) { if (handle != 0L) nativeFiles(handle, paths) }

    fun setSurface(surface: Surface?) = nativeSetSurface(handle, surface)
    fun mouseMove(x: Int, y: Int) = nativeMouseMove(handle, x, y)
    /** 0 left, 1 middle, 2 right. */
    fun mouseButton(button: Int, pressed: Boolean) = nativeMouseButton(handle, button, pressed)
    /** RDP wheel units: 120 per notch, positive away from the user. */
    fun wheel(vertical: Boolean, units: Int) = nativeWheel(handle, vertical, units)
    fun key(scancode: Int, extended: Boolean, pressed: Boolean) = nativeKey(handle, scancode, extended, pressed)
    fun unicode(codepoint: Int, pressed: Boolean) = nativeUnicode(handle, codepoint, pressed)
    fun resize(width: Int, height: Int) = nativeResize(handle, width, height)

    fun close() {
        if (handle != 0L) {
            nativeClose(handle)
            handle = 0
        }
    }

    companion object {
        init {
            System.loadLibrary("glidedesk_android")
        }

        /**
         * `lowLatency`: the native protocol (UDP video, HEVC) instead of RDP. `app`: open
         * this app of the server instead of the desktop (low-latency mode; empty for none).
         */
        fun connect(
            host: String, port: Int, user: String, password: String, width: Int, height: Int,
            lowLatency: Boolean, refresh: Int, app: String = "",
        ): NativeSession? {
            val h = nativeConnect(host, port, user, password, width, height, lowLatency, refresh, app)
            return if (h == 0L) null else NativeSession(h)
        }

        @JvmStatic private external fun nativeConnect(
            host: String, port: Int, user: String, password: String, width: Int, height: Int, native: Boolean,
            refresh: Int, app: String,
        ): Long
        @JvmStatic private external fun nativeSetSurface(handle: Long, surface: Surface?)
        @JvmStatic private external fun nativeMouseMove(handle: Long, x: Int, y: Int)
        @JvmStatic private external fun nativeMouseButton(handle: Long, button: Int, pressed: Boolean)
        @JvmStatic private external fun nativeWheel(handle: Long, vertical: Boolean, units: Int)
        @JvmStatic private external fun nativeKey(handle: Long, scancode: Int, extended: Boolean, pressed: Boolean)
        @JvmStatic private external fun nativeUnicode(handle: Long, codepoint: Int, pressed: Boolean)
        @JvmStatic private external fun nativeResize(handle: Long, width: Int, height: Int)
        @JvmStatic private external fun nativeState(handle: Long): Int
        @JvmStatic private external fun nativeError(handle: Long): String?
        @JvmStatic private external fun nativeWarning(handle: Long): String?
        @JvmStatic private external fun nativeTakeClipboard(handle: Long): String?
        @JvmStatic private external fun nativeTakeFiles(handle: Long): Array<String>?
        @JvmStatic private external fun nativeApps(handle: Long): Array<String>?
        @JvmStatic private external fun nativeClipboard(handle: Long, text: String)
        @JvmStatic private external fun nativeFiles(handle: Long, paths: Array<String>)
        @JvmStatic private external fun nativeClose(handle: Long)
    }
}
