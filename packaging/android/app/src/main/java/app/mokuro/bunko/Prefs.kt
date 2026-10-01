package app.mokuro.bunko

import android.content.Context
import java.io.File

/** The settings the native screen owns; everything else is in the server's admin UI. */
class Prefs(context: Context) {
    private val app = context.applicationContext
    private val sp = app.getSharedPreferences("bunko", Context.MODE_PRIVATE)

    var port: Int
        get() = sp.getInt("port", DEFAULT_PORT)
        set(v) = sp.edit().putInt("port", v).apply()

    var lan: Boolean
        get() = sp.getBoolean("lan", false)
        set(v) = sp.edit().putBoolean("lan", v).apply()

    /** Index into [storageVolumes]: 0 = primary shared storage, 1+ = SD cards. */
    var storageIndex: Int
        get() = sp.getInt("storage_index", 0)
        set(v) = sp.edit().putInt("storage_index", v).apply()

    /** Start the server whenever the app opens (cleared by an explicit Stop). */
    var autoStart: Boolean
        get() = sp.getBoolean("auto_start", true)
        set(v) = sp.edit().putBoolean("auto_start", v).apply()

    /** The app's external files directories that are mounted, primary first. */
    fun storageVolumes(): List<File> = app.getExternalFilesDirs(null).filterNotNull()

    /** `storage.base_path`: `<external files dir>/bunko`, else internal storage. */
    fun storageDir(): File {
        val vols = storageVolumes()
        val base = vols.getOrNull(storageIndex) ?: vols.firstOrNull() ?: app.filesDir
        return File(base, "bunko")
    }

    /** Internal, so it survives switching storage volumes. */
    fun configFile(): File = File(app.filesDir, "config.yaml")

    companion object {
        const val DEFAULT_PORT = 8080
    }
}
