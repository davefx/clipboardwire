// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.intPreferencesKey
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

private val Context.dataStore by preferencesDataStore(name = "settings")

data class Settings(
    val server: String = "",
    val user: String = "",
    val password: String = "",
    val tlsInsecure: Boolean = false,
    val serverMode: Boolean = false,
    val serverPort: Int = 8484,
    val serverUser: String = "",
    val serverPassword: String = ""
) {
    val isConfigured: Boolean
        get() = if (serverMode) {
            serverUser.isNotBlank() && serverPassword.isNotBlank()
        } else {
            server.isNotBlank() && user.isNotBlank()
        }

    companion object {
        private val KEY_SERVER = stringPreferencesKey("server")
        private val KEY_USER = stringPreferencesKey("user")
        private val KEY_PASSWORD = stringPreferencesKey("password")
        private val KEY_TLS_INSECURE = booleanPreferencesKey("tls_insecure")
        private val KEY_SERVER_MODE = booleanPreferencesKey("server_mode")
        private val KEY_SERVER_PORT = intPreferencesKey("server_port")
        private val KEY_SERVER_USER = stringPreferencesKey("server_user")
        private val KEY_SERVER_PASSWORD = stringPreferencesKey("server_password")

        suspend fun load(context: Context): Settings =
            context.dataStore.data.map { prefs ->
                Settings(
                    server = prefs[KEY_SERVER] ?: "",
                    user = prefs[KEY_USER] ?: "",
                    password = prefs[KEY_PASSWORD] ?: "",
                    tlsInsecure = prefs[KEY_TLS_INSECURE] ?: false,
                    serverMode = prefs[KEY_SERVER_MODE] ?: false,
                    serverPort = prefs[KEY_SERVER_PORT] ?: 8484,
                    serverUser = prefs[KEY_SERVER_USER] ?: "",
                    serverPassword = prefs[KEY_SERVER_PASSWORD] ?: ""
                )
            }.first()

        suspend fun save(context: Context, settings: Settings) {
            context.dataStore.edit { prefs ->
                prefs[KEY_SERVER] = settings.server
                prefs[KEY_USER] = settings.user
                prefs[KEY_PASSWORD] = settings.password
                prefs[KEY_TLS_INSECURE] = settings.tlsInsecure
                prefs[KEY_SERVER_MODE] = settings.serverMode
                prefs[KEY_SERVER_PORT] = settings.serverPort
                prefs[KEY_SERVER_USER] = settings.serverUser
                prefs[KEY_SERVER_PASSWORD] = settings.serverPassword
            }
        }
    }
}
