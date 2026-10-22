package io.github.pablosoage.glidedesk

import android.annotation.SuppressLint
import android.content.Context
import android.view.GestureDetector
import android.os.Build
import android.view.KeyEvent
import android.view.Surface
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import kotlin.math.abs
import kotlin.math.roundToInt

/**
 * The remote desktop. The decoder renders into this view's surface; the desktop is
 * [scale] times smaller than the view in pixels (a 2400 px phone at scale 2 asks for a
 * 1200 px desktop), and asks for a new desktop size whenever the view changes size.
 *
 * Touch, like a touchscreen on the remote side: tap = click, drag = drag with the left
 * button, long press = right click, two fingers = scroll. A physical keyboard sends scan
 * codes (Android reports Linux key codes, which match RDP scan codes); the soft keyboard
 * sends Unicode.
 */
@SuppressLint("ViewConstructor")
class SessionView(context: Context, private val session: NativeSession, private val scale: Float) :
    SurfaceView(context), SurfaceHolder.Callback {

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
    }

    private fun remote(v: Float) = (v / scale).roundToInt()

    override fun surfaceCreated(holder: SurfaceHolder) {}

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        session.resize(remote(width.toFloat()), remote(height.toFloat()))
        session.setSurface(holder.surface)
        // Dynamic-refresh phones drop to 60 Hz unless the surface says it carries more.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val hz = display?.supportedModes?.maxOfOrNull { it.refreshRate } ?: return
            holder.surface.setFrameRate(hz, Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        session.setSurface(null)
    }

    // --- touch ---

    private var dragging = false
    private var scrollAccum = 0f

    private val gestures = GestureDetector(context, object : GestureDetector.SimpleOnGestureListener() {
        override fun onSingleTapUp(e: MotionEvent): Boolean {
            session.mouseMove(remote(e.x), remote(e.y))
            session.mouseButton(0, true)
            session.mouseButton(0, false)
            return true
        }

        override fun onLongPress(e: MotionEvent) {
            session.mouseMove(remote(e.x), remote(e.y))
            session.mouseButton(2, true)
            session.mouseButton(2, false)
        }
    })

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(e: MotionEvent): Boolean {
        if (e.pointerCount >= 2) {
            // Two fingers: vertical scroll, 120 units per ~40 px of movement.
            if (e.actionMasked == MotionEvent.ACTION_MOVE && e.historySize > 0) {
                val dy = e.getY(0) - e.getHistoricalY(0, e.historySize - 1)
                scrollAccum += dy
                if (abs(scrollAccum) >= 40f) {
                    session.wheel(true, if (scrollAccum > 0) 120 else -120)
                    scrollAccum = 0f
                }
            }
            if (dragging) {
                session.mouseButton(0, false)
                dragging = false
            }
            return true
        }
        val handled = gestures.onTouchEvent(e)
        when (e.actionMasked) {
            MotionEvent.ACTION_MOVE -> {
                session.mouseMove(remote(e.x), remote(e.y))
                if (!dragging && e.eventTime - e.downTime > 150) {
                    session.mouseButton(0, true)
                    dragging = true
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (dragging) {
                    session.mouseMove(remote(e.x), remote(e.y))
                    session.mouseButton(0, false)
                    dragging = false
                }
            }
        }
        return handled || true
    }

    override fun onGenericMotionEvent(e: MotionEvent): Boolean {
        // Mouse wheel and hover from a connected mouse.
        if (e.actionMasked == MotionEvent.ACTION_SCROLL) {
            val v = e.getAxisValue(MotionEvent.AXIS_VSCROLL)
            if (v != 0f) session.wheel(true, (v * 120).roundToInt())
            return true
        }
        if (e.actionMasked == MotionEvent.ACTION_HOVER_MOVE) {
            session.mouseMove(remote(e.x), remote(e.y))
            return true
        }
        return super.onGenericMotionEvent(e)
    }

    // --- keyboard ---

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean = key(event, true) || super.onKeyDown(keyCode, event)
    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean = key(event, false) || super.onKeyUp(keyCode, event)

    private fun key(event: KeyEvent, pressed: Boolean): Boolean {
        if (event.keyCode == KeyEvent.KEYCODE_BACK) return false
        val sc = Keymap.toRdp(event.scanCode) ?: run {
            // Soft keyboard keys without a scan code: send the character.
            val ch = event.unicodeChar
            if (ch != 0) session.unicode(ch, pressed)
            return ch != 0
        }
        session.key(sc.first, sc.second, pressed)
        return true
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = EditorInfo.TYPE_CLASS_TEXT or EditorInfo.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI
        return object : BaseInputConnection(this, false) {
            override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
                text.codePoints().forEach {
                    session.unicode(it, true)
                    session.unicode(it, false)
                }
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                repeat(beforeLength.coerceAtLeast(1)) {
                    session.key(0x0e, false, true)
                    session.key(0x0e, false, false)
                }
                return true
            }
        }
    }
}

/** Linux key codes (Android `KeyEvent.getScanCode`) to RDP scan codes (set 1). */
object Keymap {
    private val extended = mapOf(
        96 to 0x1c, 97 to 0x1d, 98 to 0x35, 99 to 0x37, 100 to 0x38, 102 to 0x47, 103 to 0x48,
        104 to 0x49, 105 to 0x4b, 106 to 0x4d, 107 to 0x4f, 108 to 0x50, 109 to 0x51, 110 to 0x52,
        111 to 0x53, 125 to 0x5b, 126 to 0x5c, 127 to 0x5d,
    )

    /** (scan code, extended) or null when there is no physical key behind the event. */
    fun toRdp(linux: Int): Pair<Int, Boolean>? = when {
        linux in 1..0x58 -> linux to false
        linux in extended -> extended.getValue(linux) to true
        else -> null
    }
}
