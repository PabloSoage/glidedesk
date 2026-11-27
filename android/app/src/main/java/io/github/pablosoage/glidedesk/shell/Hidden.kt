// Adapted from scrcpy (https://github.com/Genymobile/scrcpy), commit c939183, files
// server/src/main/java/com/genymobile/scrcpy/: FakeContext.java, wrappers/DisplayManager.java,
// wrappers/SurfaceControl.java, video/ScreenCapture.java, video/NewDisplayCapture.java.
// Copyright (C) 2018 Genymobile, Copyright (C) 2018-2026 Romain Vimont.
// Licensed under the Apache License, Version 2.0; changes: Kotlin, only the calls used here.
package io.github.pablosoage.glidedesk.shell

import android.annotation.SuppressLint
import android.content.AttributionSource
import android.content.Context
import android.content.ContextWrapper
import android.graphics.Rect
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.os.Build
import android.os.IBinder
import android.os.Process
import android.view.Surface
import androidx.annotation.RequiresApi

/**
 * Hidden Android APIs the helper calls by reflection, as scrcpy's server does. Only valid in
 * the helper's process (shell user, started by Shizuku or `app_process`), where the hidden
 * API restrictions of apps do not apply.
 */
@SuppressLint("PrivateApi", "DiscouragedPrivateApi", "BlockedPrivateApi")
internal object Hidden {
    const val SHELL_PACKAGE = "com.android.shell"

    /**
     * The process's system context. Shizuku's user service process already has an
     * `ActivityThread` (it called `systemMain`); a bare `app_process` gets one here (its main
     * thread must have a looper first, see [ShellMain]).
     */
    private val systemContext: Context by lazy {
        val at = Class.forName("android.app.ActivityThread")
        val thread = at.getMethod("currentActivityThread").invoke(null)
            ?: at.getMethod("systemMain").invoke(null)
        at.getMethod("getSystemContext").invoke(thread) as Context
    }

    /** Makes the system context now, on the calling thread (which needs a looper). */
    fun init(): Context = context

    /** A context that calls itself the shell package, which the shell user owns (scrcpy's FakeContext). */
    val context: Context by lazy { ShellContext(systemContext) }

    private class ShellContext(base: Context) : ContextWrapper(base) {
        override fun getPackageName(): String = SHELL_PACKAGE
        override fun getOpPackageName(): String = SHELL_PACKAGE
        override fun getApplicationContext(): Context = this

        @RequiresApi(31)
        override fun getAttributionSource(): AttributionSource =
            AttributionSource.Builder(Process.myUid()).setPackageName(SHELL_PACKAGE).build()
    }

    /** What the helper needs to know of the phone's screen. */
    class DisplayInfo(val width: Int, val height: Int, val layerStack: Int, val dpi: Int)

    /** The main display, rotated as shown (`DisplayManagerGlobal.getDisplayInfo`). */
    fun mainDisplay(): DisplayInfo {
        val dmg = Class.forName("android.hardware.display.DisplayManagerGlobal")
        val global = dmg.getDeclaredMethod("getInstance").invoke(null)
        val info = dmg.getMethod("getDisplayInfo", Int::class.javaPrimitiveType).invoke(global, 0)
            ?: throw IllegalStateException("no main display")
        val c = info.javaClass
        return DisplayInfo(
            c.getDeclaredField("logicalWidth").getInt(info),
            c.getDeclaredField("logicalHeight").getInt(info),
            c.getDeclaredField("layerStack").getInt(info),
            c.getDeclaredField("logicalDensityDpi").getInt(info),
        )
    }

    /** A copy of display [displayId] scaled into [surface] (Android 14 on: `DisplayManager.createVirtualDisplay`, static and hidden). */
    fun mirror(width: Int, height: Int, displayId: Int, surface: Surface): VirtualDisplay {
        val int = Int::class.javaPrimitiveType
        return DisplayManager::class.java
            .getMethod("createVirtualDisplay", String::class.java, int, int, int, Surface::class.java)
            .invoke(null, NAME, width, height, displayId, surface) as VirtualDisplay
    }

    private val surfaceControl: Class<*> by lazy { Class.forName("android.view.SurfaceControl") }

    /** The same through `SurfaceControl` (Android 10 to 13): a display token, for [destroyDisplay]. */
    fun surfaceMirror(width: Int, height: Int, surface: Surface): IBinder {
        val sc = surfaceControl
        val int = Int::class.javaPrimitiveType
        // Since Android 12 the shell user cannot create secure displays.
        val secure = Build.VERSION.SDK_INT < 30 || (Build.VERSION.SDK_INT == 30 && Build.VERSION.CODENAME != "S")
        val token = sc.getMethod("createDisplay", String::class.java, Boolean::class.javaPrimitiveType)
            .invoke(null, NAME, secure) as IBinder
        val main = mainDisplay()
        sc.getMethod("openTransaction").invoke(null)
        try {
            sc.getMethod("setDisplaySurface", IBinder::class.java, Surface::class.java).invoke(null, token, surface)
            sc.getMethod("setDisplayProjection", IBinder::class.java, int, Rect::class.java, Rect::class.java)
                .invoke(null, token, 0, Rect(0, 0, main.width, main.height), Rect(0, 0, width, height))
            sc.getMethod("setDisplayLayerStack", IBinder::class.java, int).invoke(null, token, main.layerStack)
        } finally {
            sc.getMethod("closeTransaction").invoke(null)
        }
        return token
    }

    fun destroyDisplay(token: IBinder) {
        surfaceControl.getMethod("destroyDisplay", IBinder::class.java).invoke(null, token)
    }

    // DisplayManager.VIRTUAL_DISPLAY_FLAG_*, the hidden ones by value (NewDisplayCapture.java).
    private const val FLAG_SUPPORTS_TOUCH = 1 shl 6
    private const val FLAG_DESTROY_CONTENT_ON_REMOVAL = 1 shl 8
    private const val FLAG_SHOULD_SHOW_SYSTEM_DECORATIONS = 1 shl 9
    private const val FLAG_TRUSTED = 1 shl 10
    private const val FLAG_OWN_DISPLAY_GROUP = 1 shl 11
    private const val FLAG_ALWAYS_UNLOCKED = 1 shl 12
    private const val FLAG_TOUCH_FEEDBACK_DISABLED = 1 shl 13
    private const val FLAG_OWN_FOCUS = 1 shl 14
    private const val FLAG_DEVICE_DISPLAY_GROUP = 1 shl 15

    /**
     * A new display of its own (with a launcher and the system bars where the phone shows
     * them on other displays), drawn into [surface]. It does not rotate: apps that want
     * another orientation are letterboxed in it.
     */
    fun newDisplay(width: Int, height: Int, dpi: Int, surface: Surface): VirtualDisplay {
        var flags = DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC or
            DisplayManager.VIRTUAL_DISPLAY_FLAG_PRESENTATION or
            DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_CONTENT_ONLY or
            FLAG_SUPPORTS_TOUCH or
            FLAG_DESTROY_CONTENT_ON_REMOVAL or
            FLAG_SHOULD_SHOW_SYSTEM_DECORATIONS
        if (Build.VERSION.SDK_INT >= 33) {
            flags = flags or FLAG_TRUSTED or FLAG_OWN_DISPLAY_GROUP or FLAG_ALWAYS_UNLOCKED or
                FLAG_TOUCH_FEEDBACK_DISABLED
        }
        if (Build.VERSION.SDK_INT >= 34) {
            flags = flags or FLAG_OWN_FOCUS or FLAG_DEVICE_DISPLAY_GROUP
        }
        val ctor = DisplayManager::class.java.getDeclaredConstructor(Context::class.java)
        ctor.isAccessible = true
        val dm = ctor.newInstance(context)
        return dm.createVirtualDisplay(NAME, width, height, dpi, surface, flags)
            ?: throw IllegalStateException("the system refused the new display")
    }

    private const val NAME = "glidedesk"
}
