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

        /** `lowLatency`: the native protocol (UDP video, HEVC) instead of RDP. */
        fun connect(
            host: String, port: Int, user: String, password: String, width: Int, height: Int,
            lowLatency: Boolean,
        ): NativeSession? {
            val h = nativeConnect(host, port, user, password, width, height, lowLatency)
            return if (h == 0L) null else NativeSession(h)
        }

        @JvmStatic private external fun nativeConnect(
            host: String, port: Int, user: String, password: String, width: Int, height: Int, native: Boolean,
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
        @JvmStatic private external fun nativeClose(handle: Long)
    }
}
