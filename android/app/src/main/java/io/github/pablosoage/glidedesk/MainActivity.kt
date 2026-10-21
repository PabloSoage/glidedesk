package io.github.pablosoage.glidedesk

import android.content.Context
import android.os.Bundle
import android.view.inputmethod.InputMethodManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Keyboard
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import kotlinx.coroutines.delay

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val prefs = getSharedPreferences("connection", Context.MODE_PRIVATE)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(Modifier.fillMaxSize()) {
                    var session by remember { mutableStateOf<NativeSession?>(null) }
                    var scale by remember { mutableStateOf(2f) }
                    var message by remember { mutableStateOf("") }
                    // Debug builds only: `am start -e host H -e user U -e password P [-e scale S]`
                    // connects straight away (for automated tests on the emulator).
                    LaunchedEffect(Unit) {
                        val extras = intent.extras
                        if (BuildConfig.DEBUG && extras?.getString("host") != null && session == null) {
                            val sc = extras.getString("scale")?.toFloatOrNull() ?: 1f
                            scale = sc
                            val m = resources.displayMetrics
                            session = NativeSession.connect(
                                extras.getString("host")!!, extras.getString("port")?.toIntOrNull() ?: 3389,
                                extras.getString("user") ?: "", extras.getString("password") ?: "",
                                (m.widthPixels / sc).toInt(), (m.heightPixels / sc).toInt(),
                            )
                        }
                    }
                    val s = session
                    if (s == null) {
                        ConnectScreen(prefs, message) { host, port, user, password, sc ->
                            scale = sc
                            val m = resources.displayMetrics
                            session = NativeSession.connect(
                                host, port, user, password,
                                (m.widthPixels / sc).toInt(), (m.heightPixels / sc).toInt(),
                            )
                            message = if (session == null) "Could not start the session" else ""
                        }
                    } else {
                        SessionScreen(s, scale) { reason ->
                            s.close()
                            session = null
                            message = reason
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun ConnectScreen(
    prefs: android.content.SharedPreferences,
    message: String,
    onConnect: (String, Int, String, String, Float) -> Unit,
) {
    var host by remember { mutableStateOf(prefs.getString("host", "") ?: "") }
    var port by remember { mutableStateOf(prefs.getInt("port", 3389).toString()) }
    var user by remember { mutableStateOf(prefs.getString("user", "") ?: "") }
    var password by remember { mutableStateOf("") }
    var scale by remember { mutableStateOf(prefs.getFloat("scale", 2f).toString()) }
    Column(
        Modifier.fillMaxSize().padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp, Alignment.CenterVertically),
    ) {
        Text("glidedesk", style = MaterialTheme.typography.headlineMedium)
        OutlinedTextField(host, { host = it }, Modifier.fillMaxWidth(), label = { Text("Host") }, singleLine = true)
        OutlinedTextField(
            port, { port = it }, Modifier.fillMaxWidth(), label = { Text("Port") }, singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        )
        OutlinedTextField(user, { user = it }, Modifier.fillMaxWidth(), label = { Text("User") }, singleLine = true)
        OutlinedTextField(
            password, { password = it }, Modifier.fillMaxWidth(), label = { Text("Password") }, singleLine = true,
            visualTransformation = PasswordVisualTransformation(),
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
        )
        OutlinedTextField(
            scale, { scale = it }, Modifier.fillMaxWidth(), label = { Text("Scale (screen pixels per desktop pixel)") },
            singleLine = true, keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Decimal),
        )
        Button(
            onClick = {
                val p = port.toIntOrNull() ?: 3389
                val sc = scale.toFloatOrNull()?.coerceIn(1f, 4f) ?: 2f
                prefs.edit().putString("host", host).putInt("port", p).putString("user", user).putFloat("scale", sc).apply()
                onConnect(host.trim(), p, user.trim(), password, sc)
            },
            enabled = host.isNotBlank() && user.isNotBlank(),
            modifier = Modifier.fillMaxWidth(),
        ) { Text("Connect") }
        if (message.isNotEmpty()) Text(message, color = MaterialTheme.colorScheme.error)
    }
}

@Composable
private fun SessionScreen(session: NativeSession, scale: Float, onEnd: (String) -> Unit) {
    var state by remember { mutableStateOf(NativeSession.State.CONNECTING) }
    var view by remember { mutableStateOf<SessionView?>(null) }
    LaunchedEffect(session) {
        while (true) {
            state = session.state
            when (state) {
                NativeSession.State.FAILED -> { onEnd(session.error.ifEmpty { "Connection failed" }); return@LaunchedEffect }
                NativeSession.State.ENDED -> { onEnd("Session ended"); return@LaunchedEffect }
                else -> delay(250)
            }
        }
    }
    BackHandler { onEnd("") }
    // The desktop takes the whole screen: hide the status and navigation bars while the
    // session is open (a swipe from the edge shows them briefly).
    val window = (androidx.compose.ui.platform.LocalContext.current as? ComponentActivity)?.window
    androidx.compose.runtime.DisposableEffect(window) {
        val controller = window?.let { androidx.core.view.WindowCompat.getInsetsController(it, it.decorView) }
        controller?.systemBarsBehavior =
            androidx.core.view.WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        controller?.hide(androidx.core.view.WindowInsetsCompat.Type.systemBars())
        onDispose { controller?.show(androidx.core.view.WindowInsetsCompat.Type.systemBars()) }
    }
    Box(Modifier.fillMaxSize()) {
        AndroidView(
            factory = { ctx -> SessionView(ctx, session, scale).also { view = it; it.requestFocus() } },
            modifier = Modifier.fillMaxSize(),
        )
        if (state == NativeSession.State.CONNECTING) {
            CircularProgressIndicator(Modifier.align(Alignment.Center))
        }
        Column(Modifier.align(Alignment.TopEnd).padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            SmallFloatingActionButton(onClick = {
                view?.let {
                    it.requestFocus()
                    val imm = it.context.getSystemService(InputMethodManager::class.java)
                    imm.showSoftInput(it, InputMethodManager.SHOW_IMPLICIT)
                }
            }) { androidx.compose.material3.Icon(Icons.Filled.Keyboard, "Keyboard") }
            SmallFloatingActionButton(onClick = { onEnd("") }) {
                androidx.compose.material3.Icon(Icons.Filled.Close, "Disconnect")
            }
        }
    }
}
