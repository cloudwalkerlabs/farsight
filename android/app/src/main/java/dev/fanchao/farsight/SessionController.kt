package dev.fanchao.farsight

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Bitmap
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.KeyEvent
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import uniffi.farsight_android.CursorBitmap
import uniffi.farsight_android.RemoteCursor
import uniffi.farsight_android.ScreenLayout
import uniffi.farsight_android.Session
import uniffi.farsight_android.SessionListener
import uniffi.farsight_android.SessionOptions
import uniffi.farsight_android.SessionStats
import java.nio.ByteBuffer

/** Where the connection stands. */
sealed interface Connection {
    data object Connecting : Connection
    data class Connected(val fingerprint: String) : Connection
    data class Reconnecting(val attempt: Int, val reason: String) : Connection

    /** Over; `refused` if trying again won't help until something changes. */
    data class Closed(val reason: String, val refused: Boolean) : Connection
}

/** A sticky modifier on the extra keys bar: for the next key, or until unlocked. */
enum class Latch { ONCE, LOCKED }

/** A cursor image from the server, ready to draw. */
class CursorImage(val bitmap: Bitmap, val hotspotX: Int, val hotspotY: Int, val scale120: Int)

/**
 * One session with a server, for the UI: the Rust [Session] underneath,
 * its events turned into Compose state on the main thread, and what the
 * user does turned into input (design §4, §7).
 *
 * Typing: while a text field in the session has focus (it said so), the
 * soft keyboard's text and compositions go through the session's input
 * method as text; otherwise, as with terminals and X apps, characters are
 * typed as keys on a US keymap, and a composition is retyped as it
 * changes. A latched modifier always turns typing into keys.
 */
class SessionController(
    private val context: Context,
    val server: Server,
    private val configDir: String,
    private val onConnected: () -> Unit,
) : SessionListener {
    private val main = Handler(Looper.getMainLooper())
    private var session: Session? = null

    var connection by mutableStateOf<Connection>(Connection.Connecting)
        private set
    /** The server's layout for the current picture. */
    var remote by mutableStateOf<ScreenLayout?>(null)
        private set
    var textInput by mutableStateOf(false)
        private set
    var stats by mutableStateOf<SessionStats?>(null)
        private set
    var micAsked by mutableStateOf(false)
        private set
    var micOn by mutableStateOf(false)
        private set
    var inputMode by mutableStateOf(server.inputMode)
    val modifiers = mutableStateMapOf<Int, Latch>()

    /** The pointer, in remote pixels. */
    var cursorX = 0f
        private set
    var cursorY = 0f
        private set
    var cursor: RemoteCursor = RemoteCursor.Named("default")
        private set
    val cursorImages = HashMap<ULong, CursorImage>()

    /** Called on the main thread when the picture's size or the cursor changes. */
    var onRemote: ((ScreenLayout) -> Unit)? = null
    var onCursor: (() -> Unit)? = null

    /** Asks for RECORD_AUDIO; the callback says whether it was granted. */
    var requestMicPermission: ((Boolean) -> Unit) -> Unit = { it(false) }

    private var layout: ScreenLayout? = null
    private var composing = ""
    private var lastClip: String? = null
    private var micDemand = false

    val running: Boolean get() = session != null

    /** Connects with the screen at this layout. */
    fun start(layout: ScreenLayout) {
        stop()
        this.layout = layout
        connection = Connection.Connecting
        val options = SessionOptions(
            configDir = configDir,
            address = server.address,
            plain = server.plain,
            layout = layout,
            decoders = Decoders.available(),
            motion = server.motion,
            audio = server.audio,
            mic = server.mic != MicPolicy.NEVER,
            viewOnly = server.viewOnly,
        )
        session = try {
            Session(options, this)
        } catch (e: Exception) {
            connection = Connection.Closed(e.message ?: e.toString(), false)
            null
        }
    }

    /** Ends the session here; it lives on at the server. */
    fun stop() {
        stopMic()
        session?.let {
            it.disconnect()
            it.destroy()
        }
        session = null
        micDemand = false
        micAsked = false
    }

    fun setLayout(layout: ScreenLayout) {
        if (layout == this.layout) return
        this.layout = layout
        session?.setLayout(layout)
    }

    // --- Pointer ---

    fun pointAt(x: Float, y: Float) {
        val r = remote ?: return
        cursorX = x.coerceIn(0f, r.widthPx.toFloat() - 1)
        cursorY = y.coerceIn(0f, r.heightPx.toFloat() - 1)
        session?.pointer(cursorX, cursorY)
        onCursor?.invoke()
    }

    fun moveCursorBy(dx: Float, dy: Float) = pointAt(cursorX + dx, cursorY + dy)

    fun button(code: Int, pressed: Boolean) {
        session?.button(code.toUInt(), pressed)
    }

    /** Scrolls by this much, in remote pixels, down and right positive. */
    fun scroll(dx: Float, dy: Float, v120X: Int = 0, v120Y: Int = 0) {
        session?.scroll(dx, dy, v120X, v120Y)
    }

    // --- Keys and text ---

    /** A key from a keyboard, physical or soft: true if it went to the session. */
    fun keyEvent(event: KeyEvent): Boolean {
        val code = Evdev.fromKeyEvent(event) ?: return false
        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                // The session's apps repeat keys themselves.
                if (event.repeatCount > 0) return true
                pressModifiers()
                session?.key(code.toUInt(), true)
            }
            KeyEvent.ACTION_UP -> {
                session?.key(code.toUInt(), false)
                releaseModifiers()
            }
        }
        return true
    }

    /** Presses and releases a key, with the latched modifiers and shift if asked. */
    fun tap(code: Int, shift: Boolean = false) {
        val s = session ?: return
        pressModifiers()
        if (shift) s.key(Evdev.KEY_LEFTSHIFT.toUInt(), true)
        s.key(code.toUInt(), true)
        s.key(code.toUInt(), false)
        if (shift) s.key(Evdev.KEY_LEFTSHIFT.toUInt(), false)
        releaseModifiers()
    }

    private fun pressModifiers() {
        for (m in modifiers.keys) session?.key(m.toUInt(), true)
    }

    /** Releases the latched modifiers, and forgets the ones for one key. */
    private fun releaseModifiers() {
        for (m in modifiers.keys) session?.key(m.toUInt(), false)
        modifiers.entries.removeAll { it.value == Latch.ONCE }
    }

    /** Tap: for the next key; tap again: locked; again: off. */
    fun toggleModifier(code: Int) {
        when (modifiers[code]) {
            null -> modifiers[code] = Latch.ONCE
            Latch.ONCE -> modifiers[code] = Latch.LOCKED
            Latch.LOCKED -> modifiers.remove(code)
        }
    }

    private val asText: Boolean get() = textInput && modifiers.isEmpty()

    /** Text the soft keyboard committed. */
    fun commit(text: String) {
        if (asText) {
            session?.commitText(text)
        } else {
            retype(text)
        }
        composing = ""
    }

    /** Text the soft keyboard is composing. */
    fun compose(text: String) {
        if (asText) {
            val end = text.toByteArray().size
            session?.preedit(text, end, end)
        } else {
            retype(text)
        }
        composing = text
    }

    fun finishComposing() {
        if (asText && composing.isNotEmpty()) session?.commitText(composing)
        composing = ""
    }

    /** Deletes around the cursor, as the soft keyboard asks. */
    fun delete(before: Int, after: Int) {
        repeat(before) { tap(Evdev.KEY_BACKSPACE) }
        repeat(after) { tap(Evdev.KEY_DELETE) }
    }

    /** Turns the composition so far into `text`, typing only what differs. */
    private fun retype(text: String) {
        val common = composing.commonPrefixWith(text).length
        repeat(composing.length - common) { tap(Evdev.KEY_BACKSPACE) }
        for (c in text.substring(common)) {
            val key = Evdev.forChar(c)
            if (key != null) tap(key.first, key.second) else session?.commitText(c.toString())
        }
    }

    // --- Clipboard ---

    /** Offers the device's clipboard to the session, if it changed: when the app gains focus. */
    fun offerClipboard() {
        val cm = context.getSystemService(ClipboardManager::class.java)
        val text = cm.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
        if (text.isNullOrEmpty() || text == lastClip) return
        lastClip = text
        session?.offerClipboard(text)
    }

    // --- Microphone ---

    /** The user answered the session's request for the microphone. */
    fun answerMic(allow: Boolean) {
        micAsked = false
        if (allow && micDemand) requestMicPermission { granted -> if (granted && micDemand) startMic() }
    }

    private fun startMic() {
        val audio = context.getSystemService(AudioManager::class.java)
        // The platform's echo canceller works in communication mode, which
        // plays through the earpiece unless told otherwise: a desktop on a
        // phone wants the speaker, if nothing is plugged in.
        audio.mode = AudioManager.MODE_IN_COMMUNICATION
        if (Build.VERSION.SDK_INT >= 31) {
            val devices = audio.availableCommunicationDevices
            val headset = devices.firstOrNull {
                it.type in setOf(
                    AudioDeviceInfo.TYPE_WIRED_HEADSET, AudioDeviceInfo.TYPE_USB_HEADSET,
                    AudioDeviceInfo.TYPE_BLUETOOTH_SCO, AudioDeviceInfo.TYPE_BLE_HEADSET,
                )
            }
            val target = headset ?: devices.firstOrNull { it.type == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER }
            target?.let { audio.setCommunicationDevice(it) }
        } else {
            @Suppress("DEPRECATION")
            audio.isSpeakerphoneOn = true
        }
        try {
            session?.setMic(true)
            micOn = true
        } catch (e: Exception) {
            Log.w("farsight", "microphone: $e")
            restoreAudio()
        }
    }

    fun stopMic() {
        if (!micOn) return
        try {
            session?.setMic(false)
        } catch (_: Exception) {
        }
        micOn = false
        restoreAudio()
    }

    private fun restoreAudio() {
        val audio = context.getSystemService(AudioManager::class.java)
        if (Build.VERSION.SDK_INT >= 31) audio.clearCommunicationDevice()
        else @Suppress("DEPRECATION") run { audio.isSpeakerphoneOn = false }
        audio.mode = AudioManager.MODE_NORMAL
    }

    // --- The session's events, on its threads ---

    private fun ui(block: () -> Unit) {
        main.post(block)
    }

    override fun connected(fingerprint: String) = ui {
        connection = Connection.Connected(fingerprint)
        onConnected()
    }

    override fun reconnecting(attempt: UInt, reason: String) = ui {
        connection = Connection.Reconnecting(attempt.toInt(), reason)
        stopMic()
    }

    override fun epoch(layout: ScreenLayout, encoding: String) = ui {
        val first = remote == null
        remote = layout
        if (first) {
            cursorX = layout.widthPx.toFloat() / 2
            cursorY = layout.heightPx.toFloat() / 2
        }
        onRemote?.invoke(layout)
    }

    override fun cursorImage(image: CursorBitmap) {
        val bitmap = Bitmap.createBitmap(image.width.toInt(), image.height.toInt(), Bitmap.Config.ARGB_8888)
        bitmap.copyPixelsFromBuffer(ByteBuffer.wrap(image.rgba))
        val cursor = CursorImage(bitmap, image.hotspotX, image.hotspotY, image.scale120.toInt())
        main.post { cursorImages[image.id] = cursor }
    }

    override fun cursor(cursor: RemoteCursor) = ui {
        this.cursor = cursor
        onCursor?.invoke()
    }

    override fun clipboard(text: String) = ui {
        lastClip = text
        context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("farsight", text))
    }

    override fun textInput(active: Boolean) = ui {
        textInput = active
    }

    override fun micDemand(on: Boolean) = ui {
        micDemand = on
        if (!on) {
            micAsked = false
            stopMic()
            return@ui
        }
        when (server.mic) {
            MicPolicy.NEVER -> {}
            MicPolicy.ASK -> micAsked = true
            MicPolicy.ALWAYS -> answerMic(true)
        }
    }

    override fun stats(stats: SessionStats) = ui {
        this.stats = stats
    }

    override fun closed(reason: String, refused: Boolean) = ui {
        connection = Connection.Closed(reason, refused)
        stopMic()
    }
}
