// The encoder format and the output loop are adapted from scrcpy
// (https://github.com/Genymobile/scrcpy), commit c939183, file
// server/src/main/java/com/genymobile/scrcpy/video/SurfaceEncoder.java (createFormat, encode).
// Copyright (C) 2018 Genymobile, Copyright (C) 2018-2026 Romain Vimont.
// Licensed under the Apache License, Version 2.0; changes: Kotlin, output to the Rust core.
package io.github.pablosoage.glidedesk.server

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Bundle
import android.util.Log
import android.view.Surface

/**
 * MediaCodec encoding from an input [Surface] (the screen is drawn into it, zero copy),
 * every output buffer straight to the [NativeServer] from a thread of its own.
 */
class VideoEncoder(private val server: NativeServer) {
    private var codec: MediaCodec? = null
    private var surface: Surface? = null
    private var drain: Thread? = null
    @Volatile private var running = false

    /**
     * A new encoder of [mime] at [width] × [height]; returns the surface to draw into.
     * Throws when the encoder refuses the format (the caller then tries a smaller size).
     */
    fun start(mime: String, width: Int, height: Int, bitrate: Int, fps: Int): Surface {
        stop()
        val c = MediaCodec.createEncoderByType(mime)
        val s = try {
            c.configure(format(mime, width, height, bitrate, fps), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            c.createInputSurface().also { c.start() }
        } catch (e: Exception) {
            c.release()
            throw e
        }
        codec = c
        surface = s
        running = true
        drain = Thread({ output(c, width, height) }, "glidedesk-encoder").also { it.start() }
        Log.i(TAG, "Encoder ${c.name}: $mime ${width}x$height, ${bitrate / 1000} kb/s")
        return s
    }

    private fun output(c: MediaCodec, width: Int, height: Int) {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val i = try {
                c.dequeueOutputBuffer(info, 100_000)
            } catch (e: IllegalStateException) {
                // A codec error while running (not a stop): the core restarts it smaller.
                if (running) {
                    Log.w(TAG, "Encoder failed", e)
                    server.encoderFailed()
                }
                break
            }
            if (i < 0) continue
            try {
                val buffer = c.getOutputBuffer(i)
                if (buffer != null && info.size > 0) {
                    server.frame(buffer, info.offset, info.size, info.flags, width, height)
                }
            } finally {
                c.releaseOutputBuffer(i, false)
            }
            if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) break
        }
    }

    fun requestKeyframe() = parameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })

    fun setBitrate(bps: Int) = parameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bps) })

    private fun parameters(b: Bundle) {
        try {
            codec?.setParameters(b)
        } catch (e: IllegalStateException) {
            // Between two encoders.
        }
    }

    /** Stops and frees the encoder (the screen must not draw into its surface any more). */
    fun stop() {
        running = false
        drain?.join()
        drain = null
        codec?.let {
            try {
                it.stop()
            } catch (e: IllegalStateException) {
                // Not started, or already failed.
            }
            it.release()
        }
        codec = null
        surface?.release()
        surface = null
    }

    companion object {
        private const val TAG = "glidedesk-server"

        /** Keyframes come when the client asks; this is only a fallback. */
        private const val KEYFRAME_INTERVAL_S = 10
        /** A still screen sends its last frame again this often, so a lost one is redone. */
        private const val REPEAT_FRAME_US = 100_000L

        private fun format(mime: String, width: Int, height: Int, bitrate: Int, fps: Int) =
            MediaFormat.createVideoFormat(mime, width, height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
                // Required, but the rate follows the screen; the cap below is what counts.
                setInteger(MediaFormat.KEY_FRAME_RATE, 60)
                setFloat("max-fps-to-encoder", fps.toFloat())
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, KEYFRAME_INTERVAL_S)
                setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, REPEAT_FRAME_US)
                setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_LIMITED)
                // Real-time priority, and one frame out as soon as one is in.
                setInteger(MediaFormat.KEY_PRIORITY, 0)
                setInteger(MediaFormat.KEY_LATENCY, 1)
            }

        /** The phone has a hardware HEVC encoder (software ones are too slow for this). */
        fun hasHevc(): Boolean = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.any { info ->
            info.isEncoder && info.isHardwareAccelerated &&
                info.supportedTypes.any { it.equals(MediaFormat.MIMETYPE_VIDEO_HEVC, ignoreCase = true) }
        }
    }
}
