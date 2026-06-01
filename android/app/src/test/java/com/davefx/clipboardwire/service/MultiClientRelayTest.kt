// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Multi-client integration tests using MockWebServer as a hub simulator.
 * Validates the full clip relay flow: welcome → send clip → receive relayed
 * clip, as the real Rust server would behave.
 */
class MultiClientRelayTest {

    private lateinit var server: MockWebServer
    private val handlers = mutableListOf<WebSocketHandler>()

    @Before
    fun setUp() {
        server = MockWebServer()
    }

    @After
    fun tearDown() {
        handlers.forEach { it.close() }
        handlers.clear()
        try { server.shutdown() } catch (_: Exception) {}
    }

    private fun welcomeJson(clientId: String, lastClip: JSONObject? = null): String {
        val obj = JSONObject()
        obj.put("type", "welcome")
        obj.put("server", "clipboardwire/0.3.0")
        obj.put("client_id", clientId)
        obj.put("last_clip", lastClip ?: JSONObject.NULL)
        return obj.toString()
    }

    private fun clipJson(content: String, from: String): String {
        val obj = JSONObject()
        obj.put("type", "clip")
        obj.put("id", "relay-${System.nanoTime()}")
        obj.put("ts", System.currentTimeMillis())
        obj.put("content_type", Protocol.TEXT_CONTENT_TYPE)
        obj.put("content", content)
        obj.put("from", from)
        return obj.toString()
    }

    private fun createHandler(listener: WebSocketHandler.Listener): WebSocketHandler {
        val h = WebSocketHandler(
            serverUrl = "ws://${server.hostName}:${server.port}/sync",
            user = "alice",
            password = "hunter2",
            tlsInsecure = false,
            listener = listener
        )
        handlers.add(h)
        return h
    }

    @Test
    fun `two clients exchange clips through simulated hub`() {
        val clientAReceived = ConcurrentLinkedQueue<String>()
        val clientBReceived = ConcurrentLinkedQueue<String>()
        val bothConnected = CountDownLatch(2)
        val clipArrived = CountDownLatch(2)

        // Hub simulator: accepts two connections, relays clips between them
        val sockets = ConcurrentLinkedQueue<WebSocket>()

        val hubListener = object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                sockets.add(webSocket)
                val id = if (sockets.size == 1) "client-a" else "client-b"
                webSocket.send(welcomeJson(id))
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                val frame = JSONObject(text)
                if (frame.optString("type") == "clip") {
                    val senderId = if (sockets.toList().firstOrNull() === webSocket) "client-a" else "client-b"
                    frame.put("from", senderId)
                    val relayed = frame.toString()
                    // Fan out to all OTHER sockets (not the sender)
                    sockets.filter { it !== webSocket }.forEach { it.send(relayed) }
                }
            }
        }

        // Enqueue two WebSocket upgrades
        server.enqueue(MockResponse().withWebSocketUpgrade(hubListener))
        server.enqueue(MockResponse().withWebSocketUpgrade(hubListener))
        server.start()

        // Client A
        val handlerA = createHandler(object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) { bothConnected.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {
                clientAReceived.add(clip.content ?: "")
                clipArrived.countDown()
            }
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        // Client B
        val handlerB = createHandler(object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) { bothConnected.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {
                clientBReceived.add(clip.content ?: "")
                clipArrived.countDown()
            }
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        handlerA.connect()
        handlerB.connect()
        assertTrue("both clients should connect", bothConnected.await(5, TimeUnit.SECONDS))

        // A sends a clip → B should receive it
        handlerA.sendText(Protocol.buildClipText("hello from A"))
        // B sends a clip → A should receive it
        handlerB.sendText(Protocol.buildClipText("hello from B"))

        assertTrue("clips should arrive", clipArrived.await(5, TimeUnit.SECONDS))
        assertTrue(clientBReceived.any { it == "hello from A" })
        assertTrue(clientAReceived.any { it == "hello from B" })
    }

    @Test
    fun `welcome with last_clip delivers cached content`() {
        val cachedClip = JSONObject().apply {
            put("type", "clip")
            put("id", "cached-1")
            put("ts", 1000L)
            put("content_type", Protocol.TEXT_CONTENT_TYPE)
            put("content", "cached clipboard value")
            put("from", "earlier-peer")
        }

        server.enqueue(MockResponse().withWebSocketUpgrade(object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                webSocket.send(welcomeJson("late-joiner", cachedClip))
            }
        }))
        server.start()

        val latch = CountDownLatch(1)
        var welcomeFrame: Protocol.Frame.Welcome? = null

        val h = createHandler(object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) {
                welcomeFrame = welcome
                latch.countDown()
            }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {}
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })
        h.connect()

        assertTrue("should receive welcome", latch.await(5, TimeUnit.SECONDS))
        assertNotNull(welcomeFrame?.lastClip)
        assertEquals("cached clipboard value", welcomeFrame?.lastClip?.content)
        assertEquals("earlier-peer", welcomeFrame?.lastClip?.from)
    }

    @Test
    fun `sender does not echo own clip when hub does not relay back`() {
        server.enqueue(MockResponse().withWebSocketUpgrade(object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                webSocket.send(welcomeJson("only-client"))
            }
            // Hub receives the clip but does NOT relay it back (no other peers)
            override fun onMessage(webSocket: WebSocket, text: String) {}
        }))
        server.start()

        val connected = CountDownLatch(1)
        val unexpectedClips = ConcurrentLinkedQueue<String>()

        val h = createHandler(object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) { connected.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {
                unexpectedClips.add(clip.content ?: "")
            }
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })
        h.connect()
        assertTrue(connected.await(5, TimeUnit.SECONDS))

        h.sendText(Protocol.buildClipText("my own text"))

        // Wait a bit and confirm nothing was echoed back
        Thread.sleep(500)
        assertTrue("sender should not receive own clip", unexpectedClips.isEmpty())
    }

    @Test
    fun `three clients fan-out — clip reaches all peers`() {
        val sockets = ConcurrentLinkedQueue<WebSocket>()
        var clientCounter = 0

        val hubListener = object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                sockets.add(webSocket)
                webSocket.send(welcomeJson("client-${++clientCounter}"))
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                val frame = JSONObject(text)
                if (frame.optString("type") == "clip") {
                    frame.put("from", "sender")
                    val relayed = frame.toString()
                    sockets.filter { it !== webSocket }.forEach { it.send(relayed) }
                }
            }
        }

        server.enqueue(MockResponse().withWebSocketUpgrade(hubListener))
        server.enqueue(MockResponse().withWebSocketUpgrade(hubListener))
        server.enqueue(MockResponse().withWebSocketUpgrade(hubListener))
        server.start()

        val allConnected = CountDownLatch(3)
        val allReceived = CountDownLatch(2) // 2 peers should get the clip
        val received = ConcurrentLinkedQueue<String>()

        val listenerA = object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) { allConnected.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {}
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        }
        val peerListener = { _: String ->
            object : WebSocketHandler.Listener {
                override fun onConnected(welcome: Protocol.Frame.Welcome) { allConnected.countDown() }
                override fun onClipReceived(clip: Protocol.Frame.Clip) {
                    received.add(clip.content ?: "")
                    allReceived.countDown()
                }
                override fun onDisconnected(reason: String) {}
                override fun onError(error: String) {}
            }
        }

        val a = createHandler(listenerA)
        val b = createHandler(peerListener("B"))
        val c = createHandler(peerListener("C"))

        a.connect()
        b.connect()
        c.connect()
        assertTrue("all three should connect", allConnected.await(5, TimeUnit.SECONDS))

        a.sendText(Protocol.buildClipText("broadcast message"))
        assertTrue("both peers should receive", allReceived.await(5, TimeUnit.SECONDS))
        assertEquals(2, received.size)
        assertTrue(received.all { it == "broadcast message" })
    }

    @Test
    fun `server error frame is reported via onError`() {
        val errorJson = JSONObject().apply {
            put("type", "error")
            put("code", "bad_frame")
            put("message", "clip frame has both content and content_b64")
        }.toString()

        server.enqueue(MockResponse().withWebSocketUpgrade(object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                webSocket.send(welcomeJson("err-client"))
            }
            override fun onMessage(webSocket: WebSocket, text: String) {
                webSocket.send(errorJson)
            }
        }))
        server.start()

        val connected = CountDownLatch(1)
        val errorReceived = CountDownLatch(1)
        var errorMsg: String? = null

        val h = createHandler(object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) { connected.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {}
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {
                errorMsg = error
                errorReceived.countDown()
            }
        })
        h.connect()
        assertTrue(connected.await(5, TimeUnit.SECONDS))

        h.sendText(Protocol.buildClipText("bad"))
        assertTrue("error should arrive", errorReceived.await(5, TimeUnit.SECONDS))
        assertTrue(errorMsg!!.contains("bad_frame"))
    }

    @Test
    fun `image clip with base64 content is relayed correctly`() {
        val sockets = ConcurrentLinkedQueue<WebSocket>()
        val hubListener = object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                sockets.add(webSocket)
                val id = if (sockets.size == 1) "img-sender" else "img-receiver"
                webSocket.send(welcomeJson(id))
            }
            override fun onMessage(webSocket: WebSocket, text: String) {
                val frame = JSONObject(text)
                if (frame.optString("type") == "clip") {
                    frame.put("from", "img-sender")
                    sockets.filter { it !== webSocket }.forEach { it.send(frame.toString()) }
                }
            }
        }
        server.enqueue(MockResponse().withWebSocketUpgrade(hubListener))
        server.enqueue(MockResponse().withWebSocketUpgrade(hubListener))
        server.start()

        val connectedBoth = CountDownLatch(2)
        val imageReceived = CountDownLatch(1)
        var receivedB64: String? = null
        var receivedContentType: String? = null

        val senderH = createHandler(object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) { connectedBoth.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {}
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        val receiverH = createHandler(object : WebSocketHandler.Listener {
            override fun onConnected(welcome: Protocol.Frame.Welcome) { connectedBoth.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {
                receivedB64 = clip.contentB64
                receivedContentType = clip.contentType
                imageReceived.countDown()
            }
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        senderH.connect()
        receiverH.connect()
        assertTrue(connectedBoth.await(5, TimeUnit.SECONDS))

        val tinyPngB64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
        senderH.sendText(Protocol.buildClipImage(tinyPngB64))

        assertTrue("image clip should arrive", imageReceived.await(5, TimeUnit.SECONDS))
        assertEquals(tinyPngB64, receivedB64)
        assertEquals(Protocol.IMAGE_CONTENT_TYPE, receivedContentType)
    }
}
