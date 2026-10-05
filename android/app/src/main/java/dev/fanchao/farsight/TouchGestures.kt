package dev.fanchao.farsight

import android.os.Handler
import android.os.Looper
import android.view.MotionEvent
import android.view.VelocityTracker
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.min

/** How touches drive the pointer (design §7). */
enum class InputMode {
    /** The finger moves the cursor relatively, as on a laptop's trackpad. */
    TOUCHPAD,

    /** The pointer jumps to the finger. */
    DIRECT,
}

/**
 * Turns touches into pointer actions and viewport moves (design §7).
 *
 * Touchpad mode: a finger moves the cursor; tap clicks, two-finger tap
 * right-clicks, three-finger tap middle-clicks; touch and hold, then move,
 * drags; two fingers scroll.
 *
 * Direct mode: tap clicks where the finger is, touch and hold right-clicks
 * there, and a moving finger drags; two fingers pan the zoomed desktop, or
 * scroll when there is nothing to pan.
 *
 * Both: pinch zooms the view, and a two-finger double tap switches between
 * fit and 1:1. Panning carries on with inertia.
 */
class TouchGestures(private val slop: Float, private val actions: Actions) {
    interface Actions {
        val mode: InputMode
        val canPan: Boolean

        /** Touchpad: moves the cursor by this much, in view pixels. */
        fun moveCursorBy(dx: Float, dy: Float)

        /** Direct: the pointer goes to this view point. */
        fun pointAt(x: Float, y: Float)

        fun button(code: Int, pressed: Boolean)

        /** Scrolls by a finger's movement, in view pixels. */
        fun scroll(dx: Float, dy: Float)

        fun zoom(factor: Float, focusX: Float, focusY: Float)
        fun pan(dx: Float, dy: Float)
        fun fling(velocityX: Float, velocityY: Float)
        fun toggleZoom(focusX: Float, focusY: Float)

        /** A touch and hold registered. */
        fun feedback()
    }

    private enum class Two { UNDECIDED, PINCH, SCROLL, PAN }

    private val handler = Handler(Looper.getMainLooper())
    private var velocity: VelocityTracker? = null

    private var downTime = 0L
    private var downX = 0f
    private var downY = 0f
    private var lastX = 0f
    private var lastY = 0f
    private var lastMoveTime = 0L
    private var maxFingers = 0
    private var moved = false
    private var held = false
    private var dragging = false
    private var two = Two.UNDECIDED
    private var lastSpan = 0f
    private var startSpan = 0f
    private var lastFocusX = 0f
    private var lastFocusY = 0f
    private var lastTwoTap = 0L
    private val pendingRightClick = Runnable { click(Evdev.BTN_RIGHT) }

    private val longPress = Runnable {
        if (moved || maxFingers != 1) return@Runnable
        held = true
        actions.feedback()
        when (actions.mode) {
            InputMode.TOUCHPAD -> actions.button(Evdev.BTN_LEFT, true)
            InputMode.DIRECT -> {
                actions.pointAt(downX, downY)
                click(Evdev.BTN_RIGHT)
            }
        }
    }

    private fun click(code: Int) {
        actions.button(code, true)
        actions.button(code, false)
    }

    fun onTouchEvent(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                velocity?.recycle()
                velocity = VelocityTracker.obtain()
                downTime = e.eventTime
                downX = e.x
                downY = e.y
                lastX = e.x
                lastY = e.y
                lastMoveTime = e.eventTime
                maxFingers = 1
                moved = false
                held = false
                dragging = false
                two = Two.UNDECIDED
                handler.postDelayed(longPress, LONG_PRESS_MS)
            }
            MotionEvent.ACTION_POINTER_DOWN -> {
                handler.removeCallbacks(longPress)
                maxFingers = maxOf(maxFingers, e.pointerCount)
                if (e.pointerCount == 2) {
                    startSpan = span(e)
                    lastSpan = startSpan
                    lastFocusX = focusX(e)
                    lastFocusY = focusY(e)
                }
            }
            MotionEvent.ACTION_MOVE -> move(e)
            MotionEvent.ACTION_POINTER_UP -> {
                // The fingers left behind would jump: nothing moves until all are up.
                if (e.pointerCount == 3) {
                    lastFocusX = focusX(e, skip = e.actionIndex)
                    lastFocusY = focusY(e, skip = e.actionIndex)
                }
            }
            MotionEvent.ACTION_UP -> up(e)
            MotionEvent.ACTION_CANCEL -> {
                handler.removeCallbacks(longPress)
                if (held && actions.mode == InputMode.TOUCHPAD || dragging) actions.button(Evdev.BTN_LEFT, false)
            }
        }
        return true
    }

    private fun move(e: MotionEvent) {
        if (maxFingers == 1) {
            if (!moved && hypot(e.x - downX, e.y - downY) > slop) {
                moved = true
                handler.removeCallbacks(longPress)
                if (actions.mode == InputMode.DIRECT && !held) {
                    actions.pointAt(downX, downY)
                    actions.button(Evdev.BTN_LEFT, true)
                    dragging = true
                }
            }
            if (moved) {
                when (actions.mode) {
                    InputMode.TOUCHPAD -> {
                        val dt = (e.eventTime - lastMoveTime).coerceAtLeast(1)
                        val dx = e.x - lastX
                        val dy = e.y - lastY
                        // Faster moves go further, as a trackpad's do.
                        val speed = hypot(dx, dy) / dt
                        val gain = 1f + min(speed, 3f) * 0.6f
                        actions.moveCursorBy(dx * gain, dy * gain)
                    }
                    InputMode.DIRECT -> if (!held) actions.pointAt(e.x, e.y)
                }
            }
            lastX = e.x
            lastY = e.y
            lastMoveTime = e.eventTime
            return
        }
        if (e.pointerCount != 2 || maxFingers != 2) return
        velocity?.addMovement(e)
        val span = span(e)
        val fx = focusX(e)
        val fy = focusY(e)
        if (two == Two.UNDECIDED) {
            val spread = abs(span - startSpan)
            val travel = hypot(fx - lastFocusX, fy - lastFocusY)
            if (spread > slop * 2) {
                two = Two.PINCH
            } else if (travel > slop) {
                two = when {
                    actions.mode == InputMode.DIRECT && actions.canPan -> Two.PAN
                    else -> Two.SCROLL
                }
            } else {
                return
            }
            moved = true
        }
        when (two) {
            Two.PINCH -> {
                if (lastSpan > 0f) actions.zoom(span / lastSpan, fx, fy)
                actions.pan(fx - lastFocusX, fy - lastFocusY)
            }
            Two.PAN -> actions.pan(fx - lastFocusX, fy - lastFocusY)
            Two.SCROLL -> actions.scroll(fx - lastFocusX, fy - lastFocusY)
            Two.UNDECIDED -> {}
        }
        lastSpan = span
        lastFocusX = fx
        lastFocusY = fy
    }

    private fun up(e: MotionEvent) {
        handler.removeCallbacks(longPress)
        val quick = e.eventTime - downTime < TAP_MS
        when {
            maxFingers == 1 && held -> if (actions.mode == InputMode.TOUCHPAD) actions.button(Evdev.BTN_LEFT, false)
            maxFingers == 1 && dragging -> {
                actions.pointAt(e.x, e.y)
                actions.button(Evdev.BTN_LEFT, false)
            }
            maxFingers == 1 && !moved && quick -> {
                if (actions.mode == InputMode.DIRECT) actions.pointAt(e.x, e.y)
                click(Evdev.BTN_LEFT)
            }
            maxFingers == 2 && !moved && quick -> {
                // A second tap soon after makes a double tap, which zooms;
                // a single one right-clicks.
                if (e.eventTime - lastTwoTap < DOUBLE_TAP_MS) {
                    handler.removeCallbacks(pendingRightClick)
                    lastTwoTap = 0
                    actions.toggleZoom(lastFocusX, lastFocusY)
                } else {
                    lastTwoTap = e.eventTime
                    handler.postDelayed(pendingRightClick, DOUBLE_TAP_MS)
                }
            }
            maxFingers == 3 && !moved && quick -> click(Evdev.BTN_MIDDLE)
            maxFingers == 2 && two == Two.PAN -> {
                velocity?.let {
                    it.computeCurrentVelocity(1000)
                    actions.fling(it.xVelocity, it.yVelocity)
                }
            }
        }
        velocity?.recycle()
        velocity = null
    }

    private fun span(e: MotionEvent) = hypot(e.getX(0) - e.getX(1), e.getY(0) - e.getY(1))

    private fun focusX(e: MotionEvent, skip: Int = -1): Float {
        var sum = 0f
        var n = 0
        for (i in 0 until e.pointerCount) if (i != skip) { sum += e.getX(i); n++ }
        return sum / n
    }

    private fun focusY(e: MotionEvent, skip: Int = -1): Float {
        var sum = 0f
        var n = 0
        for (i in 0 until e.pointerCount) if (i != skip) { sum += e.getY(i); n++ }
        return sum / n
    }

    companion object {
        const val LONG_PRESS_MS = 450L
        const val TAP_MS = 250L
        const val DOUBLE_TAP_MS = 300L
    }
}
