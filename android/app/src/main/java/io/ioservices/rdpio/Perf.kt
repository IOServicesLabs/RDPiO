package io.ioservices.rdpio

import android.content.Context
import android.os.Build
import android.os.PowerManager
import android.os.Process
import android.os.SystemClock
import android.view.InputDevice
import android.view.View
import java.io.File

/** Unbuffered input, the stats line's meter and the thermal frame-rate cap. */
object Perf {
    /**
     * Pointer events straight away instead of batched to the next frame
     * ([on]: a game), or batched (a desktop: a touchscreen's full sampling
     * rate is a JNI call and a packet each, for nothing anyone sees).
     */
    fun applyUnbuffered(v: View, on: Boolean) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            v.requestUnbufferedDispatch(if (on) InputDevice.SOURCE_CLASS_POINTER else 0)
        }
    }

    /** A mouse or trackpad is attached (a keyboard cover's touchpad too): the pointer wants a fast refresh. */
    fun pointerAttached(): Boolean = InputDevice.getDeviceIds().any { id ->
        val d = InputDevice.getDevice(id)
        d != null && !d.isVirtual && (d.supportsSource(InputDevice.SOURCE_MOUSE) || d.supportsSource(InputDevice.SOURCE_TOUCHPAD))
    }

    /** The app's CPU use (% of one core), wakeups and input events a second, once a second. */
    class Meter {
        private var at = SystemClock.elapsedRealtime()
        private var cpu = Process.getElapsedCpuTime()
        private var switches = -1L
        var events = 0
        var line = ""
            private set

        /** [full]: count the wakeups too (reads every thread's /proc status: only while shown). */
        fun tick(full: Boolean) {
            val now = SystemClock.elapsedRealtime()
            val dt = now - at
            if (dt < 1000) return
            val c = Process.getElapsedCpuTime()
            var l = "app cpu ${(c - cpu) * 100 / dt}%  input ${events * 1000 / dt} ev/s"
            if (full) {
                val s = contextSwitches()
                if (switches >= 0 && s >= switches) l += "  wakeups ${(s - switches) * 1000 / dt}/s"
                switches = s
            } else {
                switches = -1
            }
            line = l
            at = now
            cpu = c
            events = 0
        }

        /** Every thread's context switches (each one a sleep or a preemption) since it started. */
        private fun contextSwitches(): Long = try {
            File("/proc/self/task").listFiles()?.sumOf { t ->
                var n = 0L
                File(t, "status").forEachLine { l ->
                    if (l.contains("ctxt_switches")) n += l.substringAfter(':').trim().toLongOrNull() ?: 0
                }
                n
            } ?: 0
        } catch (e: Exception) {
            0
        }
    }

    /**
     * The frame rate to ask the PC for, given the tablet's heat: full speed
     * while cool; from "moderate" (or forecast to throttle soon) at most 90,
     * from "severe" 60 — a steady lower rate rather than the stutter of a
     * throttled CPU and GPU.
     */
    class Thermal(ctx: Context) {
        private val pm = ctx.getSystemService(PowerManager::class.java)
        private var checked = 0L
        var cap = 0
            private set
        var line = ""
            private set

        /** The new cap when it changed (0: none), else null. */
        fun poll(): Int? {
            val now = SystemClock.elapsedRealtime()
            if (now - checked < 2000) return null
            checked = now
            val status = pm.currentThermalStatus
            // (Headroom: 1.0 is where the device starts throttling, forecast 10 s ahead.)
            val headroom = pm.getThermalHeadroom(10)
            line = "thermal ${status}" + if (headroom.isNaN()) "" else " headroom ${"%.2f".format(headroom)}"
            val want = when {
                status >= PowerManager.THERMAL_STATUS_SEVERE -> 60
                status >= PowerManager.THERMAL_STATUS_MODERATE || (!headroom.isNaN() && headroom >= 0.95f) -> 90
                // (Hysteresis: come back up only once clearly cool.)
                cap != 0 && status <= PowerManager.THERMAL_STATUS_LIGHT && (headroom.isNaN() || headroom < 0.8f) -> 0
                else -> cap
            }
            if (want == cap) return null
            cap = want
            return cap
        }
    }
}
