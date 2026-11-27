// The provider lookup and call are adapted from scrcpy (https://github.com/Genymobile/scrcpy),
// commit c939183, files server/src/main/java/com/genymobile/scrcpy/: wrappers/ActivityManager.java
// (getContentProviderExternal), wrappers/ContentProvider.java (call, per Android version).
// Copyright (C) 2018 Genymobile, Copyright (C) 2018-2026 Romain Vimont.
// Licensed under the Apache License, Version 2.0; changes: Kotlin, a binder in the extras.
package io.github.pablosoage.glidedesk.shell

import android.annotation.SuppressLint
import android.content.AttributionSource
import android.os.Binder
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.os.Looper
import android.os.Process
import io.github.pablosoage.glidedesk.BuildConfig
import kotlin.system.exitProcess

/**
 * The helper without Shizuku, started from ADB (a computer, or Termux connected to the
 * phone's own wireless debugging):
 *
 * ```
 * adb shell 'CLASSPATH=$(pm path io.github.pablosoage.glidedesk | head -n 1 | cut -d: -f2) \
 *   nohup app_process /system/bin io.github.pablosoage.glidedesk.shell.ShellMain >/dev/null 2>&1 &'
 * ```
 *
 * It hands its binder to the app through the app's [ShellBinderProvider] (as Shizuku's own
 * server does) and runs until the app's process dies or the app destroys it.
 */
@SuppressLint("PrivateApi", "DiscouragedPrivateApi")
object ShellMain {
    @JvmStatic
    fun main(args: Array<String>) {
        // The system context needs an ActivityThread, whose handler needs a main looper: both
        // made here, on the main thread (binder threads have no looper).
        Looper.prepareMainLooper()
        Hidden.init()
        val shell = ScreenShell()
        val app = try {
            deliver(shell)
        } catch (e: Exception) {
            System.err.println("glidedesk: cannot reach the app: $e")
            null
        }
        if (app == null) {
            System.err.println("glidedesk: is the app installed, and the same version as this helper?")
            exitProcess(1)
        }
        app.linkToDeath({
            shell.release()
            exitProcess(0)
        }, 0)
        println("glidedesk: helper running for the app (uid ${Process.myUid()})")
        Looper.loop()
    }

    /** Calls the app's provider with the helper's binder; returns the app's token. */
    private fun deliver(shell: IBinder): IBinder? {
        val authority = BuildConfig.APPLICATION_ID + ShellBinderProvider.AUTHORITY_SUFFIX
        val am = Class.forName("android.app.ActivityManagerNative").getMethod("getDefault").invoke(null)
            ?: return null
        val token = Binder()
        val int = Int::class.javaPrimitiveType
        val holder = try {
            am.javaClass.getMethod("getContentProviderExternal", String::class.java, int, IBinder::class.java, String::class.java)
                .invoke(am, authority, 0, token, null)
        } catch (e: NoSuchMethodException) {
            am.javaClass.getMethod("getContentProviderExternal", String::class.java, int, IBinder::class.java)
                .invoke(am, authority, 0, token)
        } ?: return null
        val field = holder.javaClass.getDeclaredField("provider")
        field.isAccessible = true
        val provider = field.get(holder) ?: return null
        val extras = Bundle().apply { putBinder(ShellBinderProvider.KEY_BINDER, shell) }
        try {
            val reply = call(provider, authority, ShellBinderProvider.METHOD_ATTACH, extras)
            return reply?.getBinder(ShellBinderProvider.KEY_BINDER)
        } finally {
            try {
                am.javaClass.getMethod("removeContentProviderExternal", String::class.java, IBinder::class.java)
                    .invoke(am, authority, token)
            } catch (e: ReflectiveOperationException) {
                // Only bookkeeping in the system server.
            }
        }
    }

    /** `IContentProvider.call`, whose parameters changed with Android 11 and 12. */
    private fun call(provider: Any, authority: String, method: String, extras: Bundle): Bundle? {
        val icp = Class.forName("android.content.IContentProvider")
        val s = String::class.java
        val b = Bundle::class.java
        val reply = if (Build.VERSION.SDK_INT >= 31) {
            val source = AttributionSource.Builder(Process.myUid()).setPackageName(Hidden.SHELL_PACKAGE).build()
            icp.getMethod("call", AttributionSource::class.java, s, s, s, b)
                .invoke(provider, source, authority, method, null, extras)
        } else if (Build.VERSION.SDK_INT == 30) {
            icp.getMethod("call", s, s, s, s, s, b)
                .invoke(provider, Hidden.SHELL_PACKAGE, null, authority, method, null, extras)
        } else {
            icp.getMethod("call", s, s, s, s, b)
                .invoke(provider, Hidden.SHELL_PACKAGE, authority, method, null, extras)
        }
        return reply as Bundle?
    }
}
