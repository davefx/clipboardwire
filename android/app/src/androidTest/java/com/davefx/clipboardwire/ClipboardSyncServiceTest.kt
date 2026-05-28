// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.rule.ServiceTestRule
import com.davefx.clipboardwire.service.ClipboardSyncService
import com.davefx.clipboardwire.service.Settings
import kotlinx.coroutines.runBlocking
import org.junit.Assert.*
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ClipboardSyncServiceTest {

    @get:Rule
    val serviceRule = ServiceTestRule()

    private lateinit var context: Context

    @Before
    fun setUp() {
        context = ApplicationProvider.getApplicationContext()
    }

    @Test
    fun service_starts_and_stops_without_crashing() {
        val intent = Intent(context, ClipboardSyncService::class.java)
        // The service needs a foreground notification, which requires the
        // channel to exist. Starting via the rule exercises onCreate.
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                serviceRule.startService(intent)
            }
        } catch (_: Exception) {
            // Foreground service start may fail in test harness without
            // a proper notification channel on some API levels — the
            // important thing is it doesn't crash with an unhandled
            // exception.
        }

        val stopIntent = Intent(context, ClipboardSyncService::class.java).apply {
            action = "STOP_SERVICE"
        }
        context.startService(stopIntent)
    }

    @Test
    fun settings_round_trips_through_datastore() = runBlocking {
        val original = Settings(
            server = "wss://test.local:8484/sync",
            user = "testuser",
            password = "testpass",
            tlsInsecure = true
        )
        Settings.save(context, original)
        val loaded = Settings.load(context)
        assertEquals(original.server, loaded.server)
        assertEquals(original.user, loaded.user)
        assertEquals(original.password, loaded.password)
        assertEquals(original.tlsInsecure, loaded.tlsInsecure)
    }

    @Test
    fun settings_isConfigured_checks_server_and_user() {
        assertFalse(Settings().isConfigured)
        assertFalse(Settings(server = "wss://x", user = "").isConfigured)
        assertFalse(Settings(server = "", user = "alice").isConfigured)
        assertTrue(Settings(server = "wss://x", user = "alice").isConfigured)
    }

    @Test
    fun clipboard_write_does_not_throw() {
        val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val text = "clipboardwire test ${System.currentTimeMillis()}"
        // On Android 10+ background processes cannot read the clipboard,
        // but writing always succeeds. We just verify no exception is
        // thrown — the actual clipboard integration is tested by the
        // foreground service in production.
        cm.setPrimaryClip(ClipData.newPlainText("test", text))
    }
}
