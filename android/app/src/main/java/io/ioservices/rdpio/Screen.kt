package io.ioservices.rdpio

import android.app.Activity
import android.view.Display
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/** This screen as the PC should see it: landscape pixels, refresh, scale. */
object Screen {
    /** Full-screen pixels, landscape (the stream screen is landscape only). */
    fun size(a: Activity): Pair<Int, Int> {
        val b = a.windowManager.maximumWindowMetrics.bounds
        return max(b.width(), b.height()) to min(b.width(), b.height())
    }

    /** The fastest refresh rate this display runs at its full resolution. */
    fun maxRefresh(a: Activity): Int = fastestMode(a)?.refreshRate?.roundToInt() ?: 60

    /** The display mode with the highest refresh at the current resolution. */
    fun fastestMode(a: Activity): Display.Mode? {
        val d = a.display ?: return null
        val cur = d.mode
        return d.supportedModes
            .filter { it.physicalWidth == cur.physicalWidth && it.physicalHeight == cur.physicalHeight }
            .maxByOrNull { it.refreshRate }
    }

    /**
     * For a stream at [fps]: the slowest mode at the current resolution that
     * runs a whole number of refreshes per frame (even pacing) and at least
     * [atLeast] Hz (a smooth pointer); the fastest mode when none does.
     */
    fun modeFor(a: Activity, fps: Int, atLeast: Int): Display.Mode? {
        val d = a.display ?: return null
        val cur = d.mode
        val modes = d.supportedModes.filter { it.physicalWidth == cur.physicalWidth && it.physicalHeight == cur.physicalHeight }
        val even = modes.filter {
            val n = it.refreshRate / fps
            n >= 1f && kotlin.math.abs(n - n.roundToInt()) < 0.02f
        }
        return even.filter { it.refreshRate >= atLeast - 0.5f }.minByOrNull { it.refreshRate }
            ?: even.maxByOrNull { it.refreshRate }
            ?: modes.maxByOrNull { it.refreshRate }
    }

    /** A Windows display scale that makes text about as big as this
     *  screen's own (Android's 160 dpi "dp" is roughly Windows' 100% at a
     *  laptop's distance; a tablet sits a little closer), in 25% steps. */
    fun autoScale(a: Activity): Int {
        val density = a.resources.displayMetrics.density
        val pct = (density * 100 * 0.8f / 25f).roundToInt() * 25
        return pct.coerceIn(100, 300)
    }

    /** Windows 365's display scale: the one chosen in the stream's menu, else [autoScale]. */
    fun cloudScale(a: Activity): Int =
        a.getSharedPreferences("settings", Activity.MODE_PRIVATE).getInt("cloudScale", 0).takeIf { it > 0 } ?: autoScale(a)
}
