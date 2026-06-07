// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import android.util.Log
import okhttp3.*
import java.util.concurrent.TimeUnit

class WebSocketHandler(
    private val serverUrl: String,
    private val user: String,
    private val password: String,
    private val tlsInsecure: Boolean,
    private val listener: Listener
) {
    interface Listener {
        fun onConnected(welcome: Protocol.Frame.Welcome)
        fun onClipReceived(clip: Protocol.Frame.Clip)
        fun onDisconnected(reason: String)
        fun onError(error: String)
    }

    private var webSocket: WebSocket? = null
    private var client: OkHttpClient? = null

    fun connect() {
        val builder = OkHttpClient.Builder()
            .protocols(listOf(okhttp3.Protocol.HTTP_1_1))
            .pingInterval(30, TimeUnit.SECONDS)
            .readTimeout(90, TimeUnit.SECONDS)

        if (tlsInsecure && serverUrl.startsWith("wss://")) {
            val trustAll = arrayOf<javax.net.ssl.TrustManager>(
                object : javax.net.ssl.X509TrustManager {
                    override fun checkClientTrusted(
                        chain: Array<java.security.cert.X509Certificate>, authType: String
                    ) {}
                    override fun checkServerTrusted(
                        chain: Array<java.security.cert.X509Certificate>, authType: String
                    ) {}
                    override fun getAcceptedIssuers(): Array<java.security.cert.X509Certificate> =
                        arrayOf()
                }
            )
            val sslContext = javax.net.ssl.SSLContext.getInstance("TLS")
            sslContext.init(null, trustAll, java.security.SecureRandom())
            builder.sslSocketFactory(
                sslContext.socketFactory,
                trustAll[0] as javax.net.ssl.X509TrustManager
            )
            builder.hostnameVerifier { _, _ -> true }
        }

        client = builder.build()
        val request = Request.Builder()
            .url(serverUrl)
            .header("Authorization", Credentials.basic(user, password))
            .build()

        webSocket = client!!.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                Log.i(TAG, "WebSocket opened")
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                when (val frame = Protocol.parseFrame(text)) {
                    is Protocol.Frame.Welcome -> {
                        Log.i(TAG, "welcome from ${frame.server}, id=${frame.clientId}")
                        listener.onConnected(frame)
                    }
                    is Protocol.Frame.Clip -> listener.onClipReceived(frame)
                    is Protocol.Frame.Error -> {
                        Log.w(TAG, "server error: ${frame.code} — ${frame.message}")
                        listener.onError("${frame.code}: ${frame.message}")
                    }
                    is Protocol.Frame.Unknown -> Log.d(TAG, "ignoring unknown frame type")
                }
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                Log.w(TAG, "WebSocket failure: ${t.message}")
                listener.onDisconnected(t.message ?: "connection failed")
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                Log.i(TAG, "WebSocket closed: $code $reason")
                listener.onDisconnected(reason.ifEmpty { "closed" })
            }
        })
    }

    fun sendText(json: String): Boolean =
        webSocket?.send(json) ?: false

    fun close() {
        webSocket?.close(1000, null)
        webSocket = null
        client?.dispatcher?.executorService?.shutdown()
        client = null
    }

    companion object {
        private const val TAG = "clipboardwire.ws"
    }
}
