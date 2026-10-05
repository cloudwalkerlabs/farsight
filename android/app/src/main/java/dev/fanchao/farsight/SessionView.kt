package dev.fanchao.farsight

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Matrix
import android.graphics.Paint
import android.graphics.Path
import android.os.Handler
import android.os.Looper
import android.view.HapticFeedbackConstants
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.PixelCopy
import android.view.PointerIcon
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewConfiguration
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import uniffi.farsight_android.RemoteCursor
import uniffi.farsight_android.ScreenLayout
import kotlin.math.abs
import kotlin.math.exp

/**
 * The remote desktop: a [SurfaceView] that the Rust side decodes into,
 * placed and scaled by the [Viewport] (so zooming costs nothing: it is the
 * compositor scaling the last frame), with the cursor drawn over it here
 * (design §4, §7). Touches go through [TouchGestures]; a mouse and a
 * hardware keyboard go to the session directly; the soft keyboard talks to
 * [RemoteInputConnection].
 */
@SuppressLint("ViewConstructor")
class SessionView(context: Context, private val controller: SessionController) :
    FrameLayout(context), TouchGestures.Actions {
    val viewport = Viewport()
    private val surface = SurfaceView(context)
    private val overlay = CursorOverlay(context)
    private val gestures = TouchGestures(ViewConfiguration.get(context).scaledTouchSlop.toFloat(), this)
    private val density = resources.displayMetrics.density
    private var keyboardHeight = 0

    /** The screen's size changed: the activity works out the layout to ask for. */
    var onSize: ((Int, Int) -> Unit)? = null

    init {
        setBackgroundColor(Color.BLACK)
        addView(surface, LayoutParams(1, 1))
        addView(overlay, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        surface.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) = NativeSurface.set(holder.surface)
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) =
                NativeSurface.set(holder.surface)

            override fun surfaceDestroyed(holder: SurfaceHolder) = NativeSurface.set(null)
        })
        isFocusable = true
        isFocusableInTouchMode = true
        // The session's cursor is drawn here, not the system's.
        pointerIcon = PointerIcon.getSystemIcon(context, PointerIcon.TYPE_NULL)
        ViewCompat.setOnApplyWindowInsetsListener(this) { _, insets ->
            keyboardHeight = insets.getInsets(WindowInsetsCompat.Type.ime()).bottom
            updateViewport()
            insets
        }
        controller.onRemote = { setRemote(it) }
        controller.onCursor = {
            if (controller.inputMode == InputMode.TOUCHPAD) follow()
            overlay.invalidate()
        }
        controller.remote?.let { setRemote(it) }
    }

    private fun setRemote(layout: ScreenLayout) {
        val w = layout.widthPx.toInt()
        val h = layout.heightPx.toInt()
        if (surface.layoutParams.width != w || surface.layoutParams.height != h) {
            surface.layoutParams = LayoutParams(w, h)
        }
        viewport.setRemote(w.toFloat(), h.toFloat())
        apply()
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        updateViewport()
        onSize?.invoke(w, h)
    }

    private fun updateViewport() {
        viewport.setView(width.toFloat(), height.toFloat(), (height - keyboardHeight).toFloat())
        // A keyboard that just opened mustn't hide the cursor.
        if (keyboardHeight > 0) follow()
        apply()
    }

    /** Puts the surface where the viewport says. */
    private fun apply() {
        surface.pivotX = 0f
        surface.pivotY = 0f
        surface.scaleX = viewport.zoom
        surface.scaleY = viewport.zoom
        surface.translationX = viewport.offsetX
        surface.translationY = viewport.offsetY
        overlay.invalidate()
    }

    private fun follow() {
        viewport.follow(controller.cursorX, controller.cursorY, 48 * density)
        apply()
    }

    fun fit() {
        viewport.fit()
        apply()
    }

    fun toggleZoom() = toggleZoom(width / 2f, height / 2f)

    // --- Touch ---

    override val mode: InputMode get() = controller.inputMode
    override val canPan: Boolean get() = viewport.canPan()

    override fun moveCursorBy(dx: Float, dy: Float) {
        stopFling()
        // In remote pixels: the same finger travel moves the cursor as far
        // on the screen whatever the zoom.
        controller.moveCursorBy(dx / viewport.zoom, dy / viewport.zoom)
    }

    override fun pointAt(x: Float, y: Float) = controller.pointAt(viewport.toRemoteX(x), viewport.toRemoteY(y))

    override fun button(code: Int, pressed: Boolean) = controller.button(code, pressed)

    override fun scroll(dx: Float, dy: Float) {
        // Content follows the fingers, as on a touch screen.
        controller.scroll(-dx / viewport.zoom, -dy / viewport.zoom)
    }

    override fun zoom(factor: Float, focusX: Float, focusY: Float) {
        stopFling()
        viewport.zoomBy(factor, focusX, focusY)
        apply()
    }

    override fun pan(dx: Float, dy: Float) {
        viewport.panBy(dx, dy)
        apply()
    }

    override fun toggleZoom(focusX: Float, focusY: Float) {
        viewport.toggle(focusX, focusY)
        apply()
    }

    override fun feedback() {
        performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
    }

    private var flingVx = 0f
    private var flingVy = 0f
    private var flingLast = 0L
    private val flingStep = object : Runnable {
        override fun run() {
            val now = System.nanoTime()
            val dt = (now - flingLast) / 1e9f
            flingLast = now
            pan(flingVx * dt, flingVy * dt)
            // Friction: a fling dies down over about half a second.
            val decay = exp(-dt / 0.15f)
            flingVx *= decay
            flingVy *= decay
            if (abs(flingVx) + abs(flingVy) > 20f) postOnAnimation(this)
        }
    }

    override fun fling(velocityX: Float, velocityY: Float) {
        flingVx = velocityX
        flingVy = velocityY
        flingLast = System.nanoTime()
        postOnAnimation(flingStep)
    }

    private fun stopFling() {
        removeCallbacks(flingStep)
        flingVx = 0f
        flingVy = 0f
    }

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (controller.server.viewOnly) {
            // Watching: fingers only zoom and pan.
            return gestures.onTouchEvent(event)
        }
        // A mouse with a button down comes as touches: it just points.
        if (event.isFromSource(InputDevice.SOURCE_MOUSE)) {
            pointAt(event.x, event.y)
            return true
        }
        if (event.actionMasked == MotionEvent.ACTION_DOWN) {
            requestFocus()
            stopFling()
        }
        return gestures.onTouchEvent(event)
    }

    /** A mouse goes straight to the session (design §7). */
    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (!event.isFromSource(InputDevice.SOURCE_MOUSE)) return super.onGenericMotionEvent(event)
        when (event.actionMasked) {
            MotionEvent.ACTION_HOVER_MOVE -> pointAt(event.x, event.y)
            MotionEvent.ACTION_BUTTON_PRESS, MotionEvent.ACTION_BUTTON_RELEASE -> {
                val code = when (event.actionButton) {
                    MotionEvent.BUTTON_PRIMARY -> Evdev.BTN_LEFT
                    MotionEvent.BUTTON_SECONDARY -> Evdev.BTN_RIGHT
                    MotionEvent.BUTTON_TERTIARY -> Evdev.BTN_MIDDLE
                    MotionEvent.BUTTON_BACK -> Evdev.BTN_SIDE
                    MotionEvent.BUTTON_FORWARD -> Evdev.BTN_EXTRA
                    else -> return true
                }
                pointAt(event.x, event.y)
                controller.button(code, event.actionMasked == MotionEvent.ACTION_BUTTON_PRESS)
            }
            MotionEvent.ACTION_SCROLL -> {
                // Android's axes point up and left; Wayland's down and right.
                // One click is 15 px, as in libinput.
                val v = -event.getAxisValue(MotionEvent.AXIS_VSCROLL)
                val h = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
                controller.scroll(h * 15, v * 15, (h * 120).toInt(), (v * 120).toInt())
            }
            else -> return false
        }
        return true
    }

    // --- Keys ---

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        // Navigation and volume stay the device's.
        when (event.keyCode) {
            KeyEvent.KEYCODE_BACK, KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.KEYCODE_VOLUME_DOWN,
            KeyEvent.KEYCODE_VOLUME_MUTE, KeyEvent.KEYCODE_HOME, KeyEvent.KEYCODE_APP_SWITCH,
            -> return super.dispatchKeyEvent(event)
        }
        if (controller.server.viewOnly) return super.dispatchKeyEvent(event)
        return controller.keyEvent(event) || super.dispatchKeyEvent(event)
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = EditorInfo.TYPE_CLASS_TEXT or EditorInfo.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_ACTION_NONE
        return RemoteInputConnection(this, controller)
    }

    fun showKeyboard(show: Boolean) {
        val imm = context.getSystemService(InputMethodManager::class.java)
        if (show) {
            requestFocus()
            imm.showSoftInput(this, 0)
        } else {
            imm.hideSoftInputFromWindow(windowToken, 0)
        }
    }

    /** A small copy of the picture, for the address book. */
    fun thumbnail(done: (Bitmap?) -> Unit) {
        val layout = controller.remote
        if (layout == null || !surface.holder.surface.isValid) return done(null)
        val w = 480
        val h = (w * layout.heightPx.toInt() / layout.widthPx.toInt()).coerceAtLeast(1)
        val bitmap = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
        try {
            PixelCopy.request(surface, bitmap, { result ->
                done(bitmap.takeIf { result == PixelCopy.SUCCESS })
            }, Handler(Looper.getMainLooper()))
        } catch (_: IllegalArgumentException) {
            done(null)
        }
    }

    /** The session's cursor, drawn where the pointer is (design §4). */
    private inner class CursorOverlay(context: Context) : View(context) {
        private val matrix = Matrix()
        private val paint = Paint(Paint.ANTI_ALIAS_FLAG or Paint.FILTER_BITMAP_FLAG)
        private val arrow = Path().apply {
            moveTo(0f, 0f); lineTo(0f, 17f); lineTo(4.5f, 13f); lineTo(7.5f, 20f)
            lineTo(10f, 19f); lineTo(7f, 12f); lineTo(12.5f, 12f); close()
        }
        private val beam = Path().apply {
            moveTo(-3f, -9f); lineTo(3f, -9f); moveTo(0f, -9f); lineTo(0f, 9f); moveTo(-3f, 9f); lineTo(3f, 9f)
        }

        override fun onDraw(canvas: Canvas) {
            if (controller.remote == null) return
            val x = viewport.toViewX(controller.cursorX)
            val y = viewport.toViewY(controller.cursorY)
            when (val c = controller.cursor) {
                is RemoteCursor.Hidden -> {}
                is RemoteCursor.Image -> {
                    val image = controller.cursorImages[c.id] ?: return drawNamed(canvas, "default", x, y)
                    // The image is in its own density; draw it at its size on
                    // the remote output, then as the viewport zooms.
                    val s = 120f / image.scale120 * viewport.zoom
                    matrix.setScale(s, s)
                    matrix.postTranslate(x - image.hotspotX * s, y - image.hotspotY * s)
                    canvas.drawBitmap(image.bitmap, matrix, paint)
                }
                is RemoteCursor.Named -> drawNamed(canvas, c.name, x, y)
            }
        }

        private fun drawNamed(canvas: Canvas, name: String, x: Float, y: Float) {
            canvas.save()
            canvas.translate(x, y)
            canvas.scale(density, density)
            val path = if (name == "text" || name == "vertical-text") beam else arrow
            paint.style = Paint.Style.STROKE
            paint.strokeWidth = if (path === beam) 3.5f else 2.5f
            paint.color = Color.BLACK
            canvas.drawPath(path, paint)
            paint.style = if (path === beam) Paint.Style.STROKE else Paint.Style.FILL
            paint.strokeWidth = 1.5f
            paint.color = Color.WHITE
            canvas.drawPath(path, paint)
            canvas.restore()
        }
    }
}

/**
 * The soft keyboard's connection: compositions and commits become text
 * or keys (see [SessionController]), and deletions become key presses.
 * Nothing is kept here: the text lives in the session.
 */
private class RemoteInputConnection(view: View, private val controller: SessionController) :
    BaseInputConnection(view, false) {
    override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
        controller.commit(text.toString())
        return true
    }

    override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
        controller.compose(text.toString())
        return true
    }

    override fun finishComposingText(): Boolean {
        controller.finishComposing()
        return true
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        controller.delete(beforeLength, afterLength)
        return true
    }

    override fun sendKeyEvent(event: KeyEvent): Boolean {
        controller.keyEvent(event)
        return true
    }

    override fun performEditorAction(actionCode: Int): Boolean {
        controller.tap(Evdev.KEY_ENTER)
        return true
    }

    override fun getTextBeforeCursor(n: Int, flags: Int): CharSequence = ""
    override fun getTextAfterCursor(n: Int, flags: Int): CharSequence = ""
}
