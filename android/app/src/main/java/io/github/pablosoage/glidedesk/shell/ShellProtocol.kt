package io.github.pablosoage.glidedesk.shell

import android.os.IBinder
import android.os.Parcel
import android.os.RemoteException
import android.view.Surface

/**
 * The binder between the app and its privileged helper ([ScreenShell], running as the shell
 * user through Shizuku or `adb shell app_process`). Written by hand rather than in AIDL:
 * a few transactions, a [Surface] the only object.
 */
object ShellProtocol {
    const val DESCRIPTOR = "io.github.pablosoage.glidedesk.shell.IScreenShell"
    const val START = IBinder.FIRST_CALL_TRANSACTION
    const val STOP = START + 1
    const val POINTER = START + 2
    const val SCROLL = START + 3
    const val KEY = START + 4
    const val TEXT = START + 5
    const val LAST = TEXT
    /** Shizuku's "destroy" for user services (`Shizuku.bindUserService`); ours as well. */
    const val DESTROY = 16777115

    /** Pointer actions, as the Rust core sends them (`PointerAction`). */
    const val POINTER_DOWN = 0
    const val POINTER_MOVE = 1
    const val POINTER_UP = 2
    const val POINTER_HOVER = 3
}

/** The app's side of the helper's binder. */
class ShellClient(val binder: IBinder) {

    /**
     * Shows the screen on [surface] at [width] × [height]: a copy of the phone's screen, or
     * with [ownDisplay] a display of its own at [dpi]. Returns the display input goes to.
     */
    fun start(surface: Surface, width: Int, height: Int, dpi: Int, ownDisplay: Boolean): Int {
        val data = Parcel.obtain()
        val reply = Parcel.obtain()
        try {
            data.writeInterfaceToken(ShellProtocol.DESCRIPTOR)
            surface.writeToParcel(data, 0)
            data.writeInt(width)
            data.writeInt(height)
            data.writeInt(dpi)
            data.writeInt(if (ownDisplay) 1 else 0)
            binder.transact(ShellProtocol.START, data, reply, 0)
            val id = reply.readInt()
            if (id < 0) throw IllegalStateException(reply.readString() ?: "the helper could not show the screen")
            return id
        } finally {
            data.recycle()
            reply.recycle()
        }
    }

    /** Stops drawing into the surface; with [keepDisplay] a display of its own stays (and its apps). */
    fun stop(keepDisplay: Boolean) {
        val data = Parcel.obtain()
        val reply = Parcel.obtain()
        try {
            data.writeInterfaceToken(ShellProtocol.DESCRIPTOR)
            data.writeInt(if (keepDisplay) 1 else 0)
            binder.transact(ShellProtocol.STOP, data, reply, 0)
        } catch (e: RemoteException) {
            // The helper is gone: nothing draws any more.
        } finally {
            data.recycle()
            reply.recycle()
        }
    }

    /** [x], [y] in pixels of the display input goes to. */
    fun pointer(action: Int, x: Float, y: Float) = oneway(ShellProtocol.POINTER) {
        writeInt(action)
        writeFloat(x)
        writeFloat(y)
    }

    /** Scroll by [h], [v] notches (positive right and up) at [x], [y]. */
    fun scroll(x: Float, y: Float, h: Float, v: Float) = oneway(ShellProtocol.SCROLL) {
        writeFloat(x)
        writeFloat(y)
        writeFloat(h)
        writeFloat(v)
    }

    fun key(down: Boolean, keycode: Int, meta: Int) = oneway(ShellProtocol.KEY) {
        writeInt(if (down) 1 else 0)
        writeInt(keycode)
        writeInt(meta)
    }

    fun text(codepoint: Int) = oneway(ShellProtocol.TEXT) { writeInt(codepoint) }

    /** Ends the helper process. */
    fun destroy() = oneway(ShellProtocol.DESTROY) {}

    /** Input goes one way: in order, without waiting for the injection. */
    private inline fun oneway(code: Int, write: Parcel.() -> Unit) {
        val data = Parcel.obtain()
        try {
            data.writeInterfaceToken(ShellProtocol.DESCRIPTOR)
            data.write()
            binder.transact(code, data, null, IBinder.FLAG_ONEWAY)
        } catch (e: RemoteException) {
            // The helper died; the server notices on the next start.
        } finally {
            data.recycle()
        }
    }
}
