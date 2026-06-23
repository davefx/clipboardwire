// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material.icons.filled.VisibilityOff
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import com.davefx.clipboardwire.service.ClipboardSyncService
import com.davefx.clipboardwire.service.Settings
import com.davefx.clipboardwire.ui.ClipboardwireTheme
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {

    private val notificationPermissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { _ -> }

    private var onBatteryResult: (() -> Unit)? = null
    private val batteryOptimizationLauncher =
        registerForActivityResult(ActivityResultContracts.StartActivityForResult()) {
            onBatteryResult?.invoke()
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
            != PackageManager.PERMISSION_GRANTED
        ) {
            notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }

        setContent {
            ClipboardwireTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background
                ) {
                    SettingsScreen()
                }
            }
        }
    }

    private fun isBatteryOptimized(): Boolean {
        val pm = getSystemService(PowerManager::class.java)
        return !pm.isIgnoringBatteryOptimizations(packageName)
    }

    private fun requestBatteryOptimizationExemption(onDone: () -> Unit) {
        val intent = Intent(
            ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
            Uri.parse("package:$packageName")
        )
        if (intent.resolveActivity(packageManager) != null) {
            onBatteryResult = onDone
            batteryOptimizationLauncher.launch(intent)
        }
    }

    @OptIn(ExperimentalMaterial3Api::class)
    @Composable
    fun SettingsScreen() {
        val scope = rememberCoroutineScope()
        var server by remember { mutableStateOf("") }
        var user by remember { mutableStateOf("") }
        var password by remember { mutableStateOf("") }
        var tlsInsecure by remember { mutableStateOf(false) }
        var passwordVisible by remember { mutableStateOf(false) }
        var loaded by remember { mutableStateOf(false) }
        var saved by remember { mutableStateOf(false) }

        var serverMode by remember { mutableStateOf(false) }
        var serverPort by remember { mutableStateOf("8484") }
        var serverUser by remember { mutableStateOf("") }
        var serverPassword by remember { mutableStateOf("") }
        var serverPasswordVisible by remember { mutableStateOf(false) }

        var batteryOptimized by remember { mutableStateOf(isBatteryOptimized()) }

        LaunchedEffect(Unit) {
            val s = Settings.load(this@MainActivity)
            server = s.server
            user = s.user
            password = s.password
            tlsInsecure = s.tlsInsecure
            serverMode = s.serverMode
            serverPort = s.serverPort.toString()
            serverUser = s.serverUser
            serverPassword = s.serverPassword
            loaded = true
        }

        if (!loaded) return

        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp)
        ) {
            Text(
                "clipboardwire",
                style = MaterialTheme.typography.headlineMedium
            )

            if (batteryOptimized) {
                Card(
                    colors = CardDefaults.cardColors(
                        containerColor = MaterialTheme.colorScheme.secondaryContainer
                    ),
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Column(modifier = Modifier.padding(16.dp)) {
                        Text(
                            "Battery optimization is enabled",
                            style = MaterialTheme.typography.titleSmall
                        )
                        Spacer(modifier = Modifier.height(4.dp))
                        Text(
                            "The system may kill the background service. " +
                                "Tap below to exempt clipboardwire.",
                            style = MaterialTheme.typography.bodySmall
                        )
                        Spacer(modifier = Modifier.height(8.dp))
                        TextButton(onClick = {
                            requestBatteryOptimizationExemption {
                                batteryOptimized = isBatteryOptimized()
                            }
                        }) {
                            Text("Disable battery optimization")
                        }
                    }
                }
            }

            // Mode toggle
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth()
            ) {
                Text("Mode", modifier = Modifier.weight(1f))
                SingleChoiceSegmentedButtonRow {
                    SegmentedButton(
                        selected = !serverMode,
                        onClick = { serverMode = false; saved = false },
                        shape = SegmentedButtonDefaults.itemShape(index = 0, count = 2)
                    ) { Text("Client") }
                    SegmentedButton(
                        selected = serverMode,
                        onClick = { serverMode = true; saved = false },
                        shape = SegmentedButtonDefaults.itemShape(index = 1, count = 2)
                    ) { Text("Server") }
                }
            }

            if (serverMode) {
                ServerModeFields(
                    port = serverPort,
                    onPortChange = { serverPort = it; saved = false },
                    user = serverUser,
                    onUserChange = { serverUser = it; saved = false },
                    password = serverPassword,
                    onPasswordChange = { serverPassword = it; saved = false },
                    passwordVisible = serverPasswordVisible,
                    onPasswordVisibilityChange = { serverPasswordVisible = it }
                )
            } else {
                ClientModeFields(
                    server = server,
                    onServerChange = { server = it; saved = false },
                    user = user,
                    onUserChange = { user = it; saved = false },
                    password = password,
                    onPasswordChange = { password = it; saved = false },
                    passwordVisible = passwordVisible,
                    onPasswordVisibilityChange = { passwordVisible = it },
                    tlsInsecure = tlsInsecure,
                    onTlsInsecureChange = { tlsInsecure = it; saved = false }
                )
            }

            Spacer(modifier = Modifier.height(8.dp))

            Button(
                onClick = {
                    scope.launch {
                        val portInt = serverPort.toIntOrNull() ?: 8484
                        Settings.save(
                            this@MainActivity,
                            Settings(
                                server, user, password, tlsInsecure,
                                serverMode, portInt, serverUser, serverPassword
                            )
                        )
                        saved = true
                        ClipboardSyncService.stop(this@MainActivity)
                        val settings = Settings(
                            server, user, password, tlsInsecure,
                            serverMode, portInt, serverUser, serverPassword
                        )
                        if (settings.isConfigured) {
                            kotlinx.coroutines.delay(500)
                            ClipboardSyncService.start(this@MainActivity)
                        }
                    }
                },
                modifier = Modifier.fillMaxWidth()
            ) {
                Text(if (saved) "Saved — service restarted" else "Save & Connect")
            }

            OutlinedButton(
                onClick = { ClipboardSyncService.stop(this@MainActivity) },
                modifier = Modifier.fillMaxWidth()
            ) {
                Text("Stop service")
            }
        }
    }

    @Composable
    private fun ClientModeFields(
        server: String, onServerChange: (String) -> Unit,
        user: String, onUserChange: (String) -> Unit,
        password: String, onPasswordChange: (String) -> Unit,
        passwordVisible: Boolean, onPasswordVisibilityChange: (Boolean) -> Unit,
        tlsInsecure: Boolean, onTlsInsecureChange: (Boolean) -> Unit
    ) {
        OutlinedTextField(
            value = server,
            onValueChange = onServerChange,
            label = { Text("Server URL") },
            placeholder = { Text("wss://192.168.1.100:8484/sync") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
            modifier = Modifier.fillMaxWidth()
        )

        OutlinedTextField(
            value = user,
            onValueChange = onUserChange,
            label = { Text("Username") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth()
        )

        OutlinedTextField(
            value = password,
            onValueChange = onPasswordChange,
            label = { Text("Password") },
            singleLine = true,
            visualTransformation = if (passwordVisible)
                VisualTransformation.None else PasswordVisualTransformation(),
            trailingIcon = {
                IconButton(onClick = { onPasswordVisibilityChange(!passwordVisible) }) {
                    Icon(
                        if (passwordVisible) Icons.Default.VisibilityOff
                        else Icons.Default.Visibility,
                        contentDescription = "Toggle password visibility"
                    )
                }
            },
            modifier = Modifier.fillMaxWidth()
        )

        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier.fillMaxWidth()
        ) {
            Checkbox(
                checked = tlsInsecure,
                onCheckedChange = onTlsInsecureChange
            )
            Text("Skip TLS verification (LAN/VPN only)")
        }
    }

    @Composable
    private fun ServerModeFields(
        port: String, onPortChange: (String) -> Unit,
        user: String, onUserChange: (String) -> Unit,
        password: String, onPasswordChange: (String) -> Unit,
        passwordVisible: Boolean, onPasswordVisibilityChange: (Boolean) -> Unit
    ) {
        Text(
            "This device will run a clipboard hub server. " +
                "Other devices connect to it as clients.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )

        OutlinedTextField(
            value = port,
            onValueChange = { onPortChange(it.filter { c -> c.isDigit() }) },
            label = { Text("Listen port") },
            placeholder = { Text("8484") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
            modifier = Modifier.fillMaxWidth()
        )

        OutlinedTextField(
            value = user,
            onValueChange = onUserChange,
            label = { Text("Username") },
            supportingText = { Text("Clients connect with these credentials") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth()
        )

        OutlinedTextField(
            value = password,
            onValueChange = onPasswordChange,
            label = { Text("Password") },
            singleLine = true,
            visualTransformation = if (passwordVisible)
                VisualTransformation.None else PasswordVisualTransformation(),
            trailingIcon = {
                IconButton(onClick = { onPasswordVisibilityChange(!passwordVisible) }) {
                    Icon(
                        if (passwordVisible) Icons.Default.VisibilityOff
                        else Icons.Default.Visibility,
                        contentDescription = "Toggle password visibility"
                    )
                }
            },
            modifier = Modifier.fillMaxWidth()
        )
    }
}
