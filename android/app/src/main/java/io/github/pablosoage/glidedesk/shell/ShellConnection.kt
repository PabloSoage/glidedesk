package io.github.pablosoage.glidedesk.shell

import android.content.ComponentName
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.Binder
import android.os.IBinder
import io.github.pablosoage.glidedesk.BuildConfig
import rikka.shizuku.Shizuku

/**
 * The app's link to its privileged helper, whichever started it: a Shizuku user service
 * ([bindShizuku]) or the ADB helper calling [ShellBinderProvider].
 */
object ShellConnection {
    /** Held by the ADB helper, which exits when this process dies. */
    val appToken: IBinder = Binder()

    @Volatile
    var client: ShellClient? = null
        private set

    /** "Shizuku" or "ADB", for the screen. */
    @Volatile
    var via: String = ""
        private set

    fun attach(binder: IBinder, how: String) {
        val c = ShellClient(binder)
        client = c
        via = how
        try {
            binder.linkToDeath({ if (client === c) client = null }, 0)
        } catch (e: Exception) {
            if (client === c) client = null
        }
    }

    enum class ShizukuState { NOT_RUNNING, NO_PERMISSION, READY }

    fun shizukuState(): ShizukuState = when {
        !Shizuku.pingBinder() -> ShizukuState.NOT_RUNNING
        Shizuku.isPreV11() -> ShizukuState.NOT_RUNNING
        Shizuku.checkSelfPermission() != PackageManager.PERMISSION_GRANTED -> ShizukuState.NO_PERMISSION
        else -> ShizukuState.READY
    }

    private val shizukuArgs by lazy {
        Shizuku.UserServiceArgs(ComponentName(BuildConfig.APPLICATION_ID, ScreenShell::class.java.name))
            .tag("screen-shell")
            .processNameSuffix("shell")
            .debuggable(BuildConfig.DEBUG)
            .version(BuildConfig.VERSION_CODE)
            // Ends with the app's process.
            .daemon(false)
    }

    private val shizukuConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, binder: IBinder?) {
            if (binder != null && binder.pingBinder()) attach(binder, "Shizuku")
        }

        override fun onServiceDisconnected(name: ComponentName?) {
            if (via == "Shizuku") client = null
        }
    }

    /** Starts (or reaches) the helper as a Shizuku user service; [client] is set once it is up. */
    fun bindShizuku() {
        if (shizukuState() == ShizukuState.READY) Shizuku.bindUserService(shizukuArgs, shizukuConnection)
    }
}
