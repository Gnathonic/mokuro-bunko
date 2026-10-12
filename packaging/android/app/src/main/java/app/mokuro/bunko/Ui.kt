package app.mokuro.bunko

import android.os.Build
import android.view.View
import android.view.WindowInsets

object Ui {
    /**
     * Pad [root] for the system bars and the keyboard. Apps targeting SDK 35+ are drawn
     * edge-to-edge; the root's background (the bar colour) shows behind the status bar.
     */
    fun applyInsets(root: View) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        root.setOnApplyWindowInsetsListener { v, insets ->
            val bars = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.ime() or WindowInsets.Type.displayCutout())
            v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            WindowInsets.CONSUMED
        }
    }

    fun statusText(activity: android.app.Activity, s: ServerService.State): String = when (s) {
        is ServerService.State.Running -> activity.getString(R.string.status_running, s.url)
        is ServerService.State.Failed -> activity.getString(R.string.status_failed, s.message)
        ServerService.State.Starting -> activity.getString(R.string.status_starting)
        ServerService.State.Stopped -> activity.getString(R.string.status_stopped)
    }
}
