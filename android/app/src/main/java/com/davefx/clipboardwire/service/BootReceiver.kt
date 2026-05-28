// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import kotlinx.coroutines.runBlocking

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent?) {
        if (intent?.action != Intent.ACTION_BOOT_COMPLETED) return
        val settings = runBlocking { Settings.load(context) }
        if (settings.isConfigured) {
            ClipboardSyncService.start(context)
        }
    }
}
