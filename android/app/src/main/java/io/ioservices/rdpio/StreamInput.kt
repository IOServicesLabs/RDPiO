package io.ioservices.rdpio

import android.graphics.Rect
import android.os.Build
import android.os.SystemClock
import android.view.InputDevice
import android.view.MotionEvent
import android.view.ViewConfiguration
import kotlin.math.hypot
import kotlin.math.ln
import kotlin.math.roundToInt

/**
 * Pointer input → the PC. Three kinds of device at once, told apart per event:
 *
 *  - **Fingers on the screen** (`SOURCE_TOUCHSCREEN`): Windows touch on the PC
 *    — taps, flicks, press-and-hold, pinch — where the PC takes it; else a
 *    finger drives the mouse (tap = click, hold = right click, drag = drag,
 *    two fingers = scroll / right-click tap).
 *  - **Mice and trackpads** (`SOURCE_MOUSE`, the keyboard cover's trackpad
 *    included): Android's own pointer, its position and buttons; wheel and
 *    trackpad scrolling as smooth wheel deltas; a trackpad's two-finger swipe
 *    scrolls and its pinch zooms (Ctrl + wheel), as on a Windows precision
 *    touchpad.
 *  - **Captured** (mouse locked for games): raw relative motion.
 *
 * Positions are normalised to [picture] (the part of the screen showing the
 * stream; the bars around it clamp to its edges).
 */
class StreamInput(private val density: Float) {
    /** The stream's rectangle on screen, in the root view's coordinates. */
    val picture = Rect()
    /** The PC injects touch; else fingers are a mouse. */
    var hostTouch = false
    /** The user's choice (menu): fingers as touch, or as a mouse. */
    var fingersAsTouch = true

    private var buttons = 0
    private var wheelRest = 0f
    private var hwheelRest = 0f

    private fun nx(x: Float) = (((x - picture.left) / picture.width().coerceAtLeast(1)) * 65535f).roundToInt().coerceIn(0, 65535)
    private fun ny(y: Float) = (((y - picture.top) / picture.height().coerceAtLeast(1)) * 65535f).roundToInt().coerceIn(0, 65535)

    // ---- Mouse and trackpad ----

    private fun isPointerDevice(e: MotionEvent) =
        e.isFromSource(InputDevice.SOURCE_MOUSE) || e.isFromSource(InputDevice.SOURCE_TOUCHPAD) ||
            e.getToolType(0) == MotionEvent.TOOL_TYPE_MOUSE

    /** Buttons pressed or released since the last event. */
    private fun syncButtons(state: Int) {
        val changed = state xor buttons
        if (changed == 0) return
        for ((bit, b) in BUTTONS) {
            if (changed and bit != 0) Native.button(b, state and bit != 0)
        }
        buttons = state
    }

    private fun wheel(vertical: Float, horizontal: Float) {
        // Windows' delta units, kept fractional between events: a trackpad
        // scrolls in small steps, which Windows apps scroll smoothly by.
        wheelRest += vertical * 120f
        hwheelRest += horizontal * 120f
        val v = wheelRest.toInt()
        if (v != 0) {
            Native.wheel(v, false)
            wheelRest -= v
        }
        val h = hwheelRest.toInt()
        if (h != 0) {
            Native.wheel(h, true)
            hwheelRest -= h
        }
    }

    // A trackpad gesture in progress (Android 14+ classifies them).
    private var gesture = 0
    private var gestureLastX = 0f
    private var gestureLastY = 0f
    private var pinchRest = 0f
    private var pinchCtrl = false

    /** Android 14+: a trackpad's two-finger swipe or pinch. True if handled. */
    private fun trackpadGesture(e: MotionEvent): Boolean {
        if (Build.VERSION.SDK_INT < 34) return false
        val classified = e.classification.let {
            it == MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE || it == MotionEvent.CLASSIFICATION_PINCH || it == CLASSIFICATION_MULTI_FINGER_SWIPE
        }
        // (Once started, a gesture is ours until it ends, whatever its last
        // events say: a pinch's Ctrl is always let go.)
        if (!classified && gesture == 0) return false
        val c = if (gesture != 0) gesture else e.classification
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                val c = e.classification
                gesture = c
                gestureLastX = e.x
                gestureLastY = e.y
                pinchRest = 0f
                if (c == MotionEvent.CLASSIFICATION_PINCH && !pinchCtrl) {
                    // Ctrl + wheel = zoom, in browsers, Office, Explorer, maps.
                    Native.key(29, 113, true)
                    pinchCtrl = true
                }
            }
            MotionEvent.ACTION_MOVE, MotionEvent.ACTION_POINTER_DOWN, MotionEvent.ACTION_POINTER_UP -> when (c) {
                MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE -> {
                    // (The fake finger follows the swipe: its travel is the
                    // scroll. Content follows the fingers, as Android scrolls.)
                    val dx = e.x - gestureLastX
                    val dy = e.y - gestureLastY
                    gestureLastX = e.x
                    gestureLastY = e.y
                    val notch = 48f * density // fingers' travel per wheel notch
                    wheel(dy / notch, -dx / notch)
                }
                MotionEvent.CLASSIFICATION_PINCH -> {
                    val f = e.getAxisValue(MotionEvent.AXIS_GESTURE_PINCH_SCALE_FACTOR)
                    if (f > 0f) {
                        pinchRest += ln(f) * 600f // ~ a notch per 20% zoom
                        val d = pinchRest.toInt()
                        if (d != 0) {
                            Native.wheel(d, false)
                            pinchRest -= d
                        }
                    }
                }
                else -> {} // three or four fingers: Android's own (back, home)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (pinchCtrl) {
                    Native.key(29, 113, false)
                    pinchCtrl = false
                }
                gesture = 0
            }
        }
        return true
    }

    /** Mouse / trackpad events (touch and generic). True if handled. */
    fun pointer(e: MotionEvent): Boolean {
        if (trackpadGesture(e)) return true
        if (!isPointerDevice(e)) return false
        when (e.actionMasked) {
            MotionEvent.ACTION_SCROLL -> {
                wheel(e.getAxisValue(MotionEvent.AXIS_VSCROLL), e.getAxisValue(MotionEvent.AXIS_HSCROLL))
                return true
            }
            MotionEvent.ACTION_HOVER_EXIT -> return true
        }
        // Position first, then the buttons (a click lands where the pointer is).
        Native.mouseAbs(nx(e.x), ny(e.y))
        syncButtons(e.buttonState)
        if (e.actionMasked == MotionEvent.ACTION_CANCEL) syncButtons(0)
        return true
    }

    // ---- Captured (mouse locked) ----

    private var capLastX = Float.NaN
    private var capLastY = Float.NaN
    private var capRestX = 0f
    private var capRestY = 0f

    fun captured(e: MotionEvent): Boolean {
        when {
            e.actionMasked == MotionEvent.ACTION_SCROLL ->
                wheel(e.getAxisValue(MotionEvent.AXIS_VSCROLL), e.getAxisValue(MotionEvent.AXIS_HSCROLL))
            e.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE) -> {
                // Counts straight from the mouse (no acceleration): what games want.
                for (i in 0 until e.historySize) move(e.getHistoricalAxisValue(MotionEvent.AXIS_RELATIVE_X, i), e.getHistoricalAxisValue(MotionEvent.AXIS_RELATIVE_Y, i))
                move(e.x, e.y)
            }
            e.isFromSource(InputDevice.SOURCE_TOUCHPAD) -> {
                // A captured trackpad reports finger positions: its motion is
                // the first finger's travel.
                when (e.actionMasked) {
                    MotionEvent.ACTION_DOWN -> { capLastX = e.x; capLastY = e.y }
                    MotionEvent.ACTION_MOVE -> {
                        if (!capLastX.isNaN()) move((e.x - capLastX) * 1.5f, (e.y - capLastY) * 1.5f)
                        capLastX = e.x
                        capLastY = e.y
                    }
                    MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> { capLastX = Float.NaN; capLastY = Float.NaN }
                }
            }
        }
        syncButtons(e.buttonState)
        return true
    }

    private fun move(dx: Float, dy: Float) {
        capRestX += dx
        capRestY += dy
        val x = capRestX.toInt()
        val y = capRestY.toInt()
        if (x != 0 || y != 0) {
            Native.mouseRel(x, y)
            capRestX -= x
            capRestY -= y
        }
    }

    // ---- Fingers on the screen ----

    /** Touch-screen events. True if handled. */
    fun touch(e: MotionEvent): Boolean {
        if (isPointerDevice(e)) return false
        return if (hostTouch && fingersAsTouch) windowsTouch(e) else fingerMouse(e)
    }

    /** Each finger to the PC as itself. */
    private fun windowsTouch(e: MotionEvent): Boolean {
        val i = e.actionIndex
        // A palm Android rejected after the fact: lifted, not a tap.
        val canceled = Build.VERSION.SDK_INT >= 33 && e.flags and MotionEvent.FLAG_CANCELED != 0
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN ->
                Native.touch(e.getPointerId(i), 0, nx(e.getX(i)), ny(e.getY(i)))
            MotionEvent.ACTION_MOVE -> for (p in 0 until e.pointerCount) Native.touch(e.getPointerId(p), 1, nx(e.getX(p)), ny(e.getY(p)))
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP ->
                Native.touch(e.getPointerId(i), if (canceled) 3 else 2, nx(e.getX(i)), ny(e.getY(i)))
            MotionEvent.ACTION_CANCEL -> for (p in 0 until e.pointerCount) Native.touch(e.getPointerId(p), 3, nx(e.getX(p)), ny(e.getY(p)))
        }
        return true
    }

    // Fingers as a mouse (a PC without touch, or the user's choice).
    private val slop = ViewConfiguration.getTouchSlop() * 1.5f
    private val holdMs = ViewConfiguration.getLongPressTimeout().toLong()
    private var downAt = 0L
    private var downX = 0f
    private var downY = 0f
    private var dragging = false
    private var held = false
    private var two = false
    private var twoMoved = false
    private var twoLastY = 0f
    private var twoLastX = 0f

    /** Called often (the status poll): a finger held still long enough is a right click. */
    fun tick() {
        if (downAt != 0L && !dragging && !held && !two && SystemClock.uptimeMillis() - downAt >= holdMs) {
            held = true
            Native.button(1, true)
            Native.button(1, false)
        }
    }

    private fun fingerMouse(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downAt = SystemClock.uptimeMillis()
                downX = e.x
                downY = e.y
                dragging = false
                held = false
                two = false
                twoMoved = false
                Native.mouseAbs(nx(e.x), ny(e.y))
            }
            MotionEvent.ACTION_POINTER_DOWN -> if (!dragging && !held && e.pointerCount == 2) {
                two = true
                twoLastX = (e.getX(0) + e.getX(1)) / 2
                twoLastY = (e.getY(0) + e.getY(1)) / 2
            }
            MotionEvent.ACTION_MOVE -> when {
                two && e.pointerCount >= 2 -> {
                    val cx = (e.getX(0) + e.getX(1)) / 2
                    val cy = (e.getY(0) + e.getY(1)) / 2
                    if (hypot(cx - twoLastX, cy - twoLastY) > 1f) twoMoved = true
                    val notch = 48f * density
                    wheel((cy - twoLastY) / notch, -(cx - twoLastX) / notch)
                    twoLastX = cx
                    twoLastY = cy
                }
                !two && !held -> {
                    if (!dragging && hypot(e.x - downX, e.y - downY) > slop) {
                        dragging = true
                        Native.button(0, true)
                    }
                    if (dragging) Native.mouseAbs(nx(e.x), ny(e.y))
                }
            }
            MotionEvent.ACTION_UP -> {
                when {
                    two -> if (!twoMoved && SystemClock.uptimeMillis() - downAt < 400) {
                        Native.button(1, true)
                        Native.button(1, false)
                    }
                    dragging -> Native.button(0, false)
                    !held -> {
                        Native.button(0, true)
                        Native.button(0, false)
                    }
                }
                downAt = 0L
                dragging = false
            }
            MotionEvent.ACTION_CANCEL -> {
                if (dragging) Native.button(0, false)
                downAt = 0L
                dragging = false
            }
        }
        return true
    }

    /** Everything up (focus lost). */
    fun reset() {
        buttons = 0
        if (pinchCtrl) {
            Native.key(29, 113, false)
            pinchCtrl = false
        }
        downAt = 0L
        dragging = false
        Native.releaseAll()
    }

    private companion object {
        /** MotionEvent's three-or-four-finger trackpad swipe (API 34; not in the public SDK). */
        const val CLASSIFICATION_MULTI_FINGER_SWIPE = 4
        val BUTTONS = listOf(
            MotionEvent.BUTTON_PRIMARY to 0,
            MotionEvent.BUTTON_SECONDARY to 1,
            MotionEvent.BUTTON_TERTIARY to 2,
            MotionEvent.BUTTON_BACK to 3,
            MotionEvent.BUTTON_FORWARD to 4,
            MotionEvent.BUTTON_STYLUS_PRIMARY to 0,
        )
    }
}
