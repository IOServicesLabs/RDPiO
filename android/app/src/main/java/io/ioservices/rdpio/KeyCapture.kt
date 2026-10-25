package io.ioservices.rdpio

import android.accessibilityservice.AccessibilityService
import android.content.Intent
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent

/**
 * The keys Android keeps for itself (Alt+Tab, the Windows key and its
 * shortcuts, Alt+F4) for the PC: an accessibility service sees a keyboard's
 * keys before the system's shortcuts do, and while a stream is in front it
 * hands them to the stream instead.
 *
 * Off until turned on in Android's accessibility settings. It reads nothing
 * else: no window content, no events, and every key when no stream is in
 * front goes on as usual.
 */
class KeyCapture : AccessibilityService() {
    /** Keys whose press went to the stream: their release goes there too. */
    private val taken = HashSet<Int>()

    override fun onServiceConnected() {
        running = this
    }

    override fun onUnbind(intent: Intent?): Boolean {
        running = null
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        running = null
        super.onDestroy()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    // (Called on the main thread, as the stream's own key handling is.)
    override fun onKeyEvent(e: KeyEvent): Boolean {
        if (e.action == KeyEvent.ACTION_UP && taken.remove(e.keyCode)) {
            stream?.captured(e)
            return true
        }
        val s = stream ?: return false
        if (e.action != KeyEvent.ACTION_DOWN || !s.capturing(e) || !s.captured(e)) return false
        taken.add(e.keyCode)
        return true
    }

    companion object {
        @Volatile
        var running: KeyCapture? = null

        /** The stream in front (StreamActivity, while it has focus). */
        @Volatile
        var stream: StreamActivity? = null

        val enabled get() = running != null
    }
}
