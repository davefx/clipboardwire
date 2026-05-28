// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire

import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class MainActivityTest {

    @get:Rule
    val composeRule = createAndroidComposeRule<MainActivity>()

    @Test
    fun settings_screen_shows_all_fields() {
        composeRule.onNodeWithText("clipboardwire").assertIsDisplayed()
        composeRule.onNodeWithText("Server URL").assertIsDisplayed()
        composeRule.onNodeWithText("Username").assertIsDisplayed()
        composeRule.onNodeWithText("Password").assertIsDisplayed()
        composeRule.onNodeWithText("Skip TLS verification (LAN/VPN only)").assertIsDisplayed()
        composeRule.onNodeWithText("Save & Connect").assertIsDisplayed()
        composeRule.onNodeWithText("Stop service").assertIsDisplayed()
    }

    @Test
    fun server_url_field_accepts_input() {
        val field = composeRule.onNodeWithText("Server URL")
        field.performClick()
        field.performTextClearance()
        field.performTextInput("wss://example:8484/sync")
        field.assertTextContains("wss://example:8484/sync")
    }

    @Test
    fun username_field_accepts_input() {
        val field = composeRule.onNodeWithText("Username")
        field.performClick()
        field.performTextClearance()
        field.performTextInput("bob")
        field.assertTextContains("bob")
    }

    @Test
    fun can_toggle_tls_checkbox() {
        composeRule.onNodeWithText("Skip TLS verification (LAN/VPN only)")
            .performClick()
    }
}
