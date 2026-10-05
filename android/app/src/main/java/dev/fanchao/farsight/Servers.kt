package dev.fanchao.farsight

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.util.UUID

/** Whether the session may have the microphone when an app in it records (design §8). */
enum class MicPolicy { NEVER, ASK, ALWAYS }

/** A saved server, with how to connect to it. */
data class Server(
    val id: String = UUID.randomUUID().toString(),
    val name: String = "",
    /** `host[:port]`. */
    val address: String = "",
    /** Plaintext mode, for a server on a tailnet (design §1). */
    val plain: Boolean = false,
    val inputMode: InputMode = InputMode.TOUCHPAD,
    /** The desktop's scale in percent; 0 follows the device's density. */
    val scalePercent: Int = 0,
    /** `WIDTHxHEIGHT` to keep the desktop at; empty follows the screen. */
    val resolution: String = "",
    val motion: Boolean = false,
    val audio: Boolean = true,
    val mic: MicPolicy = MicPolicy.ASK,
    val viewOnly: Boolean = false,
    /** When it was last connected to, in ms; 0 if never. */
    val lastUsed: Long = 0,
) {
    val title: String get() = name.ifBlank { address }

    /** The fixed size asked for, if there is one. */
    fun fixedSize(): Pair<Int, Int>? {
        val parts = resolution.lowercase().split('x')
        if (parts.size != 2) return null
        val w = parts[0].trim().toIntOrNull() ?: return null
        val h = parts[1].trim().toIntOrNull() ?: return null
        return if (w >= 64 && h >= 64) w to h else null
    }

    fun toJson(): JSONObject = JSONObject()
        .put("id", id)
        .put("name", name)
        .put("address", address)
        .put("plain", plain)
        .put("inputMode", inputMode.name)
        .put("scalePercent", scalePercent)
        .put("resolution", resolution)
        .put("motion", motion)
        .put("audio", audio)
        .put("mic", mic.name)
        .put("viewOnly", viewOnly)
        .put("lastUsed", lastUsed)

    companion object {
        fun fromJson(o: JSONObject) = Server(
            id = o.getString("id"),
            name = o.optString("name"),
            address = o.optString("address"),
            plain = o.optBoolean("plain"),
            inputMode = runCatching { InputMode.valueOf(o.optString("inputMode")) }.getOrDefault(InputMode.TOUCHPAD),
            scalePercent = o.optInt("scalePercent"),
            resolution = o.optString("resolution"),
            motion = o.optBoolean("motion"),
            audio = o.optBoolean("audio", true),
            mic = runCatching { MicPolicy.valueOf(o.optString("mic")) }.getOrDefault(MicPolicy.ASK),
            viewOnly = o.optBoolean("viewOnly"),
            lastUsed = o.optLong("lastUsed"),
        )
    }
}

/** The address book, in the app's files, with a thumbnail of each server's desktop. */
class Servers(context: Context) {
    private val dir = context.filesDir
    private val file = File(dir, "servers.json")
    private val thumbs = File(dir, "thumbs")

    /** Where this device's key and known servers live, for the Rust side. */
    val configDir: String = File(dir, "farsight").apply { mkdirs() }.path

    fun load(): List<Server> = runCatching {
        val array = JSONArray(file.readText())
        (0 until array.length()).map { Server.fromJson(array.getJSONObject(it)) }
    }.getOrDefault(emptyList())

    fun save(servers: List<Server>) {
        val array = JSONArray()
        servers.forEach { array.put(it.toJson()) }
        val tmp = File(dir, "servers.json.tmp")
        tmp.writeText(array.toString(2))
        tmp.renameTo(file)
    }

    fun put(server: Server): List<Server> {
        val all = load().filter { it.id != server.id } + server
        save(all)
        return all
    }

    fun remove(server: Server): List<Server> {
        File(thumbs, "${server.id}.png").delete()
        val all = load().filter { it.id != server.id }
        save(all)
        return all
    }

    fun thumbnail(server: Server): Bitmap? =
        File(thumbs, "${server.id}.png").takeIf { it.exists() }?.let { BitmapFactory.decodeFile(it.path) }

    fun saveThumbnail(server: Server, bitmap: Bitmap) {
        thumbs.mkdirs()
        File(thumbs, "${server.id}.png").outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 90, it) }
    }
}
