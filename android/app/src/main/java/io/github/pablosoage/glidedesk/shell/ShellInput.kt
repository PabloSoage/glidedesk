// Adapted from scrcpy (https://github.com/Genymobile/scrcpy), commit c939183, files
// server/src/main/java/com/genymobile/scrcpy/: control/Controller.java (injectTouch,
// injectScroll, injectChar), device/Device.java (injectEvent, injectKeyEvent),
// wrappers/InputManager.java (injectInputEvent, setDisplayId).
// Copyright (C) 2018 Genymobile, Copyright (C) 2018-2026 Romain Vimont.
// Licensed under the Apache License, Version 2.0; changes: Kotlin, one pointer, no UHID.
package io.github.pablosoage.glidedesk.shell

import android.content.Context
import android.hardware.input.InputManager
import android.os.SystemClock
import android.util.Log
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent

/**
 * Input injected with `InputManager.injectInputEvent` (the shell user holds INJECT_EVENTS):
 * the left button as a finger, the mouse moving without buttons as a hovering mouse, the
 * wheel as mouse scrolling, keys from a virtual keyboard.
 */
internal class ShellInput(
    /** Where events go: 0 for the phone's screen, else the display of its own. */
    private val displayId: () -> Int,
) {
    private val manager = Hidden.context.getSystemService(Context.INPUT_SERVICE) as InputManager
    private val injectMethod = InputManager::class.java
        .getMethod("injectInputEvent", InputEvent::class.java, Int::class.javaPrimitiveType)
    private val setDisplayId = InputEvent::class.java.getMethod("setDisplayId", Int::class.javaPrimitiveType)
    private val charMap = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD)

    private var downTime = 0L
    private val properties = arrayOf(MotionEvent.PointerProperties().apply { id = 0 })
    private val coords = arrayOf(MotionEvent.PointerCoords().apply { size = 1f })

    private fun inject(event: InputEvent) {
        try {
            val target = displayId()
            if (target != 0) setDisplayId.invoke(event, target)
            injectMethod.invoke(manager, event, INJECT_ASYNC)
        } catch (e: ReflectiveOperationException) {
            Log.w(TAG, "Injecting $event failed", e)
        }
    }

    fun pointer(action: Int, x: Float, y: Float) {
        val now = SystemClock.uptimeMillis()
        val hover = action == ShellProtocol.POINTER_HOVER
        val motion = when (action) {
            ShellProtocol.POINTER_DOWN -> {
                downTime = now
                MotionEvent.ACTION_DOWN
            }
            ShellProtocol.POINTER_MOVE -> MotionEvent.ACTION_MOVE
            ShellProtocol.POINTER_UP -> MotionEvent.ACTION_UP
            else -> MotionEvent.ACTION_HOVER_MOVE
        }
        properties[0].toolType = if (hover) MotionEvent.TOOL_TYPE_MOUSE else MotionEvent.TOOL_TYPE_FINGER
        coords[0].apply {
            this.x = x
            this.y = y
            pressure = if (motion == MotionEvent.ACTION_UP || hover) 0f else 1f
            setAxisValue(MotionEvent.AXIS_HSCROLL, 0f)
            setAxisValue(MotionEvent.AXIS_VSCROLL, 0f)
        }
        val source = if (hover) InputDevice.SOURCE_MOUSE else InputDevice.SOURCE_TOUCHSCREEN
        val event = MotionEvent.obtain(
            if (hover) now else downTime, now, motion, 1, properties, coords,
            0, 0, 1f, 1f, 0, 0, source, 0,
        )
        inject(event)
        event.recycle()
    }

    fun scroll(x: Float, y: Float, h: Float, v: Float) {
        val now = SystemClock.uptimeMillis()
        properties[0].toolType = MotionEvent.TOOL_TYPE_MOUSE
        coords[0].apply {
            this.x = x
            this.y = y
            pressure = 0f
            setAxisValue(MotionEvent.AXIS_HSCROLL, h)
            setAxisValue(MotionEvent.AXIS_VSCROLL, v)
        }
        val event = MotionEvent.obtain(
            now, now, MotionEvent.ACTION_SCROLL, 1, properties, coords,
            0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_MOUSE, 0,
        )
        inject(event)
        event.recycle()
    }

    fun key(down: Boolean, keycode: Int, meta: Int) {
        val now = SystemClock.uptimeMillis()
        val action = if (down) KeyEvent.ACTION_DOWN else KeyEvent.ACTION_UP
        inject(
            KeyEvent(now, now, action, keycode, 0, meta, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD),
        )
    }

    /** Text with no key: the key presses that type it on a virtual keyboard, if any do. */
    fun text(codepoint: Int) {
        val events = charMap.getEvents(Character.toChars(codepoint)) ?: return
        for (e in events) inject(e)
    }

    private companion object {
        const val TAG = "glidedesk-shell"
        /** `InputManager.INJECT_INPUT_EVENT_MODE_ASYNC`. */
        const val INJECT_ASYNC = 0
    }
}
