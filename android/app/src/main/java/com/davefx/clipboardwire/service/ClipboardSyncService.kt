// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import android.app.*
import android.content.*
import android.content.pm.ServiceInfo
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.Build
import android.os.IBinder
import android.util.Base64
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.FileProvider
import kotlinx.coroutines.*
import java.io.ByteArrayOutputStream
import java.io.File
import java.net.InetAddress
import java.util.concurrent.atomic.AtomicReference

class ClipboardSyncService : Service(), WebSocketHandler.Listener {

    private var wsHandler: WebSocketHandler? = null
    private lateinit var clipboardManager: ClipboardManager
    private var clientId: String? = null
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    private val lastSentContent = AtomicReference<String?>(null)
    private val lastReceivedContent = AtomicReference<String?>(null)

    private var backoff = INITIAL_BACKOFF_MS
    private var connectedSince: Long = 0
    private var serverLabel: String = ""
    @Volatile private var stopping = false
    private var serverIsPrivate = false
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private val networkReady = kotlinx.coroutines.channels.Channel<Unit>(1)

    private val clipListener = ClipboardManager.OnPrimaryClipChangedListener {
        onLocalClipboardChanged()
    }

    override fun onCreate() {
        super.onCreate()
        clipboardManager = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID, buildNotification("Starting…", null),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING
            )
        } else {
            startForeground(NOTIFICATION_ID, buildNotification("Starting…", null))
        }
        clipboardManager.addPrimaryClipChangedListener(clipListener)
        scope.launch { connectLoop() }
    }

    private suspend fun connectLoop() {
        val prefs = Settings.load(this@ClipboardSyncService)
        if (prefs.server.isBlank() || prefs.user.isBlank()) {
            updateNotification("Not configured", "Open the app to set up")
            return
        }
        serverLabel = prefs.server
            .removePrefix("wss://").removePrefix("ws://")
            .removeSuffix("/sync")

        serverIsPrivate = isPrivateAddress(serverLabel.substringBefore(":"))
        if (serverIsPrivate) registerNetworkCallback()

        while (isActive()) {
            if (serverIsPrivate && !hasWifi()) {
                updateNotification("Paused", "Waiting for WiFi — $serverLabel is a LAN address")
                Log.i(TAG, "server is on a private IP, waiting for WiFi")
                networkReady.receiveCatching()
                if (!isActive()) return
                backoff = INITIAL_BACKOFF_MS
            }

            updateNotification("Connecting…", serverLabel)
            wsHandler?.close()
            wsHandler = WebSocketHandler(
                serverUrl = prefs.server,
                user = prefs.user,
                password = prefs.password,
                tlsInsecure = prefs.tlsInsecure,
                listener = this@ClipboardSyncService
            )
            wsHandler!!.connect()

            suspendCancellableCoroutine<Unit> { cont ->
                disconnectCont = cont
            }

            if (!isActive()) return
            updateNotification("Disconnected", "Retrying $serverLabel in ${backoff / 1000}s")
            delay(backoff)
            backoff = (backoff * 2).coerceAtMost(MAX_BACKOFF_MS)
        }
    }

    private fun isPrivateAddress(host: String): Boolean {
        return try {
            val addr = InetAddress.getByName(host)
            addr.isSiteLocalAddress || addr.isLoopbackAddress || addr.isLinkLocalAddress
        } catch (_: Exception) {
            false
        }
    }

    private fun hasWifi(): Boolean {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val net = cm.activeNetwork ?: return false
        val caps = cm.getNetworkCapabilities(net) ?: return false
        return caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
    }

    private fun registerNetworkCallback() {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .build()
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                Log.i(TAG, "WiFi available, resuming")
                networkReady.trySend(Unit)
            }
        }
        networkCallback = cb
        cm.registerNetworkCallback(request, cb)
    }

    private fun unregisterNetworkCallback() {
        networkCallback?.let {
            val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
            try { cm.unregisterNetworkCallback(it) } catch (_: Exception) {}
        }
        networkCallback = null
    }

    @Volatile
    private var disconnectCont: CancellableContinuation<Unit>? = null

    private fun resumeReconnect() {
        disconnectCont?.let {
            if (it.isActive) it.resumeWith(Result.success(Unit))
        }
        disconnectCont = null
    }

    private fun isActive(): Boolean = scope.isActive

    // --- WebSocketHandler.Listener ---

    override fun onConnected(welcome: Protocol.Frame.Welcome) {
        clientId = welcome.clientId
        connectedSince = System.currentTimeMillis()
        backoff = INITIAL_BACKOFF_MS
        updateNotification("Connected", serverLabel)
        Log.i(TAG, "connected as ${welcome.clientId}")

        welcome.lastClip?.let { applyInboundClip(it) }
    }

    override fun onClipReceived(clip: Protocol.Frame.Clip) {
        applyInboundClip(clip)
    }

    override fun onDisconnected(reason: String) {
        Log.w(TAG, "disconnected: $reason")
        if (System.currentTimeMillis() - connectedSince > STABLE_MS) {
            backoff = INITIAL_BACKOFF_MS
        }
        resumeReconnect()
    }

    override fun onError(error: String) {
        Log.w(TAG, "server error: $error")
    }

    // --- Clipboard handling ---

    private fun applyInboundClip(clip: Protocol.Frame.Clip) {
        if (clip.from != null && clip.from == clientId) return

        val fingerprint = clip.content ?: clip.contentB64 ?: return
        if (fingerprint == lastReceivedContent.get()) return
        lastReceivedContent.set(fingerprint)
        lastSentContent.set(fingerprint)

        when {
            clip.contentType.startsWith("text/") && clip.content != null -> {
                val clipData = ClipData.newPlainText("clipboardwire", clip.content)
                clipboardManager.setPrimaryClip(clipData)
                Log.d(TAG, "applied text clip (${clip.content.length} chars)")
            }
            clip.contentType == Protocol.IMAGE_CONTENT_TYPE && clip.contentB64 != null -> {
                val bytes = Base64.decode(clip.contentB64, Base64.DEFAULT)
                val bitmap = BitmapFactory.decodeByteArray(bytes, 0, bytes.size) ?: return
                val file = File(cacheDir, "clipboard_image.png")
                file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
                val uri = FileProvider.getUriForFile(this, "$packageName.fileprovider", file)
                val clipData = ClipData.newUri(contentResolver, "clipboardwire", uri)
                clipboardManager.setPrimaryClip(clipData)
                Log.d(TAG, "applied image clip (${bytes.size} bytes)")
            }
        }
    }

    private fun onLocalClipboardChanged() {
        val clip = clipboardManager.primaryClip ?: return
        if (clip.itemCount == 0) return
        val item = clip.getItemAt(0)
        val mime = clip.description?.getMimeType(0) ?: return

        when {
            mime.startsWith("text/") -> {
                val text = item.text?.toString() ?: return
                if (text == lastSentContent.get() || text == lastReceivedContent.get()) return
                lastSentContent.set(text)
                wsHandler?.sendText(Protocol.buildClipText(text))
                Log.d(TAG, "sent text clip (${text.length} chars)")
            }
            mime.startsWith("image/") -> {
                val uri = item.uri ?: return
                val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() } ?: return
                val bitmap = BitmapFactory.decodeByteArray(bytes, 0, bytes.size) ?: return
                val out = ByteArrayOutputStream()
                bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
                val b64 = Base64.encodeToString(out.toByteArray(), Base64.NO_WRAP)
                if (b64 == lastSentContent.get() || b64 == lastReceivedContent.get()) return
                lastSentContent.set(b64)
                wsHandler?.sendText(Protocol.buildClipImage(b64))
                Log.d(TAG, "sent image clip (${bytes.size} bytes)")
            }
        }
    }

    // --- Notification ---

    private fun buildNotification(status: String, subtitle: String? = null): Notification {
        ensureNotificationChannel()

        val openIntent = packageManager.getLaunchIntentForPackage(packageName)?.let {
            PendingIntent.getActivity(this, 0, it,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        }
        val stopIntent = Intent(this, ClipboardSyncService::class.java).apply {
            action = ACTION_STOP
        }
        val stopPending = PendingIntent.getService(
            this, 1, stopIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("clipboardwire — $status")
            .apply { if (subtitle != null) setContentText(subtitle) }
            .setSmallIcon(android.R.drawable.ic_menu_share)
            .setOngoing(true)
            .setContentIntent(openIntent)
            .addAction(android.R.drawable.ic_delete, "Stop", stopPending)
            .build()
    }

    private fun updateNotification(status: String, subtitle: String? = null) {
        if (stopping) return
        val notification = buildNotification(status, subtitle)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            try {
                startForeground(
                    NOTIFICATION_ID, notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING
                )
            } catch (_: Exception) {
                // Service may already be stopping
            }
        } else {
            val nm = getSystemService(NotificationManager::class.java)
            nm?.notify(NOTIFICATION_ID, notification)
        }
    }

    private fun ensureNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val nm = getSystemService(NotificationManager::class.java) ?: return
            val existing = nm.getNotificationChannel(CHANNEL_ID)
            if (existing == null) {
                val channel = NotificationChannel(
                    CHANNEL_ID, "Clipboard Sync",
                    NotificationManager.IMPORTANCE_DEFAULT
                ).apply {
                    setSound(null, null)
                    enableVibration(false)
                }
                nm.createNotificationChannel(channel)
            }
        }
    }

    // --- Lifecycle ---

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopping = true
            scope.cancel()
            wsHandler?.close()
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }
        return START_STICKY
    }

    override fun onDestroy() {
        stopping = true
        unregisterNetworkCallback()
        networkReady.close()
        clipboardManager.removePrimaryClipChangedListener(clipListener)
        scope.cancel()
        wsHandler?.close()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val TAG = "clipboardwire.svc"
        private const val CHANNEL_ID = "clipboardwire_sync"
        private const val NOTIFICATION_ID = 1
        private const val ACTION_STOP = "STOP_SERVICE"
        private const val INITIAL_BACKOFF_MS = 1_000L
        private const val MAX_BACKOFF_MS = 60_000L
        private const val STABLE_MS = 30_000L

        fun start(context: Context) {
            val intent = Intent(context, ClipboardSyncService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        fun stop(context: Context) {
            val intent = Intent(context, ClipboardSyncService::class.java).apply {
                action = ACTION_STOP
            }
            context.startService(intent)
        }
    }
}
