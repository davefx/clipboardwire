// SPDX-License-Identifier: GPL-3.0-or-later
package com.davefx.clipboardwire.service

import android.content.Context
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

private val Context.dataStore by preferencesDataStore(name = "settings")

data class Settings(
    val server: String = "",
    val user: String = "",
    val password: String = "",
    val tlsInsecure: Boolean = false
) {
    val isConfigured: Boolean get() = server.isNotBlank() && user.isNotBlank()

    companion object {
        private val KEY_SERVER = stringPreferencesKey("server")
        private val KEY_USER = stringPreferencesKey("user")
        private val KEY_PASSWORD = stringPreferencesKey("password")
        private val KEY_TLS_INSECURE = booleanPreferencesKey("tls_insecure")

        suspend fun load(context: Context): Settings =
            context.dataStore.data.map { prefs ->
                Settings(
                    server = prefs[KEY_SERVER] ?: "",
                    user = prefs[KEY_USER] ?: "",
                    password = prefs[KEY_PASSWORD] ?: "",
                    tlsInsecure = prefs[KEY_TLS_INSECURE] ?: false
                )
            }.first()

        suspend fun save(context: Context, settings: Settings) {
            context.dataStore.edit { prefs ->
                prefs[KEY_SERVER] = settings.server
                prefs[KEY_USER] = settings.user
                prefs[KEY_PASSWORD] = settings.password
                prefs[KEY_TLS_INSECURE] = settings.tlsInsecure
            }
        }
    }
}
