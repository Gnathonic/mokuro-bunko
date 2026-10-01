package app.mokuro.bunko

import android.app.Application
import android.system.Os

class BunkoApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // The server resolves ~ and XDG paths (TLS auto-certificates, defaults) and temp
        // files from the environment; an app process has no usable HOME or TMPDIR. Set
        // them before the native library is loaded.
        val home = filesDir.absolutePath
        Os.setenv("HOME", home, true)
        Os.setenv("XDG_DATA_HOME", home, true)
        Os.setenv("XDG_CONFIG_HOME", home, true)
        Os.setenv("TMPDIR", cacheDir.absolutePath, true)
    }
}
