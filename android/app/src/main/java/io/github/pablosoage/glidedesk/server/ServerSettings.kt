package io.github.pablosoage.glidedesk.server

import android.content.Context

/** What the server screen sets, kept in the app's private preferences. */
class ServerSettings(context: Context) {
    private val prefs = context.getSharedPreferences("server", Context.MODE_PRIVATE)

    enum class Way { SHIZUKU, ADB, PROJECTION }

    var way: Way
        get() = runCatching { Way.valueOf(prefs.getString("way", null) ?: "") }.getOrDefault(Way.SHIZUKU)
        set(v) = prefs.edit().putString("way", v.name).apply()

    var user: String
        get() = prefs.getString("user", null) ?: "phone"
        set(v) = prefs.edit().putString("user", v).apply()

    /** Kept so the server can start again without typing it (the app's files are private). */
    var password: String
        get() = prefs.getString("password", null) ?: ""
        set(v) = prefs.edit().putString("password", v).apply()

    var port: Int
        get() = prefs.getInt("port", 3391)
        set(v) = prefs.edit().putInt("port", v).apply()

    /** A display of its own at the client's size (Shizuku or ADB) instead of a copy of the screen. */
    var ownDisplay: Boolean
        get() = prefs.getBoolean("ownDisplay", false)
        set(v) = prefs.edit().putBoolean("ownDisplay", v).apply()

    /** Longest side of the video, in pixels. */
    var maxSize: Int
        get() = prefs.getInt("maxSize", 1920)
        set(v) = prefs.edit().putInt("maxSize", v).apply()

    var maxBitrateMbps: Int
        get() = prefs.getInt("maxBitrateMbps", 16)
        set(v) = prefs.edit().putInt("maxBitrateMbps", v).apply()
}
