package app.mokuro.bunko

/**
 * The Rust server (crates/bunko-android, libbunko_android.so). `start` and `stop` block
 * (startup / graceful shutdown), so call them off the main thread.
 */
object BunkoNative {
    init {
        System.loadLibrary("bunko_android")
    }

    /** Returns "ok:<local url>" or "error:<message>". */
    @JvmStatic external fun start(storageDir: String, configPath: String, port: Int, lan: Boolean): String

    /** Returns whether a server was running. */
    @JvmStatic external fun stop(): Boolean

    @JvmStatic external fun isRunning(): Boolean

    /** The last ~300 log lines. */
    @JvmStatic external fun logTail(): String

    /** Why the server last failed, or "". */
    @JvmStatic external fun lastError(): String
}
