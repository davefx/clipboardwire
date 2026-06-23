// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import android.util.Log

object NativeServer {
    private const val TAG = "clipboardwire.native"

    init {
        System.loadLibrary("clipboardwire_android_bridge")
    }

    @Volatile
    private var handle: Long = 0L

    val isRunning: Boolean get() = handle != 0L

    val clientCount: Int
        get() = if (handle != 0L) nativeGetClientCount(handle) else 0

    fun start(
        bindPort: Int,
        user: String,
        password: String,
        stateDir: String,
        tlsDisabled: Boolean,
        pingIntervalSecs: Long,
        readTimeoutSecs: Long
    ): Boolean {
        if (handle != 0L) {
            Log.w(TAG, "server already running")
            return true
        }
        return try {
            handle = nativeStart(
                bindPort, user, password, stateDir,
                tlsDisabled, pingIntervalSecs, readTimeoutSecs
            )
            handle != 0L
        } catch (e: RuntimeException) {
            Log.e(TAG, "failed to start server: ${e.message}")
            false
        }
    }

    fun stop() {
        val h = handle
        if (h == 0L) return
        handle = 0L
        nativeStop(h)
    }

    private external fun nativeStart(
        bindPort: Int,
        user: String,
        password: String,
        stateDir: String,
        tlsDisabled: Boolean,
        pingIntervalSecs: Long,
        readTimeoutSecs: Long
    ): Long

    private external fun nativeStop(handle: Long)
    private external fun nativeGetClientCount(handle: Long): Int
}
