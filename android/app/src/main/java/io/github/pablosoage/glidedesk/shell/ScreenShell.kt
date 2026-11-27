package io.github.pablosoage.glidedesk.shell

import android.content.Context
import android.hardware.display.VirtualDisplay
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import android.util.Log
import android.view.Surface
import kotlin.system.exitProcess

/**
 * The privileged helper: runs as the shell user, as a Shizuku user service or under
 * `adb shell app_process` ([ShellMain]), and does for the app what an app may not: show the
 * phone's screen (or a display of its own) on the app's encoder surface without asking, and
 * inject input as a real touch screen, mouse and keyboard.
 */
class ScreenShell() : Binder() {

    /** Shizuku (v13 on) builds user services with their context; the helper does not need it. */
    @Suppress("UNUSED_PARAMETER")
    constructor(context: Context) : this()

    init {
        attachInterface(null, ShellProtocol.DESCRIPTOR)
    }

    private val lock = Any()
    private var surface: Surface? = null
    private var mirror: VirtualDisplay? = null
    private var mirrorToken: IBinder? = null
    private var own: VirtualDisplay? = null
    /** The display input goes to (0: the phone's screen). */
    @Volatile private var target = 0
    private val input by lazy { ShellInput { target } }

    override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
        if (code == ShellProtocol.DESTROY) {
            release()
            exitProcess(0)
        }
        if (code < ShellProtocol.START || code > ShellProtocol.LAST) {
            return super.onTransact(code, data, reply, flags)
        }
        data.enforceInterface(ShellProtocol.DESCRIPTOR)
        when (code) {
            ShellProtocol.START -> {
                val s = Surface.CREATOR.createFromParcel(data)
                val (w, h, dpi) = Triple(data.readInt(), data.readInt(), data.readInt())
                val ownDisplay = data.readInt() != 0
                try {
                    reply?.writeInt(start(s, w, h, dpi, ownDisplay))
                } catch (e: Exception) {
                    Log.w(TAG, "Cannot show the screen", e)
                    s.release()
                    reply?.writeInt(-1)
                    reply?.writeString(e.cause?.message ?: e.message ?: e.toString())
                }
            }
            ShellProtocol.STOP -> stop(keepDisplay = data.readInt() != 0)
            ShellProtocol.POINTER -> input.pointer(data.readInt(), data.readFloat(), data.readFloat())
            ShellProtocol.SCROLL -> input.scroll(data.readFloat(), data.readFloat(), data.readFloat(), data.readFloat())
            ShellProtocol.KEY -> input.key(data.readInt() != 0, data.readInt(), data.readInt())
            ShellProtocol.TEXT -> input.text(data.readInt())
        }
        return true
    }

    private fun start(s: Surface, width: Int, height: Int, dpi: Int, ownDisplay: Boolean): Int = synchronized(lock) {
        stopMirror()
        surface?.release()
        surface = s
        val id = if (ownDisplay) {
            // Kept across encoder restarts (a resize, a new client): its apps stay open.
            val d = own?.also {
                it.resize(width, height, dpi)
                it.surface = s
            } ?: Hidden.newDisplay(width, height, dpi, s).also { own = it }
            d.display.displayId
        } else {
            releaseOwn()
            try {
                mirror = Hidden.mirror(width, height, 0, s)
            } catch (e: Exception) {
                Log.i(TAG, "No DisplayManager mirror ($e); SurfaceControl instead")
                mirrorToken = Hidden.surfaceMirror(width, height, s)
            }
            0
        }
        target = id
        Log.i(TAG, "Showing ${width}x$height on display $id")
        id
    }

    private fun stop(keepDisplay: Boolean) = synchronized(lock) {
        stopMirror()
        if (keepDisplay) own?.surface = null else releaseOwn()
        surface?.release()
        surface = null
    }

    private fun stopMirror() {
        mirror?.release()
        mirror = null
        mirrorToken?.let {
            try {
                Hidden.destroyDisplay(it)
            } catch (e: Exception) {
                Log.w(TAG, "Destroying the display failed", e)
            }
        }
        mirrorToken = null
    }

    private fun releaseOwn() {
        own?.release()
        own = null
        target = 0
    }

    fun release() = stop(keepDisplay = false)

    private companion object {
        const val TAG = "glidedesk-shell"
    }
}
