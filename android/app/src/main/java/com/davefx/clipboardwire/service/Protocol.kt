// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import org.json.JSONObject
import java.util.UUID

object Protocol {
    const val TEXT_CONTENT_TYPE = "text/plain; charset=utf-8"
    const val IMAGE_CONTENT_TYPE = "image/png"

    sealed class Frame {
        data class Welcome(
            val server: String,
            val clientId: String,
            val lastClip: Clip?
        ) : Frame()

        data class Clip(
            val id: String,
            val ts: Long,
            val contentType: String,
            val content: String?,
            val contentB64: String?,
            val from: String?
        ) : Frame()

        data class Error(val code: String, val message: String) : Frame()

        data object Unknown : Frame()
    }

    fun parseFrame(json: String): Frame {
        val obj = JSONObject(json)
        return when (obj.optString("type")) {
            "welcome" -> Frame.Welcome(
                server = obj.getString("server"),
                clientId = obj.getString("client_id"),
                lastClip = obj.optJSONObject("last_clip")?.let { parseClip(it) }
            )
            "clip" -> parseClip(obj)
            "error" -> Frame.Error(
                code = obj.getString("code"),
                message = obj.getString("message")
            )
            else -> Frame.Unknown
        }
    }

    private fun parseClip(obj: JSONObject): Frame.Clip = Frame.Clip(
        id = obj.getString("id"),
        ts = obj.getLong("ts"),
        contentType = obj.getString("content_type"),
        content = obj.optString("content", null),
        contentB64 = obj.optString("content_b64", null),
        from = obj.optString("from", null)
    )

    fun buildClipText(text: String): String = JSONObject().apply {
        put("type", "clip")
        put("id", UUID.randomUUID().toString())
        put("ts", System.currentTimeMillis())
        put("content_type", TEXT_CONTENT_TYPE)
        put("content", text)
    }.toString()

    fun buildClipImage(base64Png: String): String = JSONObject().apply {
        put("type", "clip")
        put("id", UUID.randomUUID().toString())
        put("ts", System.currentTimeMillis())
        put("content_type", IMAGE_CONTENT_TYPE)
        put("content_b64", base64Png)
    }.toString()
}
