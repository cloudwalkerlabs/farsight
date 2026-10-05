package dev.fanchao.farsight

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectVerticalDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import kotlinx.coroutines.delay
import kotlin.math.roundToInt

/** How long the toolbar stays before it tucks itself away. */
private const val TOOLBAR_HIDE_MS = 4000L

/**
 * The session, full screen, with its chrome over it (design §7): a small
 * draggable toolbar that hides itself, the extra keys bar above the
 * keyboard, statistics, and what the connection is doing.
 */
@Composable
fun SessionScreen(
    controller: SessionController,
    view: SessionView,
    onLeave: () -> Unit,
    onRetry: () -> Unit,
    onForget: () -> Unit,
) {
    var keyboard by remember { mutableStateOf(false) }
    var extraKeys by remember { mutableStateOf(false) }
    var showStats by remember { mutableStateOf(false) }
    Box(Modifier.fillMaxSize().background(Color.Black)) {
        AndroidView(factory = { view }, modifier = Modifier.fillMaxSize())

        if (showStats) Stats(controller, Modifier.align(Alignment.TopStart).padding(8.dp))
        if (controller.micOn) {
            Row(
                Modifier.align(Alignment.TopCenter).padding(top = 8.dp)
                    .background(Color(0xCCB00020), RoundedCornerShape(12.dp)).padding(horizontal = 10.dp, vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(Modifier.size(8.dp).background(Color.White, CircleShape))
                Spacer(Modifier.size(6.dp))
                Text("Microphone on", color = Color.White, fontSize = 12.sp)
            }
        }

        Toolbar(
            controller = controller,
            modifier = Modifier.align(Alignment.TopEnd),
            onKeyboard = {
                keyboard = !keyboard
                view.showKeyboard(keyboard)
            },
            onExtraKeys = { extraKeys = !extraKeys },
            onZoom = { view.toggleZoom() },
            onStats = { showStats = !showStats },
            onLeave = onLeave,
        )

        if (extraKeys && !controller.server.viewOnly) {
            ExtraKeys(controller, Modifier.align(Alignment.BottomCenter).imePadding())
        }

        Status(controller, onLeave, onRetry, onForget)

        if (controller.micAsked) {
            AlertDialog(
                onDismissRequest = { controller.answerMic(false) },
                title = { Text("Microphone") },
                text = { Text("An app in the session wants to record. Send this device's microphone?") },
                confirmButton = { TextButton({ controller.answerMic(true) }) { Text("Allow") } },
                dismissButton = { TextButton({ controller.answerMic(false) }) { Text("Not now") } },
            )
        }
    }
}

@Composable
private fun Status(controller: SessionController, onLeave: () -> Unit, onRetry: () -> Unit, onForget: () -> Unit) {
    when (val c = controller.connection) {
        is Connection.Connected -> {}
        is Connection.Connecting -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                CircularProgressIndicator()
                Spacer(Modifier.height(12.dp))
                Text("Connecting to ${controller.server.title}…", color = Color.White)
            }
        }
        is Connection.Reconnecting -> Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.TopCenter) {
            Text(
                "Reconnecting… (${c.attempt})",
                color = Color.White,
                modifier = Modifier.padding(top = 36.dp).background(Color(0xCC000000), RoundedCornerShape(12.dp))
                    .padding(horizontal = 12.dp, vertical = 6.dp),
            )
        }
        is Connection.Closed -> Box(Modifier.fillMaxSize().background(Color(0xCC000000)), contentAlignment = Alignment.Center) {
            Surface(shape = RoundedCornerShape(16.dp), modifier = Modifier.padding(24.dp).widthIn(max = 480.dp)) {
                Column(Modifier.padding(20.dp)) {
                    Text(controller.server.title, style = MaterialTheme.typography.titleMedium)
                    Spacer(Modifier.height(8.dp))
                    Text(c.reason, style = MaterialTheme.typography.bodyMedium)
                    Spacer(Modifier.height(16.dp))
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(onLeave) { Text("Back") }
                        if (c.reason.contains("certificate has changed")) {
                            OutlinedButton(onForget) { Text("Trust the new key") }
                        }
                        Button(onRetry) { Text("Retry") }
                    }
                }
            }
        }
    }
}

@Composable
private fun Stats(controller: SessionController, modifier: Modifier) {
    val s = controller.stats ?: return
    Text(
        "%s · %.0f fps\nnetwork %.1f · decode %.1f · total %.1f ms\nrtt %.1f ms · lost %d · audio %.0f ms".format(
            s.encoding, s.fps, s.networkMs, s.decodeMs, s.totalMs, s.rttMs, s.lost.toLong(), s.audioMs,
        ),
        color = Color.White,
        fontFamily = FontFamily.Monospace,
        fontSize = 11.sp,
        modifier = modifier.background(Color(0x99000000), RoundedCornerShape(6.dp)).padding(6.dp),
    )
}

@Composable
private fun Toolbar(
    controller: SessionController,
    modifier: Modifier,
    onKeyboard: () -> Unit,
    onExtraKeys: () -> Unit,
    onZoom: () -> Unit,
    onStats: () -> Unit,
    onLeave: () -> Unit,
) {
    var offsetY by remember { mutableFloatStateOf(120f) }
    var shown by remember { mutableStateOf(true) }
    var touched by remember { mutableIntStateOf(0) }
    LaunchedEffect(shown, touched) {
        if (shown) {
            delay(TOOLBAR_HIDE_MS)
            shown = false
        }
    }
    val drag = Modifier.pointerInput(Unit) {
        detectVerticalDragGestures { _, dy ->
            offsetY = (offsetY + dy).coerceIn(0f, size.height * 20f)
            touched++
        }
    }
    Box(modifier.offset { IntOffset(0, offsetY.roundToInt()) }.then(drag)) {
        if (!shown) {
            Box(
                Modifier.padding(top = 8.dp).size(width = 14.dp, height = 56.dp)
                    .background(Color(0x88FFFFFF), RoundedCornerShape(topStart = 8.dp, bottomStart = 8.dp))
                    .clickable { shown = true },
            )
            return@Box
        }
        Column(
            Modifier.padding(6.dp).background(Color(0xCC202020), RoundedCornerShape(14.dp)).padding(4.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            fun act(f: () -> Unit) = { touched++; f() }
            ToolButton("⌨", "Keyboard", act(onKeyboard))
            ToolButton(
                if (controller.inputMode == InputMode.TOUCHPAD) "▭" else "☝",
                if (controller.inputMode == InputMode.TOUCHPAD) "Touchpad" else "Direct",
                act {
                    controller.inputMode =
                        if (controller.inputMode == InputMode.TOUCHPAD) InputMode.DIRECT else InputMode.TOUCHPAD
                },
            )
            ToolButton("⇧", "Keys", act(onExtraKeys))
            ToolButton("⤢", "Zoom", act(onZoom))
            ToolButton("⎘", "Clipboard", act { controller.offerClipboard() })
            ToolButton("ⓘ", "Stats", act(onStats))
            ToolButton("✕", "Leave", act(onLeave))
        }
    }
}

@Composable
private fun ToolButton(glyph: String, label: String, onClick: () -> Unit) {
    Column(
        Modifier.clickable(onClick = onClick).padding(horizontal = 6.dp, vertical = 5.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(glyph, color = Color.White, fontSize = 20.sp)
        Text(label, color = Color(0xFFBBBBBB), fontSize = 9.sp)
    }
}

/** Keys a phone's keyboard lacks, with sticky modifiers (design §7). */
@Composable
private fun ExtraKeys(controller: SessionController, modifier: Modifier) {
    Row(
        modifier.fillMaxWidth().background(Color(0xEE1A1A1A)).horizontalScroll(rememberScrollState())
            .padding(horizontal = 4.dp, vertical = 4.dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Key("Esc") { controller.tap(Evdev.KEY_ESC) }
        Key("Tab") { controller.tap(Evdev.KEY_TAB) }
        for ((label, code) in listOf(
            "Ctrl" to Evdev.KEY_LEFTCTRL, "Alt" to Evdev.KEY_LEFTALT,
            "Super" to Evdev.KEY_LEFTMETA, "Shift" to Evdev.KEY_LEFTSHIFT,
        )) {
            Key(label, latch = controller.modifiers[code]) { controller.toggleModifier(code) }
        }
        Key("←") { controller.tap(Evdev.KEY_LEFT) }
        Key("↑") { controller.tap(Evdev.KEY_UP) }
        Key("↓") { controller.tap(Evdev.KEY_DOWN) }
        Key("→") { controller.tap(Evdev.KEY_RIGHT) }
        Key("Home") { controller.tap(Evdev.KEY_HOME) }
        Key("End") { controller.tap(Evdev.KEY_END) }
        Key("PgUp") { controller.tap(Evdev.KEY_PAGEUP) }
        Key("PgDn") { controller.tap(Evdev.KEY_PAGEDOWN) }
        Key("Del") { controller.tap(Evdev.KEY_DELETE) }
        Key("Ins") { controller.tap(Evdev.KEY_INSERT) }
        for (n in 1..12) Key("F$n") { controller.tap(Evdev.function(n)) }
    }
}

@Composable
private fun Key(label: String, latch: Latch? = null, onClick: () -> Unit) {
    val background = when (latch) {
        Latch.LOCKED -> Color(0xFF3D7BFD)
        Latch.ONCE -> Color(0xFF2A3F66)
        null -> Color(0xFF333333)
    }
    Box(
        Modifier.height(40.dp).widthIn(min = 44.dp)
            .background(background, RoundedCornerShape(6.dp))
            .then(if (latch != null) Modifier.border(1.dp, Color(0xFF3D7BFD), RoundedCornerShape(6.dp)) else Modifier)
            .clickable(onClick = onClick).padding(horizontal = 8.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = Color.White, fontSize = 14.sp)
    }
}
