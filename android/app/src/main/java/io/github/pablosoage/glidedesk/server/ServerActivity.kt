package io.github.pablosoage.glidedesk.server

import android.Manifest
import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import io.github.pablosoage.glidedesk.BuildConfig
import io.github.pablosoage.glidedesk.shell.ShellConnection
import kotlinx.coroutines.delay
import rikka.shizuku.Shizuku
import java.net.Inet4Address
import java.net.NetworkInterface

/**
 * The server screen: share this phone's screen with glidedesk clients. Three ways in, from
 * the most capable: the privileged helper through Shizuku, the same helper started from
 * ADB, or screen capture with an accessibility service for the input (no privileges).
 */
class ServerActivity : ComponentActivity() {
    private val shizukuResult = Shizuku.OnRequestPermissionResultListener { _, _ -> }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        Shizuku.addRequestPermissionResultListener(shizukuResult)
        val settings = ServerSettings(this)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(Modifier.fillMaxSize()) { ServerScreen(settings) }
            }
        }
    }

    override fun onDestroy() {
        Shizuku.removeRequestPermissionResultListener(shizukuResult)
        super.onDestroy()
    }
}

/** The command that starts the helper over ADB (from a computer, or Termux on the phone). */
private val ADB_COMMAND =
    "adb shell 'CLASSPATH=\$(pm path ${BuildConfig.APPLICATION_ID} | head -n 1 | cut -d: -f2) " +
        "nohup app_process /system/bin ${BuildConfig.APPLICATION_ID}.shell.ShellMain >/dev/null 2>&1 &'"

/** The phone's IPv4 addresses, where clients connect. */
private fun addresses(): List<String> = runCatching {
    NetworkInterface.getNetworkInterfaces().toList()
        .filter { it.isUp && !it.isLoopback }
        .flatMap { it.inetAddresses.toList() }
        .filterIsInstance<Inet4Address>()
        .map { it.hostAddress ?: "" }
        .filter { it.isNotEmpty() }
}.getOrDefault(emptyList())

@Composable
private fun ServerScreen(settings: ServerSettings) {
    val context = androidx.compose.ui.platform.LocalContext.current
    val activity = context as Activity
    var way by remember { mutableStateOf(settings.way) }
    var user by remember { mutableStateOf(settings.user) }
    var password by remember { mutableStateOf(settings.password) }
    var port by remember { mutableStateOf(settings.port.toString()) }
    var ownDisplay by remember { mutableStateOf(settings.ownDisplay) }
    var maxSize by remember { mutableStateOf(settings.maxSize.toString()) }
    var bitrate by remember { mutableStateOf(settings.maxBitrateMbps.toString()) }

    // Polled, as the session screen does: the service, Shizuku, the helper, the accessibility service.
    var running by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf("") }
    var fingerprint by remember { mutableStateOf("") }
    var error by remember { mutableStateOf("") }
    var shizuku by remember { mutableStateOf(ShellConnection.ShizukuState.NOT_RUNNING) }
    var helper by remember { mutableStateOf("") }
    var accessibility by remember { mutableStateOf(false) }
    var ips by remember { mutableStateOf(emptyList<String>()) }
    LaunchedEffect(Unit) {
        while (true) {
            val service = ScreenServerService.instance
            running = service != null
            status = service?.status() ?: ""
            fingerprint = service?.fingerprint() ?: ""
            error = ScreenServerService.lastError
            shizuku = runCatching { ShellConnection.shizukuState() }.getOrDefault(ShellConnection.ShizukuState.NOT_RUNNING)
            helper = if (ShellConnection.client != null) ShellConnection.via else ""
            accessibility = InputAccessibilityService.instance != null
            ips = addresses()
            delay(500)
        }
    }

    val save = {
        settings.way = way
        settings.user = user.trim()
        settings.password = password
        settings.port = port.toIntOrNull()?.coerceIn(1, 65535) ?: 3391
        settings.ownDisplay = ownDisplay
        settings.maxSize = maxSize.toIntOrNull()?.coerceIn(320, 4096) ?: 1920
        settings.maxBitrateMbps = bitrate.toIntOrNull()?.coerceIn(1, 200) ?: 16
    }
    val capture = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        val data = result.data
        if (result.resultCode == Activity.RESULT_OK && data != null) ScreenServerService.start(context, data)
    }
    val notifications = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {}
    val start = {
        save()
        if (Build.VERSION.SDK_INT >= 33 &&
            context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            notifications.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        when (way) {
            ServerSettings.Way.PROJECTION -> {
                val mpm = context.getSystemService(MediaProjectionManager::class.java)
                capture.launch(mpm.createScreenCaptureIntent())
            }
            // The service starts (or reaches) the helper.
            ServerSettings.Way.SHIZUKU, ServerSettings.Way.ADB -> ScreenServerService.start(context)
        }
    }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Share this phone", style = MaterialTheme.typography.headlineMedium)
        Text(
            "glidedesk clients (low latency mode) connect to this phone and see and drive its screen.",
            style = MaterialTheme.typography.bodyMedium,
        )

        Text("How", style = MaterialTheme.typography.titleMedium)
        Choice(way == ServerSettings.Way.SHIZUKU, "Shizuku (recommended): no prompt, real mouse and keyboard") {
            way = ServerSettings.Way.SHIZUKU
        }
        Choice(way == ServerSettings.Way.ADB, "ADB helper: the same, started with an adb command") {
            way = ServerSettings.Way.ADB
        }
        Choice(way == ServerSettings.Way.PROJECTION, "Screen capture: no setup, asks each time, touch only") {
            way = ServerSettings.Way.PROJECTION
        }
        when (way) {
            ServerSettings.Way.SHIZUKU -> when (shizuku) {
                ShellConnection.ShizukuState.NOT_RUNNING ->
                    Text("Shizuku is not running: install it and start it (wireless debugging or root).", color = MaterialTheme.colorScheme.error)
                ShellConnection.ShizukuState.NO_PERMISSION -> OutlinedButton(onClick = { Shizuku.requestPermission(REQUEST_SHIZUKU) }) {
                    Text("Allow glidedesk in Shizuku")
                }
                ShellConnection.ShizukuState.READY -> Text("Shizuku is ready.")
            }
            ServerSettings.Way.ADB -> {
                Text(
                    if (helper.isNotEmpty()) "The helper is running ($helper)." else "Run this from a computer or from Termux (adb connected to this phone); again after a reboot or when the app was closed:",
                )
                SelectionContainer {
                    Text(ADB_COMMAND, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                }
                OutlinedButton(onClick = {
                    context.getSystemService(ClipboardManager::class.java)
                        ?.setPrimaryClip(ClipData.newPlainText("adb", ADB_COMMAND))
                }) { Text("Copy the command") }
            }
            ServerSettings.Way.PROJECTION -> {
                Text(
                    if (accessibility) {
                        "Input: the glidedesk accessibility service is on."
                    } else {
                        "For input, turn on glidedesk in the accessibility settings (without it the client only watches)."
                    },
                )
                if (!accessibility) {
                    OutlinedButton(onClick = {
                        activity.startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
                    }) { Text("Open the accessibility settings") }
                }
            }
        }
        if (way != ServerSettings.Way.PROJECTION) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Switch(checked = ownDisplay, onCheckedChange = { ownDisplay = it })
                Text(
                    if (ownDisplay) "  A display of its own, at the client's size" else "  A copy of the phone's screen",
                    Modifier.padding(start = 8.dp),
                )
            }
        }

        Text("Login", style = MaterialTheme.typography.titleMedium)
        OutlinedTextField(user, { user = it }, Modifier.fillMaxWidth(), label = { Text("User") }, singleLine = true)
        OutlinedTextField(
            password, { password = it }, Modifier.fillMaxWidth(), label = { Text("Password") }, singleLine = true,
            visualTransformation = PasswordVisualTransformation(),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
        )
        OutlinedTextField(
            port, { port = it }, Modifier.fillMaxWidth(), label = { Text("Port (TCP and UDP)") }, singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        )
        OutlinedTextField(
            maxSize, { maxSize = it }, Modifier.fillMaxWidth(), label = { Text("Longest side of the video (pixels)") },
            singleLine = true, keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        )
        OutlinedTextField(
            bitrate, { bitrate = it }, Modifier.fillMaxWidth(), label = { Text("Highest bitrate (Mb/s)") },
            singleLine = true, keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        )

        if (running) {
            Button(onClick = { ScreenServerService.stop(context) }, Modifier.fillMaxWidth()) { Text("Stop") }
            Text(status.ifEmpty { "Starting…" })
            if (ips.isNotEmpty()) Text("Connect to: " + ips.joinToString(", ") { "$it:${port.trim()}" })
            if (fingerprint.isNotEmpty()) {
                Text("Certificate fingerprint (the client asks to trust it):", style = MaterialTheme.typography.bodySmall)
                SelectionContainer {
                    Text(fingerprint, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                }
            }
        } else {
            Button(
                onClick = start,
                enabled = user.isNotBlank() && password.isNotEmpty() &&
                    (way != ServerSettings.Way.SHIZUKU || shizuku == ShellConnection.ShizukuState.READY),
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Start") }
        }
        if (error.isNotEmpty()) Text(error, color = MaterialTheme.colorScheme.error)
    }

    // Leaving the screen does not stop the server: its notification has the Stop button.
    DisposableEffect(Unit) { onDispose { save() } }
}

@Composable
private fun Choice(selected: Boolean, label: String, onSelect: () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        RadioButton(selected = selected, onClick = onSelect)
        Text(label, Modifier.padding(start = 4.dp))
    }
}

private const val REQUEST_SHIZUKU = 1
