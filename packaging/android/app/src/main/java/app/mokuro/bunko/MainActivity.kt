package app.mokuro.bunko

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.view.View
import android.webkit.ValueCallback
import android.webkit.WebChromeClient
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import android.widget.Button
import android.widget.TextView
import android.widget.Toast
import android.window.OnBackInvokedCallback
import android.window.OnBackInvokedDispatcher

/** The server's own web UI (setup wizard, catalog, admin) in a WebView. */
class MainActivity : Activity() {
    private lateinit var web: WebView
    private lateinit var placeholder: TextView
    private lateinit var status: TextView
    private var loadedUrl: String? = null
    private var fileCallback: ValueCallback<Array<Uri>>? = null
    private var backCallback: Any? = null
    private val onState: () -> Unit = { render() }

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        Ui.applyInsets(findViewById(R.id.root))
        web = findViewById(R.id.web)
        placeholder = findViewById(R.id.placeholder)
        status = findViewById(R.id.status)
        findViewById<Button>(R.id.open_settings).setOnClickListener {
            startActivity(Intent(this, SettingsActivity::class.java))
        }
        findViewById<Button>(R.id.reload).setOnClickListener {
            if (ServerService.state is ServerService.State.Running) web.reload() else ServerService.start(this)
        }

        web.settings.apply {
            javaScriptEnabled = true
            domStorageEnabled = true
            allowFileAccess = false
            allowContentAccess = false
        }
        web.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean {
                val host = request.url.host
                if (host == "127.0.0.1" || host == "localhost") return false
                // Anything else (Mokuro Reader, GitHub, docs) opens in the browser.
                runCatching { startActivity(Intent(Intent.ACTION_VIEW, request.url)) }
                return true
            }

            override fun doUpdateVisitedHistory(view: WebView, url: String?, isReload: Boolean) {
                updateBack()
            }
        }
        web.webChromeClient = object : WebChromeClient() {
            // <input type="file"> in the upload pages.
            override fun onShowFileChooser(view: WebView, callback: ValueCallback<Array<Uri>>, params: FileChooserParams): Boolean {
                fileCallback?.onReceiveValue(null)
                fileCallback = callback
                return try {
                    startActivityForResult(params.createIntent(), REQUEST_FILES)
                    true
                } catch (_: Exception) {
                    fileCallback = null
                    false
                }
            }
        }
        if (savedInstanceState != null) web.restoreState(savedInstanceState)

        askForNotifications()
        if (Prefs(this).autoStart && !BunkoNative.isRunning()) ServerService.start(this)
    }

    override fun onStart() {
        super.onStart()
        ServerService.listeners.add(onState)
        render()
    }

    override fun onStop() {
        ServerService.listeners.remove(onState)
        super.onStop()
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        web.saveState(outState)
    }

    private fun render() {
        val s = ServerService.state
        status.text = Ui.statusText(this, s)
        if (s is ServerService.State.Running) {
            placeholder.visibility = View.GONE
            web.visibility = View.VISIBLE
            // (Re)load when the server (port) changed, not on every state callback.
            if (loadedUrl != s.url) {
                loadedUrl = s.url
                web.loadUrl(s.url)
            }
        } else {
            placeholder.visibility = View.VISIBLE
            placeholder.text = when (s) {
                is ServerService.State.Failed -> getString(R.string.status_failed, s.message)
                ServerService.State.Starting -> getString(R.string.status_starting)
                else -> getString(R.string.not_running_hint)
            }
            if (s !is ServerService.State.Starting) loadedUrl = null
        }
    }

    /** Back goes back in the WebView while it can, otherwise leaves (predictive back). */
    private fun updateBack() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val want = web.canGoBack()
        val have = backCallback != null
        if (want && !have) {
            val cb = OnBackInvokedCallback { if (web.canGoBack()) web.goBack(); updateBack() }
            onBackInvokedDispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT, cb)
            backCallback = cb
        } else if (!want && have) {
            onBackInvokedDispatcher.unregisterOnBackInvokedCallback(backCallback as OnBackInvokedCallback)
            backCallback = null
        }
    }

    @Deprecated("Used below API 33 only")
    override fun onBackPressed() {
        if (web.canGoBack()) web.goBack() else @Suppress("DEPRECATION") super.onBackPressed()
    }

    @Deprecated("Platform API without AndroidX")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (requestCode == REQUEST_FILES) {
            fileCallback?.onReceiveValue(WebChromeClient.FileChooserParams.parseResult(resultCode, data))
            fileCallback = null
            return
        }
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
    }

    private fun askForNotifications() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFICATIONS)
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (requestCode == REQUEST_NOTIFICATIONS && grantResults.firstOrNull() == PackageManager.PERMISSION_DENIED) {
            Toast.makeText(this, R.string.notifications_denied, Toast.LENGTH_LONG).show()
        }
    }

    override fun onDestroy() {
        web.destroy()
        super.onDestroy()
    }

    companion object {
        private const val REQUEST_FILES = 1
        private const val REQUEST_NOTIFICATIONS = 2
    }
}
