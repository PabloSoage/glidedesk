package io.github.pablosoage.glidedesk.server

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.Intent
import android.graphics.Path
import android.graphics.PointF
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import io.github.pablosoage.glidedesk.shell.ShellProtocol

/**
 * The client's input without privileges, for the MediaProjection server: the left button as
 * one finger (`dispatchGesture`, the stroke continued as the mouse moves), the wheel as a
 * swipe, Back, Home, Recents and the notifications as global actions, and typing as edits
 * of the focused text field. Real key events cannot be sent this way (arrows only move the
 * cursor in text, shortcuts other than copy, cut, paste and select all do nothing).
 */
class InputAccessibilityService : AccessibilityService() {
    private val main = Handler(Looper.getMainLooper())
    private val finger = Finger()
    private val charMap by lazy { KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD) }

    override fun onServiceConnected() {
        instance = this
    }

    override fun onUnbind(intent: Intent?): Boolean {
        instance = null
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        instance = null
        super.onDestroy()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    // Called from the server's thread; all the work happens on the main thread.

    fun pointer(action: Int, x: Float, y: Float) {
        main.post { finger.pointer(action, x, y) }
    }

    fun scroll(x: Float, y: Float, h: Float, v: Float) {
        main.post { swipe(x, y, h, v) }
    }

    fun key(down: Boolean, keycode: Int, meta: Int) {
        if (down) main.post { keyDown(keycode, meta) }
    }

    fun text(codepoint: Int) {
        main.post { insert(String(Character.toChars(codepoint))) }
    }

    /** One finger that follows the mouse while the button is down: each part of the path a
     * stroke continuing the last, sent once the previous one has been played. */
    private inner class Finger {
        private var stroke: GestureDescription.StrokeDescription? = null
        /** Where the strokes sent so far end. */
        private var end: PointF? = null
        private val pending = ArrayList<PointF>()
        private var latest = PointF()
        private var down = false
        private var lifting = false
        private var busy = false
        private var sentAt = 0L

        fun pointer(action: Int, x: Float, y: Float) {
            // Gesture paths may not go negative.
            val p = PointF(x.coerceAtLeast(0f), y.coerceAtLeast(0f))
            when (action) {
                ShellProtocol.POINTER_DOWN -> {
                    if (down) return
                    stroke = null
                    end = null
                    pending.clear()
                    pending += p
                    down = true
                    lifting = false
                    sentAt = SystemClock.uptimeMillis()
                }
                ShellProtocol.POINTER_MOVE -> if (down) pending += p else return
                ShellProtocol.POINTER_UP -> {
                    if (!down) return
                    pending += p
                    down = false
                    lifting = true
                }
                else -> return
            }
            latest = p
            send()
        }

        val idle get() = !down && !busy && !lifting

        private fun send() {
            if (busy || (pending.isEmpty() && !lifting)) return
            val from = end ?: pending.removeFirstOrNull() ?: return
            val path = Path().apply { moveTo(from.x, from.y) }
            // A path that goes nowhere is a touch held in place.
            if (pending.isEmpty()) path.lineTo(from.x, from.y) else pending.forEach { path.lineTo(it.x, it.y) }
            val to = pending.lastOrNull() ?: from
            pending.clear()
            val now = SystemClock.uptimeMillis()
            val duration = (now - sentAt).coerceIn(MIN_STROKE_MS, MAX_STROKE_MS)
            sentAt = now
            val more = !lifting
            val s = stroke?.continueStroke(path, 0, duration, more)
                ?: GestureDescription.StrokeDescription(path, 0, duration, more)
            if (more) {
                stroke = s
                end = to
            } else {
                stroke = null
                end = null
                lifting = false
            }
            busy = true
            val gesture = GestureDescription.Builder().addStroke(s).build()
            if (!dispatchGesture(gesture, done, main)) busy = false
        }

        private val done = object : AccessibilityService.GestureResultCallback() {
            override fun onCompleted(gestureDescription: GestureDescription?) {
                busy = false
                send()
            }

            override fun onCancelled(gestureDescription: GestureDescription?) {
                busy = false
                // The finger was lifted by the system: put it down again where the mouse is.
                stroke = null
                end = null
                if (down) pending.add(0, latest)
                send()
            }
        }
    }

    /** The wheel as a swipe from the pointer (positive [v]: the content goes down, as up the wheel). */
    private fun swipe(x: Float, y: Float, h: Float, v: Float) {
        if (!finger.idle) return
        val step = resources.displayMetrics.density * SWIPE_DP_PER_NOTCH
        val path = Path().apply {
            moveTo(x.coerceAtLeast(0f), y.coerceAtLeast(0f))
            lineTo((x - h * step).coerceAtLeast(0f), (y + v * step).coerceAtLeast(0f))
        }
        val stroke = GestureDescription.StrokeDescription(path, 0, SWIPE_MS)
        dispatchGesture(GestureDescription.Builder().addStroke(stroke).build(), null, main)
    }

    private fun keyDown(keycode: Int, meta: Int) {
        val ctrl = meta and KeyEvent.META_CTRL_ON != 0
        when (keycode) {
            KeyEvent.KEYCODE_BACK, KeyEvent.KEYCODE_ESCAPE -> performGlobalAction(GLOBAL_ACTION_BACK)
            KeyEvent.KEYCODE_HOME, KeyEvent.KEYCODE_META_LEFT, KeyEvent.KEYCODE_META_RIGHT ->
                performGlobalAction(GLOBAL_ACTION_HOME)
            KeyEvent.KEYCODE_APP_SWITCH -> performGlobalAction(GLOBAL_ACTION_RECENTS)
            KeyEvent.KEYCODE_NOTIFICATION -> performGlobalAction(GLOBAL_ACTION_NOTIFICATIONS)
            KeyEvent.KEYCODE_DEL -> delete(before = true)
            KeyEvent.KEYCODE_FORWARD_DEL -> delete(before = false)
            KeyEvent.KEYCODE_DPAD_LEFT -> moveCursor(forward = false)
            KeyEvent.KEYCODE_DPAD_RIGHT -> moveCursor(forward = true)
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> enter()
            else -> when {
                ctrl && keycode == KeyEvent.KEYCODE_C -> focused()?.performAction(AccessibilityNodeInfo.ACTION_COPY)
                ctrl && keycode == KeyEvent.KEYCODE_X -> focused()?.performAction(AccessibilityNodeInfo.ACTION_CUT)
                ctrl && keycode == KeyEvent.KEYCODE_V -> focused()?.performAction(AccessibilityNodeInfo.ACTION_PASTE)
                ctrl && keycode == KeyEvent.KEYCODE_A -> focused()?.let { select(it, 0, fieldText(it).length) }
                ctrl -> {}
                else -> {
                    val c = charMap.get(keycode, meta)
                    // 0: no character; the accent bit: a dead key, which needs a next key.
                    if (c != 0 && c and KeyCharacterMap.COMBINING_ACCENT == 0) {
                        insert(String(Character.toChars(c)))
                    }
                }
            }
        }
    }

    /** The text field with the input focus, if any. */
    private fun focused(): AccessibilityNodeInfo? =
        findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.takeIf { it.isEditable }

    private fun fieldText(node: AccessibilityNodeInfo): String =
        if (node.isShowingHintText) "" else node.text?.toString() ?: ""

    /** The selection, in order; the end of the text when the field reports none. */
    private fun selection(node: AccessibilityNodeInfo, length: Int): Pair<Int, Int> {
        val a = node.textSelectionStart
        val b = node.textSelectionEnd
        if (a < 0 || b < 0) return length to length
        return minOf(a, b).coerceAtMost(length) to maxOf(a, b).coerceAtMost(length)
    }

    private fun setText(node: AccessibilityNodeInfo, text: String, cursor: Int) {
        val args = Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text)
        }
        node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
        select(node, cursor, cursor)
    }

    private fun select(node: AccessibilityNodeInfo, start: Int, end: Int) {
        val args = Bundle().apply {
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, start)
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, end)
        }
        node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, args)
    }

    /** Types [s] over the selection of the focused field. (Password fields hide their text,
     * so typing there replaces it.) */
    private fun insert(s: String) {
        val node = focused() ?: return
        val t = fieldText(node)
        val (a, b) = selection(node, t.length)
        setText(node, t.substring(0, a) + s + t.substring(b), a + s.length)
    }

    private fun delete(before: Boolean) {
        val node = focused() ?: return
        val t = fieldText(node)
        var (a, b) = selection(node, t.length)
        if (a == b) {
            if (before && a > 0) a = t.offsetByCodePoints(a, -1)
            else if (!before && b < t.length) b = t.offsetByCodePoints(b, 1)
            else return
        }
        setText(node, t.substring(0, a) + t.substring(b), a)
    }

    private fun moveCursor(forward: Boolean) {
        val node = focused() ?: return
        val args = Bundle().apply {
            putInt(
                AccessibilityNodeInfo.ACTION_ARGUMENT_MOVEMENT_GRANULARITY_INT,
                AccessibilityNodeInfo.MOVEMENT_GRANULARITY_CHARACTER,
            )
            putBoolean(AccessibilityNodeInfo.ACTION_ARGUMENT_EXTEND_SELECTION_BOOLEAN, false)
        }
        val action = if (forward) {
            AccessibilityNodeInfo.ACTION_NEXT_AT_MOVEMENT_GRANULARITY
        } else {
            AccessibilityNodeInfo.ACTION_PREVIOUS_AT_MOVEMENT_GRANULARITY
        }
        node.performAction(action, args)
    }

    private fun enter() {
        val node = focused() ?: return
        if (Build.VERSION.SDK_INT >= 30) {
            node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)
        } else if (node.isMultiLine) {
            insert("\n")
        }
    }

    companion object {
        /** The running service, once the user has turned it on in the accessibility settings. */
        @Volatile
        var instance: InputAccessibilityService? = null
            private set

        private const val MIN_STROKE_MS = 10L
        private const val MAX_STROKE_MS = 500L
        private const val SWIPE_DP_PER_NOTCH = 60f
        private const val SWIPE_MS = 200L
    }
}
