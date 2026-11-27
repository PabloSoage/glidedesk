package io.github.pablosoage.glidedesk.server

import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.projection.MediaProjection
import android.view.Surface
import io.github.pablosoage.glidedesk.shell.ShellClient

/**
 * Where the served picture comes from and where the client's input goes. Positions are in
 * pixels of the display input goes to (the phone's screen, or a display of its own).
 */
interface ScreenSource {
    /** A display of its own at the video size: positions need no scaling. */
    val ownDisplay: Boolean

    /** Draws the screen into [surface] at [width] × [height] (again after a restart). */
    fun start(surface: Surface, width: Int, height: Int, dpi: Int)

    /** Stops drawing into the surface (the encoder is about to go). */
    fun stop()

    /** The server is closing. */
    fun release()

    fun pointer(action: Int, x: Float, y: Float)
    fun scroll(x: Float, y: Float, h: Float, v: Float)
    fun key(down: Boolean, keycode: Int, meta: Int)
    fun text(codepoint: Int)
}

/** The privileged helper (Shizuku or ADB): any display, real input. */
class ShellSource(private val shell: ShellClient, override val ownDisplay: Boolean) : ScreenSource {
    override fun start(surface: Surface, width: Int, height: Int, dpi: Int) {
        shell.start(surface, width, height, dpi, ownDisplay)
    }

    override fun stop() = shell.stop(keepDisplay = true)
    override fun release() = shell.stop(keepDisplay = false)
    override fun pointer(action: Int, x: Float, y: Float) = shell.pointer(action, x, y)
    override fun scroll(x: Float, y: Float, h: Float, v: Float) = shell.scroll(x, y, h, v)
    override fun key(down: Boolean, keycode: Int, meta: Int) = shell.key(down, keycode, meta)
    override fun text(codepoint: Int) = shell.text(codepoint)
}

/**
 * No privileges: MediaProjection for the picture (the user agrees each time), the
 * [InputAccessibilityService] for the input (gestures and global actions; text into the
 * focused field).
 */
class ProjectionSource(private val projection: MediaProjection) : ScreenSource {
    override val ownDisplay = false
    private var display: VirtualDisplay? = null

    override fun start(surface: Surface, width: Int, height: Int, dpi: Int) {
        // One virtual display per projection (Android 14 refuses a second): later encoders
        // get it resized onto their surface.
        val d = display
        if (d == null) {
            display = projection.createVirtualDisplay(
                "glidedesk", width, height, dpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                surface, null, null,
            )
        } else {
            d.resize(width, height, dpi)
            d.surface = surface
        }
    }

    override fun stop() {
        display?.surface = null
    }

    override fun release() {
        display?.release()
        display = null
        projection.stop()
    }

    private val input get() = InputAccessibilityService.instance

    override fun pointer(action: Int, x: Float, y: Float) {
        input?.pointer(action, x, y)
    }

    override fun scroll(x: Float, y: Float, h: Float, v: Float) {
        input?.scroll(x, y, h, v)
    }

    override fun key(down: Boolean, keycode: Int, meta: Int) {
        input?.key(down, keycode, meta)
    }

    override fun text(codepoint: Int) {
        input?.text(codepoint)
    }
}
