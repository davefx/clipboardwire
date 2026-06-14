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

        var batteryOptimized by remember { mutableStateOf(isBatteryOptimized()) }

        LaunchedEffect(Unit) {
            val s = Settings.load(this@MainActivity)
            server = s.server
            user = s.user
            password = s.password
            tlsInsecure = s.tlsInsecure
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

            OutlinedTextField(
                value = server,
                onValueChange = { server = it; saved = false },
                label = { Text("Server URL") },
                placeholder = { Text("wss://192.168.1.100:8484/sync") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                modifier = Modifier.fillMaxWidth()
            )

            OutlinedTextField(
                value = user,
                onValueChange = { user = it; saved = false },
                label = { Text("Username") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth()
            )

            OutlinedTextField(
                value = password,
                onValueChange = { password = it; saved = false },
                label = { Text("Password") },
                singleLine = true,
                visualTransformation = if (passwordVisible)
                    VisualTransformation.None else PasswordVisualTransformation(),
                trailingIcon = {
                    IconButton(onClick = { passwordVisible = !passwordVisible }) {
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
                    onCheckedChange = { tlsInsecure = it; saved = false }
                )
                Text("Skip TLS verification (LAN/VPN only)")
            }

            Spacer(modifier = Modifier.height(8.dp))

            Button(
                onClick = {
                    scope.launch {
                        Settings.save(
                            this@MainActivity,
                            Settings(server, user, password, tlsInsecure)
                        )
                        saved = true
                        ClipboardSyncService.stop(this@MainActivity)
                        if (server.isNotBlank() && user.isNotBlank()) {
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
}
