// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.davefx.clipboardwire.service.Protocol
import com.davefx.clipboardwire.service.WebSocketHandler
import org.junit.After
import org.junit.Assert.*
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * Integration tests against a real clipboardwire Rust server.
 *
 * Skipped unless the `CLIPBOARDWIRE_TEST_SERVER` env var / system property
 * is set (the CI job sets it to `ws://10.0.2.2:<port>/sync` where the
 * emulator can reach the host-side server).
 */
@RunWith(AndroidJUnit4::class)
class ServerIntegrationTest {

    private var serverUrl: String? = null
    private val handlers = mutableListOf<WebSocketHandler>()

    @Before
    fun setUp() {
        val args = InstrumentationRegistry.getArguments()
        serverUrl = args.getString("testServerUrl")
        assumeTrue(
            "Skipped — no testServerUrl instrumentation arg set",
            !serverUrl.isNullOrBlank()
        )
    }

    @After
    fun tearDown() {
        handlers.forEach { it.close() }
        handlers.clear()
    }

    private fun connect(listener: WebSocketHandler.Listener): WebSocketHandler {
        val h = WebSocketHandler(
            serverUrl = serverUrl!!,
            user = "testuser",
            password = "testpass",
            tlsInsecure = false,
            listener = listener
        )
        handlers.add(h)
        h.connect()
        return h
    }

    @Test
    fun connects_and_receives_welcome_from_real_server() {
        val latch = CountDownLatch(1)
        var welcome: Protocol.Frame.Welcome? = null

        connect(object : WebSocketHandler.Listener {
            override fun onConnected(w: Protocol.Frame.Welcome) {
                welcome = w
                latch.countDown()
            }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {}
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        assertTrue("should connect within 10s", latch.await(10, TimeUnit.SECONDS))
        assertNotNull(welcome)
        assertTrue(welcome!!.server.startsWith("clipboardwire/"))
        assertFalse(welcome!!.clientId.isBlank())
    }

    @Test
    fun two_clients_relay_clip_through_real_server() {
        val connectedA = CountDownLatch(1)
        val connectedB = CountDownLatch(1)
        val clipReceivedByB = CountDownLatch(1)
        var relayedContent: String? = null

        connect(object : WebSocketHandler.Listener {
            override fun onConnected(w: Protocol.Frame.Welcome) { connectedA.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {}
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        connect(object : WebSocketHandler.Listener {
            override fun onConnected(w: Protocol.Frame.Welcome) { connectedB.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {
                relayedContent = clip.content
                clipReceivedByB.countDown()
            }
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        assertTrue("A should connect", connectedA.await(10, TimeUnit.SECONDS))
        assertTrue("B should connect", connectedB.await(10, TimeUnit.SECONDS))

        // A sends a clip; B should receive it
        val testText = "integration-test-${System.currentTimeMillis()}"
        handlers[0].sendText(Protocol.buildClipText(testText))

        assertTrue("B should receive clip from A", clipReceivedByB.await(5, TimeUnit.SECONDS))
        assertEquals(testText, relayedContent)
    }

    @Test
    fun clip_fans_out_to_multiple_peers() {
        val allConnected = CountDownLatch(3)
        val clipsReceived = CountDownLatch(2)
        val received = ConcurrentLinkedQueue<String>()

        // Client A — sender
        connect(object : WebSocketHandler.Listener {
            override fun onConnected(w: Protocol.Frame.Welcome) { allConnected.countDown() }
            override fun onClipReceived(clip: Protocol.Frame.Clip) {}
            override fun onDisconnected(reason: String) {}
            override fun onError(error: String) {}
        })

        // Clients B and C — receivers
        repeat(2) {
            connect(object : WebSocketHandler.Listener {
                override fun onConnected(w: Protocol.Frame.Welcome) { allConnected.countDown() }
                override fun onClipReceived(clip: Protocol.Frame.Clip) {
                    received.add(clip.content ?: "")
                    clipsReceived.countDown()
                }
                override fun onDisconnected(reason: String) {}
                override fun onError(error: String) {}
            })
        }

        assertTrue("all should connect", allConnected.await(10, TimeUnit.SECONDS))

        val testText = "fanout-${System.currentTimeMillis()}"
        handlers[0].sendText(Protocol.buildClipText(testText))

        assertTrue("both peers should receive", clipsReceived.await(5, TimeUnit.SECONDS))
        assertEquals(2, received.size)
        assertTrue(received.all { it == testText })
    }

    @Test
    fun wrong_password_triggers_disconnect() {
        val disconnected = CountDownLatch(1)
        var reason: String? = null

        val h = WebSocketHandler(
            serverUrl = serverUrl!!,
            user = "testuser",
            password = "wrongpassword",
            tlsInsecure = false,
            listener = object : WebSocketHandler.Listener {
                override fun onConnected(w: Protocol.Frame.Welcome) {}
                override fun onClipReceived(clip: Protocol.Frame.Clip) {}
                override fun onDisconnected(r: String) {
                    reason = r
                    disconnected.countDown()
                }
                override fun onError(error: String) {}
            }
        )
        handlers.add(h)
        h.connect()

        assertTrue("should get disconnected", disconnected.await(10, TimeUnit.SECONDS))
        assertNotNull(reason)
    }
}
