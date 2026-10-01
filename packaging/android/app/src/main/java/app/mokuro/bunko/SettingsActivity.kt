package app.mokuro.bunko

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.os.Bundle
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.text.format.Formatter
import android.view.View
import android.widget.Button
import android.widget.EditText
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import java.io.File

/**
 * The few settings that must exist before the server runs (port, LAN access, storage),
 * start/stop, the addresses to give Mokuro Reader, and the log. Everything else is in
 * the server's admin panel inside the WebView.
 */
class SettingsActivity : Activity() {
    private lateinit var prefs: Prefs
    private val handler = Handler(Looper.getMainLooper())
    private val onState: () -> Unit = { render() }
    private val logTicker = object : Runnable {
        override fun run() {
            refreshLog()
            handler.postDelayed(this, 2000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_settings)
        Ui.applyInsets(findViewById(R.id.root))
        prefs = Prefs(this)

        findViewById<Button>(R.id.back).setOnClickListener { finish() }
        findViewById<EditText>(R.id.port).setText(prefs.port.toString())
        findViewById<Switch>(R.id.lan).apply {
            isChecked = prefs.lan
            setOnCheckedChangeListener { _, _ -> renderAddresses() }
        }
        buildStorageChoices()

        findViewById<Button>(R.id.start_stop).setOnClickListener {
            if (ServerService.state is ServerService.State.Running || ServerService.state is ServerService.State.Starting) {
                ServerService.stop(this)
            } else if (save()) {
                ServerService.start(this)
            }
        }
        findViewById<Button>(R.id.apply).setOnClickListener {
            if (save()) ServerService.restart(this)
        }
        findViewById<Button>(R.id.copy).setOnClickListener {
            val text = addresses().joinToString("\n")
            getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("Mokuro Bunko", text))
            Toast.makeText(this, R.string.copied, Toast.LENGTH_SHORT).show()
        }
        findViewById<Button>(R.id.battery_open).setOnClickListener {
            // The list screen needs no special permission (unlike the direct request).
            runCatching { startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)) }
        }
        findViewById<Button>(R.id.licences).setOnClickListener {
            startActivity(Intent(this, LicensesActivity::class.java))
        }
    }

    override fun onStart() {
        super.onStart()
        ServerService.listeners.add(onState)
        render()
        handler.post(logTicker)
    }

    override fun onStop() {
        handler.removeCallbacks(logTicker)
        ServerService.listeners.remove(onState)
        super.onStop()
    }

    /** Validate and store the form; false (with a message) if the port is invalid. */
    private fun save(): Boolean {
        val portField = findViewById<EditText>(R.id.port)
        val port = portField.text.toString().trim().toIntOrNull()
        // Below 1024 needs root on Android.
        if (port == null || port !in 1024..65535) {
            portField.error = getString(R.string.invalid_port)
            return false
        }
        prefs.port = port
        prefs.lan = findViewById<Switch>(R.id.lan).isChecked
        val group = findViewById<RadioGroup>(R.id.storage_group)
        val checked = group.checkedRadioButtonId
        if (checked != View.NO_ID) prefs.storageIndex = group.indexOfChild(group.findViewById(checked))
        renderStoragePath()
        return true
    }

    private fun buildStorageChoices() {
        val group = findViewById<RadioGroup>(R.id.storage_group)
        group.removeAllViews()
        prefs.storageVolumes().forEachIndexed { i, dir ->
            val removable = runCatching { Environment.isExternalStorageRemovable(dir) }.getOrDefault(false)
            val free = Formatter.formatShortFileSize(this, dir.usableSpace)
            val label = (if (removable) "SD card" else "Phone storage") + " — $free free"
            val rb = RadioButton(this).apply {
                id = View.generateViewId()
                text = label
            }
            group.addView(rb)
            if (i == prefs.storageIndex) rb.isChecked = true
        }
        group.setOnCheckedChangeListener { _, _ -> renderStoragePath() }
        renderStoragePath()
    }

    private fun selectedStorageDir(): File {
        val group = findViewById<RadioGroup>(R.id.storage_group)
        val checked = group.checkedRadioButtonId
        val idx = if (checked == View.NO_ID) prefs.storageIndex else group.indexOfChild(group.findViewById(checked))
        val base = prefs.storageVolumes().getOrNull(idx) ?: return prefs.storageDir()
        return File(base, "bunko")
    }

    private fun renderStoragePath() {
        findViewById<TextView>(R.id.library_path).text = File(selectedStorageDir(), "library").absolutePath
    }

    private fun addresses(): List<String> {
        val port = findViewById<EditText>(R.id.port).text.toString().toIntOrNull() ?: prefs.port
        val local = Net.url("127.0.0.1", port)
        val lan = if (findViewById<Switch>(R.id.lan).isChecked) Net.lanAddresses().map { Net.url(it, port) } else emptyList()
        return listOf(local) + lan
    }

    private fun renderAddresses() {
        val all = addresses()
        findViewById<TextView>(R.id.local_url).text = all.first()
        val lan = all.drop(1)
        val lanOn = findViewById<Switch>(R.id.lan).isChecked
        findViewById<TextView>(R.id.lan_label).setText(if (lanOn) R.string.connect_lan else R.string.connect_lan_off)
        findViewById<TextView>(R.id.lan_urls).apply {
            visibility = if (lanOn) View.VISIBLE else View.GONE
            text = if (lan.isEmpty()) "—" else lan.joinToString("\n")
        }
    }

    private fun render() {
        val s = ServerService.state
        findViewById<TextView>(R.id.status).text = Ui.statusText(this, s)
        val busy = s is ServerService.State.Running || s is ServerService.State.Starting
        findViewById<Button>(R.id.start_stop).setText(if (busy) R.string.stop else R.string.start)
        renderAddresses()
        val exempt = getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(packageName)
        findViewById<TextView>(R.id.battery_status).setText(if (exempt) R.string.battery_exempt else R.string.battery_optimised)
        refreshLog()
    }

    private fun refreshLog() {
        val tail = BunkoNative.logTail().lines().takeLast(80).joinToString("\n")
        findViewById<TextView>(R.id.log).text = tail
    }
}
