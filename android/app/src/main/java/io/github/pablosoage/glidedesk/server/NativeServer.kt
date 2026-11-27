package io.github.pablosoage.glidedesk.server

import java.nio.ByteBuffer

/**
 * The phone server's Rust core (crates/android/src/server): the native protocol towards the
 * client. It takes the encoder's output ([frame]) and tells what to do ([nextEvent]).
 *
 * Not thread-safe against [close]: every thread using it must have stopped first.
 */
class NativeServer private constructor(private var handle: Long) {

    /** One MediaCodec output buffer (`flags`: its `BUFFER_FLAG_*`) of a [width] × [height] video. */
    fun frame(buffer: ByteBuffer, offset: Int, size: Int, flags: Int, width: Int, height: Int) =
        nativeFrame(handle, buffer, offset, size, flags, width, height)

    /** The phone's screen changed size (rotation). */
    fun display(width: Int, height: Int) = nativeDisplay(handle, width, height)

    /** The encoder refused the last size: the core tries a smaller one. */
    fun encoderFailed() = nativeEncoderFailed(handle)

    /** Waits up to [timeoutMs] for the next event, written to [out] (seven ints: the kind first). */
    fun nextEvent(out: IntArray, timeoutMs: Int): Boolean = nativeNextEvent(handle, out, timeoutMs)

    val status: String get() = nativeStatus(handle) ?: ""

    /** SHA-256 of the certificate, the fingerprint clients ask to trust. */
    val fingerprint: String get() = nativeFingerprint(handle) ?: ""

    fun close() {
        if (handle != 0L) {
            nativeStop(handle)
            handle = 0
        }
    }

    companion object {
        init {
            System.loadLibrary("glidedesk_android")
        }

        // Event kinds (`Event::to_ints` in the core).
        /** codec (1: HEVC), width, height, bitrate, fps: (re)start the capture and the encoder. */
        const val START = 1
        const val STOP = 2
        const val KEYFRAME = 3
        /** bits per second */
        const val BITRATE = 4
        /** action (ShellProtocol.POINTER_*), x, y in video pixels */
        const val POINTER = 5
        /** x, y, horizontal, vertical (120 per notch, positive right and up) */
        const val SCROLL = 6
        /** Android key code, down (1), meta state */
        const val KEY = 7
        /** a code point */
        const val TEXT = 8

        /** Starts listening on TCP and UDP [port]; throws with the reason when it cannot. */
        fun start(
            port: Int, user: String, password: String, stateDir: String, hevc: Boolean,
            ownDisplay: Boolean, maxSize: Int, maxBitrate: Int, displayWidth: Int, displayHeight: Int,
        ): NativeServer {
            val h = nativeStart(
                port, user, password, stateDir, hevc, ownDisplay, maxSize, maxBitrate,
                displayWidth, displayHeight,
            )
            if (h == 0L) throw IllegalStateException(nativeStartError() ?: "the server did not start")
            return NativeServer(h)
        }

        @JvmStatic private external fun nativeStart(
            port: Int, user: String, password: String, stateDir: String, hevc: Boolean,
            ownDisplay: Boolean, maxSize: Int, maxBitrate: Int, displayWidth: Int, displayHeight: Int,
        ): Long
        @JvmStatic private external fun nativeStartError(): String?
        @JvmStatic private external fun nativeFrame(
            handle: Long, buffer: ByteBuffer, offset: Int, size: Int, flags: Int, width: Int, height: Int,
        )
        @JvmStatic private external fun nativeDisplay(handle: Long, width: Int, height: Int)
        @JvmStatic private external fun nativeEncoderFailed(handle: Long)
        @JvmStatic private external fun nativeNextEvent(handle: Long, out: IntArray, timeoutMs: Int): Boolean
        @JvmStatic private external fun nativeStatus(handle: Long): String?
        @JvmStatic private external fun nativeFingerprint(handle: Long): String?
        @JvmStatic private external fun nativeStop(handle: Long)
    }
}
