package io.github.pablosoage.glidedesk

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import android.util.Log
import androidx.core.content.FileProvider
import java.io.File
import java.util.concurrent.Executors

/**
 * The phone's clipboard shared with the session (low-latency mode): text both ways, and
 * files. Files copied in the session arrive in the app's cache (the Rust core fetches them)
 * and go on the clipboard as content URIs of [FileProvider]; files copied on the phone are
 * copied into the cache first, then offered to the server by path.
 */
class ClipboardBridge(private val context: Context, private val session: NativeSession) {
    private val clipboard = context.getSystemService(ClipboardManager::class.java)
    private val io = Executors.newSingleThreadExecutor()
    /** What we put on the clipboard ourselves, not to send it back. */
    private var own: ClipData? = null

    private val listener = ClipboardManager.OnPrimaryClipChangedListener { onLocalCopy() }

    fun start() = clipboard?.addPrimaryClipChangedListener(listener)

    fun stop() {
        clipboard?.removePrimaryClipChangedListener(listener)
        io.shutdown()
    }

    /** Called often from the session screen: what the session copied goes on the clipboard. */
    fun poll() {
        val cb = clipboard ?: return
        session.takeClipboard()?.let { text ->
            val clip = ClipData.newPlainText(LABEL, text)
            own = clip
            cb.setPrimaryClip(clip)
        }
        session.takeFiles()?.let { paths ->
            val uris = paths.flatMap { regularFiles(File(it)) }.mapNotNull { f ->
                runCatching { FileProvider.getUriForFile(context, "${context.packageName}.files", f) }
                    .onFailure { e -> Log.w(TAG, "No URI for $f", e) }
                    .getOrNull()
            }
            if (uris.isEmpty()) return@let
            val clip = ClipData.newUri(context.contentResolver, LABEL, uris[0])
            uris.drop(1).forEach { clip.addItem(ClipData.Item(it)) }
            own = clip
            cb.setPrimaryClip(clip)
        }
    }

    private fun onLocalCopy() {
        val clip = clipboard?.primaryClip ?: return
        if (clip.description?.label == LABEL && own != null) return
        val uris = (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri }
        if (uris.isEmpty()) {
            val text = (0 until clip.itemCount)
                .mapNotNull { clip.getItemAt(it).coerceToText(context)?.toString() }
                .joinToString("\n")
            if (text.isNotEmpty()) session.clipboard(text)
            return
        }
        // Reading content URIs can take a while: off the main thread.
        io.execute {
            val dir = File(context.cacheDir, "outgoing/${System.currentTimeMillis()}").apply { mkdirs() }
            val paths = uris.mapIndexedNotNull { i, uri -> copyIn(uri, dir, i) }
            if (paths.isNotEmpty()) session.files(paths.toTypedArray())
        }
    }

    /** Copies one content URI into `dir` under its display name; its path, or null. */
    private fun copyIn(uri: Uri, dir: File, index: Int): String? = runCatching {
        val resolver = context.contentResolver
        val name = resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) c.getString(0) else null
        }?.replace('/', '_')?.takeIf { it.isNotBlank() } ?: "file-$index"
        val target = File(dir, name)
        resolver.openInputStream(uri)?.use { input -> target.outputStream().use { input.copyTo(it) } }
            ?: return@runCatching null
        target.absolutePath
    }.onFailure { Log.w(TAG, "Could not read $uri", it) }.getOrNull()

    private fun regularFiles(f: File): List<File> =
        if (f.isDirectory) f.listFiles()?.sortedBy { it.name }?.flatMap { regularFiles(it) } ?: emptyList()
        else listOf(f)

    companion object {
        private const val LABEL = "glidedesk"
        private const val TAG = "glidedesk"
    }
}
