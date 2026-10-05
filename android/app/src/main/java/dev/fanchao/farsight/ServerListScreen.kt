package dev.fanchao.farsight

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.text.format.DateUtils
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** Saved servers, most recent first, each with the last look at its desktop (design §7). */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalFoundationApi::class)
@Composable
fun ServerListScreen(
    servers: List<Server>,
    /** Changes when a thumbnail is saved. */
    thumbnails: Int,
    thumbnail: (Server) -> Bitmap?,
    keyLine: () -> String,
    onConnect: (Server) -> Unit,
    onSave: (Server) -> Unit,
    onDelete: (Server) -> Unit,
) {
    var editing by remember { mutableStateOf<Server?>(null) }
    var showKey by remember { mutableStateOf(false) }
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("farsight") },
                actions = { TextButton({ showKey = true }) { Text("This device's key") } },
            )
        },
        floatingActionButton = {
            FloatingActionButton({ editing = Server() }) { Text("+", fontSize = 24.sp) }
        },
    ) { padding ->
        if (servers.isEmpty()) {
            Box(Modifier.fillMaxSize().padding(padding).padding(32.dp), contentAlignment = Alignment.Center) {
                Text(
                    "No servers yet. Add one with +, and authorize this device on it: its key is under " +
                        "\"This device's key\".",
                    style = MaterialTheme.typography.bodyLarge,
                )
            }
        }
        LazyColumn(
            Modifier.fillMaxSize().padding(padding),
            contentPadding = PaddingValues(12.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            items(servers.sortedByDescending { it.lastUsed }, key = { it.id }) { server ->
                Card(Modifier.fillMaxWidth().combinedClickable(onClick = { onConnect(server) }, onLongClick = { editing = server })) {
                    val thumb = remember(server.id, thumbnails) { thumbnail(server) }
                    if (thumb != null) {
                        Image(
                            thumb.asImageBitmap(), null,
                            Modifier.fillMaxWidth().aspectRatio(thumb.width.toFloat() / thumb.height),
                            contentScale = ContentScale.Crop,
                        )
                    } else {
                        Box(Modifier.fillMaxWidth().height(72.dp).background(Color(0xFF263238)))
                    }
                    Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text(server.title, style = MaterialTheme.typography.titleMedium)
                            val last = if (server.lastUsed > 0) " · " +
                                DateUtils.getRelativeTimeSpanString(server.lastUsed) else ""
                            Text(server.address + last, style = MaterialTheme.typography.bodySmall)
                        }
                        TextButton({ editing = server }) { Text("Edit") }
                    }
                }
            }
        }
    }
    editing?.let { server ->
        EditServer(
            server,
            onDismiss = { editing = null },
            onSave = { editing = null; onSave(it) },
            onDelete = { editing = null; onDelete(it) },
        )
    }
    if (showKey) KeyDialog(keyLine(), onDismiss = { showKey = false })
}

@Composable
private fun KeyDialog(line: String, onDismiss: () -> Unit) {
    val context = LocalContext.current
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("This device's key") },
        text = {
            Column {
                Text("Add this line to ~/.config/farsight/authorized_keys on each server:")
                Spacer(Modifier.height(8.dp))
                Text(line, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
            }
        },
        confirmButton = {
            TextButton({
                context.getSystemService(ClipboardManager::class.java)
                    .setPrimaryClip(ClipData.newPlainText("farsight key", line))
                onDismiss()
            }) { Text("Copy") }
        },
        dismissButton = {
            TextButton({ share(context, line) }) { Text("Share") }
        },
    )
}

private fun share(context: Context, text: String) {
    val intent = Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_TEXT, text)
    context.startActivity(Intent.createChooser(intent, "Share this device's key"))
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun EditServer(server: Server, onDismiss: () -> Unit, onSave: (Server) -> Unit, onDelete: (Server) -> Unit) {
    var s by remember { mutableStateOf(server) }
    val isNew = server.address.isEmpty()
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (isNew) "Add a server" else "Edit ${server.title}") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    s.address, { s = s.copy(address = it.trim()) }, label = { Text("Address (host or host:port)") },
                    singleLine = true, keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                )
                OutlinedTextField(s.name, { s = s.copy(name = it) }, label = { Text("Name") }, singleLine = true)
                Choice("Touch", InputMode.entries, s.inputMode, { if (it == InputMode.TOUCHPAD) "Touchpad" else "Direct" }) {
                    s = s.copy(inputMode = it)
                }
                Choice("Scale", listOf(0, 100, 125, 150, 200, 250), s.scalePercent, { if (it == 0) "Device" else "$it%" }) {
                    s = s.copy(scalePercent = it)
                }
                OutlinedTextField(
                    s.resolution, { s = s.copy(resolution = it.trim()) },
                    label = { Text("Fixed size, e.g. 1920x1080 (empty follows the screen)") }, singleLine = true,
                )
                Choice("Microphone", MicPolicy.entries, s.mic, { it.name.lowercase().replaceFirstChar(Char::uppercase) }) {
                    s = s.copy(mic = it)
                }
                Toggle("Echo cancellation (off with headphones)", s.echoCancel) { s = s.copy(echoCancel = it) }
                Toggle("Sound", s.audio) { s = s.copy(audio = it) }
                Toggle("Prefer motion to sharp text", s.motion) { s = s.copy(motion = it) }
                Toggle("Watch only", s.viewOnly) { s = s.copy(viewOnly = it) }
                Toggle("Plaintext (tailnets only)", s.plain) { s = s.copy(plain = it) }
            }
        },
        confirmButton = { TextButton({ onSave(s) }, enabled = s.address.isNotBlank()) { Text("Save") } },
        dismissButton = {
            Row {
                if (!isNew) TextButton({ onDelete(server) }) { Text("Delete") }
                TextButton(onDismiss) { Text("Cancel") }
            }
        },
    )
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun <T> Choice(title: String, options: List<T>, selected: T, label: (T) -> String, onSelect: (T) -> Unit) {
    Column {
        Text(title, style = MaterialTheme.typography.labelLarge)
        FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            for (o in options) FilterChip(o == selected, { onSelect(o) }, label = { Text(label(o)) })
        }
    }
}

@Composable
private fun Toggle(title: String, value: Boolean, onChange: (Boolean) -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(title, Modifier.weight(1f))
        Switch(value, onChange)
    }
}
