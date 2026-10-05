package dev.fanchao.farsight

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import uniffi.farsight_android.ScreenLayout
import uniffi.farsight_android.clientKeyLine
import uniffi.farsight_android.forgetServer
import uniffi.farsight_android.initLogging
import kotlin.math.roundToInt

/**
 * The only activity, never recreated: configuration changes (rotation, a
 * keyboard plugged in, a desktop window resized) are handled in place, so
 * the session keeps its surface (design §7). It shows the address book, or
 * one session full screen.
 *
 * A session lasts while the app is in the foreground: it is closed when
 * the app goes to the background, and resumed (at the server it never
 * ended) when the app comes back.
 *
 * For tests, `adb shell am start -n dev.fanchao.farsight/.MainActivity
 * --es address HOST[:PORT]` connects to that address at once, adding it to
 * the address book if it is new.
 */
class MainActivity : ComponentActivity() {
    private lateinit var servers: Servers
    private var list by mutableStateOf<List<Server>>(emptyList())
    private var controller by mutableStateOf<SessionController?>(null)
    private var sessionView: SessionView? = null
    private var layout: ScreenLayout? = null
    private var stopped = false
    private var pendingMic: ((Boolean) -> Unit)? = null

    private val micPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        pendingMic?.invoke(granted)
        pendingMic = null
    }

    private val back = object : OnBackPressedCallback(false) {
        override fun handleOnBackPressed() = leave()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        initLogging()
        servers = Servers(this)
        list = servers.load()
        enableEdgeToEdge()
        onBackPressedDispatcher.addCallback(this, back)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                val c = controller
                val v = sessionView
                if (c != null && v != null) {
                    // A new session brings a new view, which AndroidView
                    // only takes when it is created.
                    key(v) {
                        SessionScreen(
                        c, v,
                        onLeave = ::leave,
                        onRetry = { layout?.let { c.start(it) } },
                        onForget = {
                            forgetServer(servers.configDir, c.server.address)
                            layout?.let { c.start(it) }
                        },
                    )
                    }
                } else {
                    ServerListScreen(
                        list,
                        thumbnail = servers::thumbnail,
                        keyLine = { clientKeyLine(servers.configDir, "android@${Build.MODEL.replace(' ', '-')}") },
                        onConnect = ::connect,
                        onSave = { list = servers.put(it) },
                        onDelete = { list = servers.remove(it) },
                    )
                }
            }
        }
        handleIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    private fun handleIntent(intent: Intent) {
        val address = intent.getStringExtra("address") ?: return
        val existing = list.firstOrNull { it.address == address }
        val server = existing ?: Server(
            address = address,
            plain = intent.getBooleanExtra("plain", false),
            inputMode = if (intent.getStringExtra("mode") == "direct") InputMode.DIRECT else InputMode.TOUCHPAD,
            mic = runCatching { MicPolicy.valueOf(intent.getStringExtra("mic") ?: "") }.getOrDefault(MicPolicy.ASK),
            scalePercent = intent.getIntExtra("scale", 0),
        ).also { list = servers.put(it) }
        if (controller?.server?.id != server.id) {
            leave()
            connect(server)
        }
    }

    private fun connect(server: Server) {
        val c = SessionController(this, server, servers.configDir) {
            list = servers.put(server.copy(lastUsed = System.currentTimeMillis()))
        }
        c.requestMicPermission = { done ->
            if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
                done(true)
            } else {
                pendingMic = done
                micPermission.launch(Manifest.permission.RECORD_AUDIO)
            }
        }
        val view = SessionView(this, c)
        view.onSize = { w, h ->
            val l = layoutFor(server, w, h)
            layout = l
            if (c.running) c.setLayout(l) else if (!stopped) c.start(l)
        }
        sessionView = view
        controller = c
        back.isEnabled = true
        immersive(true)
    }

    /** What to ask the server for: the screen, at the device's density or the chosen scale (design §5). */
    private fun layoutFor(server: Server, width: Int, height: Int): ScreenLayout {
        val density = resources.displayMetrics.density
        val scale = if (server.scalePercent == 0) (density * 120).roundToInt() else server.scalePercent * 120 / 100
        val (w, h) = server.fixedSize() ?: (width to height)
        val refresh = ((display?.refreshRate ?: 60f) * 1000).roundToInt()
        return ScreenLayout(w.toUInt(), h.toUInt(), scale.toUInt(), refresh.toUInt())
    }

    private fun leave() {
        val c = controller ?: return
        val view = sessionView
        controller = null
        sessionView = null
        layout = null
        back.isEnabled = false
        immersive(false)
        val stop = {
            c.stop()
            list = servers.load()
        }
        if (view != null && c.connection is Connection.Connected) {
            view.thumbnail { bitmap ->
                bitmap?.let { servers.saveThumbnail(c.server, it) }
                stop()
            }
        } else {
            stop()
        }
    }

    private fun immersive(on: Boolean) {
        val insets = WindowCompat.getInsetsController(window, window.decorView)
        if (on) {
            insets.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            insets.hide(WindowInsetsCompat.Type.systemBars())
            window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            insets.show(WindowInsetsCompat.Type.systemBars())
            window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }
    }

    override fun onStart() {
        super.onStart()
        val c = controller ?: return
        if (stopped) {
            stopped = false
            layout?.let { c.start(it) }
        }
    }

    override fun onStop() {
        super.onStop()
        val c = controller ?: return
        stopped = true
        sessionView?.thumbnail { bitmap -> bitmap?.let { servers.saveThumbnail(c.server, it) } }
        c.stop()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        // Android lets the focused app read the clipboard; this is when it
        // may have changed.
        if (hasFocus) controller?.offerClipboard()
    }
}
