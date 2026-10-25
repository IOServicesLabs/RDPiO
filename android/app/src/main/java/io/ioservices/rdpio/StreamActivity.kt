package io.ioservices.rdpio

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.hardware.input.InputManager
import android.net.wifi.WifiManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.MessageQueue
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.system.Os
import android.text.InputType
import android.util.Log
import android.view.Gravity
import android.view.KeyCharacterMap
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import android.widget.TextView
import android.widget.Toast
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.math.min
import kotlin.math.roundToInt

/** The stream, full screen: the picture, the input, a small menu. */
class StreamActivity : Activity() {
    private lateinit var root: Root
    private lateinit var video: SurfaceView
    private lateinit var stats: TextView
    private lateinit var banner: TextView
    private lateinit var menu: StreamMenu
    private lateinit var input: StreamInput
    private val ui = Handler(Looper.getMainLooper())

    private var fps = 60
    private var streamW = 0
    private var streamH = 0
    private var cursorGen = -1L
    private var clipGen = -1L
    private var icon: PointerIcon? = null
    private var showStats = false
    private var mouseLocked = false
    private var keyboardShown = false
    private var lastClipFromHost: String? = null
    private var lastClipSent: String? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var bannerUntil = 0L
    private var ended = false
    /** Keyboard in the tablet's layout (characters) instead of the PC's (keys). */
    private var keysAsText = false
    /** Keys typed as characters: their releases aren't keys for the PC. */
    private val typedKeys = HashSet<Int>()
    private var deadAccent = 0
    private lateinit var cameraFeed: CameraFeed
    /** The mic/camera permissions were asked for (once a stream: no nagging). */
    private var askedPermissions = false
    /** System shortcuts to the PC (when [KeyCapture] is turned on). */
    private var captureKeys = true
    /** CPU and input events a second, for the stats line. */
    private val meter = Perf.Meter()
    /** Between onResume and onPause. */
    private var resumed = false
    /** On screen at all (between onStart and onStop): in the background the poll only watches for the end. */
    private var visible = true
    /** Last touch, pointer or key ([SystemClock.uptimeMillis]): the poll runs fast for a while after. */
    private var lastInputAt = 0L
    /** When a mouse or trackpad last moved, and whether the display is up at the pointer's rate for it. */
    private var lastPointerAt = 0L
    private var pointerFast = false
    /** The core's news, watched by this thread's looper ([watchNotify]); null: polling only. */
    private var notifyFd: ParcelFileDescriptor? = null
    /** News came at least once (the core does signal): only then may the poll slow down. */
    private var newsCame = false
    /** A finger (or button) is down: the poll at its fastest for a held finger's right click. */
    private var fingerDown = false
    /** The view size last told to the PC. */
    private var viewTold = 0 to 0
    /** Game mode's resolution, % of the screen: the PC sends fewer pixels, the display scales them up for free. [AUTO]: Auto. */
    private var gameScale = AUTO
    /** Office mode's resolution, the same way (a weak connection: far fewer bits for the same text). [AUTO]: Auto. */
    private var officeScale = AUTO
    /**
     * Auto's current pick per mode: full size at the start, then what the
     * connection carries (the core decides from the bits that arrive; see
     * autores.rs). Not kept across sessions: a slow link's pick would hold a
     * LAN back.
     */
    private var autoGame = 100
    private var autoOffice = 100
    /** Upscale a smaller stream with Snapdragon GSR (else the display's own scaler). */
    private var upscaleOn = true
    /** The GPU couldn't (logged): the display scales from then on. */
    private var upscaleFailed = false
    private var upscaler: Upscaler? = null
    /** The size last set on the stream's Surface buffers. */
    private var bufferSize = 0 to 0
    private lateinit var thermal: Perf.Thermal
    private val clipListener = ClipboardManager.OnPrimaryClipChangedListener { sendClipboard() }
    /** A mouse or trackpad came or went: the refresh rate follows (see [displayMode]). */
    private val devices = object : InputManager.InputDeviceListener {
        override fun onInputDeviceAdded(id: Int) = applyDisplay()
        override fun onInputDeviceRemoved(id: Int) = applyDisplay()
        override fun onInputDeviceChanged(id: Int) {}
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Native.setDataDir(filesDir.path)
        fps = intent.getIntExtra("fps", 60)
        thermal = Perf.Thermal(this)
        keysAsText = getSharedPreferences("settings", MODE_PRIVATE).getBoolean("keysAsText", false)
        captureKeys = getSharedPreferences("settings", MODE_PRIVATE).getBoolean("captureKeys", true)
        // (Auto unless a size was picked.)
        gameScale = getSharedPreferences("settings", MODE_PRIVATE).getInt("gameScale", AUTO)
        officeScale = getSharedPreferences("settings", MODE_PRIVATE).getInt("officeScale", AUTO)
        upscaleOn = getSharedPreferences("settings", MODE_PRIVATE).getBoolean("upscale", true)
        cameraFeed = CameraFeed(this)
        Native.setCameraCount(cameraFeed.count())
        Native.setPermissions(granted(Manifest.permission.RECORD_AUDIO), granted(Manifest.permission.CAMERA))
        input = StreamInput(resources.displayMetrics.density)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setDecorFitsSystemWindows(false)
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
            // The display at its fastest refresh: a 144 Hz stream on a 144 Hz panel.
            displayMode()?.let { preferredDisplayModeId = it.modeId }
        }

        root = Root(this)
        video = SurfaceView(this)
        video.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(h: SurfaceHolder) = attach(h.surface)
            // (Same Surface, new size: the decoder keeps drawing into it.)
            override fun surfaceChanged(h: SurfaceHolder, format: Int, w: Int, ht: Int) {}
            override fun surfaceDestroyed(h: SurfaceHolder) = detach()
        })
        root.addView(video, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))

        stats = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 12f
            typeface = android.graphics.Typeface.MONOSPACE
            setBackgroundColor(0xB0000000.toInt())
            setPadding(dp(10), dp(6), dp(10), dp(6))
            visibility = View.GONE
        }
        root.addView(stats, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.START).apply {
            setMargins(dp(12), dp(12), 0, 0)
        })
        banner = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 15f
            gravity = Gravity.CENTER
            background = GradientDrawable().apply { setColor(0xD0101212.toInt()); cornerRadius = dp(6).toFloat() }
            setPadding(dp(18), dp(12), dp(18), dp(12))
            visibility = View.GONE
        }
        root.addView(banner, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.CENTER))

        // The menu: a round button over the picture (drag it anywhere); tapped, the actions beside it.
        fun onOff(on: Boolean) = if (on) "On" else "Off"
        menu = StreamMenu(root).apply {
            section("Input")
            item("Keyboard", { onOff(keyboardShown) }) { toggleKeyboard() }
            item("Lock mouse", { onOff(mouseLocked) }, keepOpen = false) { setMouseLocked(!mouseLocked) }
            item("Fingers", { if (input.fingersAsTouch && input.hostTouch) "Touch" else "Mouse" }) {
                input.fingersAsTouch = !input.fingersAsTouch
                say(if (input.fingersAsTouch && input.hostTouch) "Fingers: Windows touch" else "Fingers: mouse (tap = click, hold = right click)")
            }
            item("Keyboard layout", { if (keysAsText) "Tablet" else "PC" }) { setKeysAsText(!keysAsText) }
            item("Alt+Tab to PC", { onOff(KeyCapture.enabled && captureKeys) }) { toggleCapture() }
            section("Stream")
            item("Mode", { if (Native.office()) "Office" else "Game" }) { toggleOffice() }
            item("Game resolution", { if (gameScale == AUTO) "Auto $autoGame%" else "$gameScale%" }) {
                gameScale = when (gameScale) { AUTO -> 100; 100 -> 75; 75 -> 50; else -> AUTO }
                getSharedPreferences("settings", MODE_PRIVATE).edit().putInt("gameScale", gameScale).apply()
                layoutPicture()
                say(
                    when (gameScale) {
                        AUTO -> "Game mode: the resolution follows the connection (now $autoGame%)"
                        100 -> "Game mode at the screen's full resolution"
                        else -> "Game mode at $gameScale%: fewer pixels to send and decode, scaled up here"
                    },
                )
            }
            item("Office resolution", { if (officeScale == AUTO) "Auto $autoOffice%" else "$officeScale%" }) {
                officeScale = when (officeScale) { AUTO -> 100; 100 -> 75; 75 -> 50; 50 -> 33; else -> AUTO }
                getSharedPreferences("settings", MODE_PRIVATE).edit().putInt("officeScale", officeScale).apply()
                layoutPicture()
                say(
                    when (officeScale) {
                        AUTO -> "Office mode: the resolution follows the connection (now $autoOffice%)"
                        100 -> "Office mode at the screen's full resolution"
                        else -> "Office mode at $officeScale%: for a weak connection, far less data, scaled up here"
                    } + if (Native.sessions().startsWith("w365")) " (Hyper only: Windows 365 stays at full)" else "",
                )
            }
            item("Text size (Windows 365)", {
                val chosen = getSharedPreferences("settings", MODE_PRIVATE).getInt("cloudScale", 0)
                if (chosen == 0) "Auto ${Screen.autoScale(this@StreamActivity)}%" else "$chosen%"
            }) {
                val prefs = getSharedPreferences("settings", MODE_PRIVATE)
                val steps = listOf(0, 100, 125, 150, 175, 200, 225, 250, 300)
                val next = steps[(steps.indexOf(prefs.getInt("cloudScale", 0)) + 1) % steps.size]
                prefs.edit().putInt("cloudScale", next).apply()
                Native.setDesktopScale(Screen.cloudScale(this@StreamActivity))
                say(
                    "Windows 365 text at ${Screen.cloudScale(this@StreamActivity)}%" +
                        if (Native.sessions().contains("w365")) "" else " (from the next Windows 365 connection)",
                )
            }
            item("Upscaler", { if (upscaleOn) "Snapdragon" else "Display" }) {
                upscaleOn = !upscaleOn
                getSharedPreferences("settings", MODE_PRIVATE).edit().putBoolean("upscale", upscaleOn).apply()
                say(
                    if (upscaleOn) "Upscaling with Snapdragon GSR on the GPU (sharper; below 100%)"
                    else "Upscaling by the display (no GPU work; softer)",
                )
            }
            item("Stats", { onOff(showStats) }) { toggleStats() }
            section("Session")
            item("Ctrl+Alt+Del") { Native.ctrlAltDel() }
            item("Switch session") { switchSession() }
            item("Other PCs") {
                // (The session stays open: "Back to…" there, or Switch session.)
                startActivity(Intent(this@StreamActivity, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP))
            }
            item("Disconnect", danger = true) { leave() }
        }

        setContentView(root)
        Perf.applyUnbuffered(root, !relaxed())
        hideSystemBars()
        root.isFocusable = true
        root.isFocusableInTouchMode = true
        root.requestFocus()

        (getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager).addPrimaryClipChangedListener(clipListener)
        (getSystemService(Context.INPUT_SERVICE) as InputManager).registerInputDeviceListener(devices, ui)
        // The back gesture opens the menu (Android 13+ hands it to a callback,
        // not as a key; a keyboard's Esc still goes to the PC, and Shift+Esc
        // opens it — see `handleKey`).
        onBackInvokedDispatcher.registerOnBackInvokedCallback(android.window.OnBackInvokedDispatcher.PRIORITY_DEFAULT) { menu.toggle() }
        // The connection lives on while the app is in the background.
        KeepAlive.start(this)
        say("Ctrl+Shift+M locks the mouse · Ctrl+Shift+Tab switches session · Ctrl+Shift+Q disconnects · the round button or Shift+Esc opens the menu (drag it anywhere)", 6000)
        watchNotify()
        ui.post(poll)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun noteInput() {
        meter.events++
        lastInputAt = SystemClock.uptimeMillis()
    }

    /**
     * A mouse or trackpad moved: the display up to the pointer's rate at once
     * (relaxed sessions), back down by [poll] once it has rested 1.5 s.
     */
    private fun notePointer(e: MotionEvent) {
        if (!e.isFromSource(InputDevice.SOURCE_MOUSE) && !e.isFromSource(InputDevice.SOURCE_TOUCHPAD) &&
            !e.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE)
        ) return
        lastPointerAt = SystemClock.uptimeMillis()
        if (!pointerFast && relaxed()) {
            pointerFast = true
            applyDisplay()
        }
    }

    private fun attach(surface: Surface) {
        // Compose the stream at its own rate (the display stays at its fastest).
        try {
            surface.setFrameRate(frameRate(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        } catch (_: Exception) {
        }
        // Smaller than the screen in game mode: upscaled on the GPU (Snapdragon
        // GSR) when that's on, else the display scales the decoder's pictures.
        upscaler = if (wantUpscale()) Upscaler.tryCreate(this, surface) else null
        upscaler?.setFrameRate(streamRate())
        if (wantUpscale() && upscaler == null && !upscaleFailed) {
            upscaleFailed = true
            say("Snapdragon upscaling isn't available here: the display scales the picture")
        }
        bufferSize = 0 to 0
        Native.setSurface(upscaler?.input ?: surface)
        layoutPicture()
    }

    private fun detach() {
        Native.setSurface(null)
        upscaler?.release()
        upscaler = null
    }

    private fun wantUpscale() = upscaleOn && !upscaleFailed && scale() < 100

    /** The stream's resolution, % of the screen, for the session and mode in front (Windows 365: always full). */
    private fun scale() = when {
        Native.sessions().startsWith("w365") -> 100
        Native.office() -> if (officeScale == AUTO) autoOffice else officeScale
        else -> if (gameScale == AUTO) autoGame else gameScale
    }

    /**
     * Auto resolution: the core's pick from the last second's numbers (it
     * changes at most once per 10 s: each change is a new stream size and a
     * keyframe on the PC). `debug.rdpio.autores=0`: it never changes.
     */
    private fun followAuto() {
        if (Native.sessions().startsWith("w365")) return
        val office = Native.office()
        if ((if (office) officeScale else gameScale) != AUTO) return
        val now = scale()
        val next = Native.autoScale(now, office)
        if (next == now || next <= 0) return
        if (office) autoOffice = next else autoGame = next
        layoutPicture()
        val mode = if (office) "Office" else "Game"
        say(
            if (next < now) "Slow connection: $mode mode at $next% (Auto resolution)"
            else "The connection keeps up: $mode mode at $next% (Auto resolution)",
            3000,
        )
    }

    /** A new Surface for the stream (and with it the upscaler, or not). */
    private fun reattach() {
        video.visibility = View.GONE
        ui.post { video.visibility = View.VISIBLE }
    }

    /** Office mode or Windows 365: a 60 fps desktop, nothing racing for latency. */
    private fun relaxed() = Native.sessions().startsWith("w365") || Native.office()

    /**
     * The display's mode: the fastest, or (when relaxed) 60 Hz — 120 while a
     * mouse or trackpad moves, 60 fps paced evenly and the pointer smooth.
     * (An attached mouse at rest kept the panel at 120 Hz, twice the
     * refreshes for a still desktop; `debug.rdpio.pointerhz=0`: 120 whenever
     * one is attached, as before.)
     */
    private fun displayMode() =
        if (relaxed()) Screen.modeFor(this, 60, if (pointerWantsFast()) 120 else 60) else Screen.fastestMode(this)

    private fun pointerWantsFast() =
        if (Native.ab("pointerhz", true)) pointerFast else Perf.pointerAttached()

    private fun frameRate() = if (relaxed()) 60f else fps.toFloat()

    /** The rate pictures come at: [frameRate], under the thermal cap (the upscaler's frame budget). */
    private fun streamRate() = if (thermal.cap > 0) min(frameRate(), thermal.cap.toFloat()) else frameRate()

    private fun applyDisplay() {
        window.attributes = window.attributes.apply { preferredDisplayModeId = displayMode()?.modeId ?: 0 }
        Perf.applyUnbuffered(root, !relaxed())
        upscaler?.setFrameRate(streamRate())
        try {
            video.holder.surface.takeIf { it.isValid }?.setFrameRate(frameRate(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        } catch (_: Exception) {
        }
    }

    /**
     * Wi-Fi without power-save naps while the stream plays (each nap can
     * hold a packet up to a beacon interval) — except for a relaxed session,
     * where the radio may doze between bursts.
     */
    private fun applyWifi() {
        val lock = !relaxed()
        if (lock && wifiLock == null) {
            val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
            wifiLock = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "rdpio-stream").apply {
                setReferenceCounted(false)
                acquire()
            }
        } else if (!lock) {
            wifiLock?.release()
            wifiLock = null
        }
    }

    private fun hideSystemBars() {
        window.insetsController?.let {
            it.hide(WindowInsets.Type.systemBars())
            it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    override fun onStart() {
        super.onStart()
        visible = true
        // (Back from the background: the poll at its full rate again.)
        ui.removeCallbacks(poll)
        ui.post(poll)
    }

    override fun onStop() {
        super.onStop()
        visible = false
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        hideSystemBars()
        Native.setPaused(false)
        applyRefresh()
        applyWifi()
        if (mouseLocked) root.requestPointerCapture()
    }

    override fun onPause() {
        super.onPause()
        resumed = false
        Native.setPaused(true)
        input.reset()
        wifiLock?.release()
        wifiLock = null
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) {
            KeyCapture.stream = this
            Perf.applyUnbuffered(root, !relaxed())
            hideSystemBars()
            // (Android lets an app read the clipboard only while it's in front.)
            sendClipboard()
            if (mouseLocked) root.requestPointerCapture()
        } else {
            if (KeyCapture.stream === this) KeyCapture.stream = null
            input.reset()
            typedKeys.clear()
            deadAccent = 0
        }
    }

    private fun granted(permission: String) = checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        Native.setPermissions(granted(Manifest.permission.RECORD_AUDIO), granted(Manifest.permission.CAMERA))
    }

    override fun onDestroy() {
        cameraFeed.close()
        if (KeyCapture.stream === this) KeyCapture.stream = null
        ui.removeCallbacksAndMessages(null)
        notifyFd?.let {
            Looper.getMainLooper().queue.removeOnFileDescriptorEventListener(it.fileDescriptor)
            it.close()
        }
        notifyFd = null
        (getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager).removePrimaryClipChangedListener(clipListener)
        (getSystemService(Context.INPUT_SERVICE) as InputManager).unregisterInputDeviceListener(devices)
        if (isFinishing && !ended) Native.disconnect()
        if (Native.sessions().isEmpty()) KeepAlive.stop(this)
        super.onDestroy()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        // Back from the connect screen, maybe with another session current.
        fps = intent.getIntExtra("fps", fps)
        showCurrent()
    }

    /** Ctrl+Shift+Tab or the menu: show the other open session. */
    private fun switchSession() {
        if (!Native.switchSession()) return say("No other session open — Other PCs connects one")
        showCurrent()
    }

    /**
     * Hand the screen to the current session. The Surface is made anew: one a
     * decoder drew into can't be drawn on by the CPU (Windows 365's
     * compositor), nor the other way round.
     */
    private fun showCurrent() {
        input.reset()
        typedKeys.clear()
        streamW = 0
        streamH = 0
        cursorGen = -1
        clipGen = -1
        video.visibility = View.GONE
        ui.post {
            video.visibility = View.VISIBLE
            Native.setPaused(false)
            applyDisplay()
            if (resumed) applyWifi()
            say(if (Native.sessions().startsWith("w365")) "Windows 365" else "Hyper stream", 1500)
        }
    }

    private fun leave(why: String = "closed") {
        if (ended) return
        Native.disconnect()
        if (Native.sessions().isNotEmpty()) {
            // The other session is still open: show it.
            say(if (why.startsWith("closed")) "Disconnected" else "Ended: $why", 3000)
            showCurrent()
            return
        }
        ended = true
        KeepAlive.stop(this)
        startActivity(Intent(this, MainActivity::class.java).putExtra("ended", why).addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP))
        finish()
    }

    private fun say(text: String, ms: Long = 2500) {
        banner.text = text
        banner.visibility = View.VISIBLE
        bannerUntil = System.currentTimeMillis() + ms
    }

    /** The display's rate for the PC, under the thermal cap (when on). */
    private fun applyRefresh() {
        val max = Screen.maxRefresh(this)
        Native.setRefresh(if (thermal.cap > 0) min(max, thermal.cap) else max)
        upscaler?.setFrameRate(streamRate())
    }

    private fun toggleStats() {
        showStats = !showStats
        stats.visibility = if (showStats) View.VISIBLE else View.GONE
    }

    private fun toggleOffice() {
        val office = !Native.office()
        Native.setOffice(office)
        applyDisplay()
        if (resumed) applyWifi()
        say(if (office) "Office mode: crisp text, 60 fps" else "Game mode: lowest latency")
    }

    private fun setMouseLocked(on: Boolean) {
        mouseLocked = on
        if (on) root.requestPointerCapture() else root.releasePointerCapture()
        say(if (on) "Mouse locked to the stream (games) — Ctrl+Shift+M frees it" else "Mouse free")
    }

    private fun toggleKeyboard() {
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        keyboardShown = !keyboardShown
        root.requestFocus()
        if (keyboardShown) imm.showSoftInput(root, 0) else imm.hideSoftInputFromWindow(root.windowToken, 0)
    }

    private fun sendClipboard() {
        val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val text = try {
            cm.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(this)?.toString()
        } catch (_: Exception) {
            null
        } ?: return
        if (text == lastClipFromHost || text == lastClipSent) return
        lastClipSent = text
        Native.sendClipboard(text)
    }

    /** The picture keeps its shape: as big as fits, centred, black around it. */
    private fun layoutPicture() {
        val cw = root.width
        val ch = root.height
        if (cw == 0 || ch == 0 || streamW == 0 || streamH == 0) return
        val scale = min(cw.toFloat() / streamW, ch.toFloat() / streamH)
        val w = (streamW * scale).roundToInt()
        val h = (streamH * scale).roundToInt()
        val lp = video.layoutParams as FrameLayout.LayoutParams
        if (lp.width != w || lp.height != h) {
            lp.width = w
            lp.height = h
            lp.gravity = Gravity.CENTER
            video.layoutParams = lp
        }
        // Decode at the stream's size and let the display scale it (no copy);
        // upscaling, the GPU draws at the screen's.
        val buffer = if (upscaler != null) w to h else streamW to streamH
        if (buffer != bufferSize) {
            bufferSize = buffer
            video.holder.setFixedSize(buffer.first, buffer.second)
        }
        upscaler?.setInputSize(streamW, streamH)
        val left = (cw - w) / 2
        val top = (ch - h) / 2
        input.picture.set(left, top, left + w, top + h)
        // (The PC fits the stream to this: smaller when asked.)
        val s = scale()
        val view = cw * s / 100 to ch * s / 100
        if (viewTold != view) {
            viewTold = view
            Native.setView(view.first, view.second)
        }
    }

    /** A pointer shape as the PC sent it, at a scale: the key to its made icon. */
    private class Shape(val raw: ByteArray, val scale: Float) {
        private val hash = raw.contentHashCode() * 31 + scale.hashCode()
        override fun hashCode() = hash
        override fun equals(other: Any?) = other is Shape && other.hash == hash && other.scale == scale && other.raw.contentEquals(raw)
    }

    /**
     * Icons made from the PC's shapes, the most recent 16: the arrow, the
     * I-beam and the hand come back again and again over a page, each a new
     * shape message, and each made anew was a pixel loop, a bitmap or two and
     * a PointerIcon (`debug.rdpio.cursorcache=0`: the old way).
     */
    private val icons = object : LinkedHashMap<Shape, PointerIcon>(16, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Shape, PointerIcon>) = size > 16
    }

    /** The host's pointer as Android's pointer icon (for a mouse or trackpad). */
    private fun updateCursor() {
        val t0 = SystemClock.elapsedRealtimeNanos()
        val raw = Native.cursor()
        var how = "made"
        icon = if (raw == null) {
            // Hidden by the PC (a game, a video).
            PointerIcon.getSystemIcon(this, PointerIcon.TYPE_NULL)
        } else if (raw.size < 8) {
            null // (no shape from the PC yet: Android's own arrow)
        } else {
            val b = ByteBuffer.wrap(raw).order(ByteOrder.LITTLE_ENDIAN)
            val w = b.short.toInt() and 0xFFFF
            val h = b.short.toInt() and 0xFFFF
            val hx = b.short.toInt() and 0xFFFF
            val hy = b.short.toInt() and 0xFFFF
            // As big on this screen as on the PC's.
            val s = if (streamW > 0) input.picture.width().toFloat() / streamW else 1f
            if (w == 0 || h == 0 || raw.size < 8 + w * h * 4) {
                PointerIcon.getSystemIcon(this, PointerIcon.TYPE_ARROW)
            } else if (Native.ab("cursorcache", true)) {
                val key = Shape(raw, s)
                icons[key]?.also { how = "cached" } ?: pointerIcon(raw, w, h, hx, hy, s).also { icons[key] = it }
            } else {
                val px = IntArray(w * h)
                for (i in px.indices) {
                    val o = 8 + i * 4
                    val r = raw[o].toInt() and 0xFF
                    val g = raw[o + 1].toInt() and 0xFF
                    val bl = raw[o + 2].toInt() and 0xFF
                    val a = raw[o + 3].toInt() and 0xFF
                    px[i] = (a shl 24) or (r shl 16) or (g shl 8) or bl
                }
                var bmp = Bitmap.createBitmap(px, w, h, Bitmap.Config.ARGB_8888)
                var sx = hx.toFloat()
                var sy = hy.toFloat()
                if (s > 0.1f && (s < 0.95f || s > 1.05f)) {
                    bmp = Bitmap.createScaledBitmap(bmp, (w * s).roundToInt().coerceAtLeast(1), (h * s).roundToInt().coerceAtLeast(1), true)
                    sx *= s
                    sy *= s
                }
                PointerIcon.create(bmp, sx.coerceIn(0f, bmp.width - 1f), sy.coerceIn(0f, bmp.height - 1f))
            }
        }
        root.pointerIcon = icon
        if (Native.ab("perflog", false)) Log.i("rdpio", "pointer: %s in %.3f ms".format(how, (SystemClock.elapsedRealtimeNanos() - t0) / 1e6))
    }

    /**
     * A PC shape (straight RGBA after an 8-byte header) as a pointer icon at
     * scale [s], hot spot scaled too. The pixels go in with one copy: a
     * bitmap's memory is RGBA bytes already, but premultiplied — so the
     * colour of each partly transparent pixel is multiplied by its alpha
     * first (what [Bitmap.createBitmap] from colours does, rounded the same).
     */
    private fun pointerIcon(raw: ByteArray, w: Int, h: Int, hx: Int, hy: Int, s: Float): PointerIcon {
        val px = raw.copyOfRange(8, 8 + w * h * 4)
        var o = 0
        while (o < px.size) {
            val a = px[o + 3].toInt() and 0xFF
            if (a != 255) {
                for (c in o until o + 3) {
                    val p = (px[c].toInt() and 0xFF) * a + 128
                    px[c] = ((p + (p shr 8)) shr 8).toByte()
                }
            }
            o += 4
        }
        var bmp = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
        bmp.copyPixelsFromBuffer(ByteBuffer.wrap(px))
        var sx = hx.toFloat()
        var sy = hy.toFloat()
        if (s > 0.1f && (s < 0.95f || s > 1.05f)) {
            bmp = Bitmap.createScaledBitmap(bmp, (w * s).roundToInt().coerceAtLeast(1), (h * s).roundToInt().coerceAtLeast(1), true)
            sx *= s
            sy *= s
        }
        return PointerIcon.create(bmp, sx.coerceIn(0f, bmp.width - 1f), sy.coerceIn(0f, bmp.height - 1f))
    }

    /**
     * Wake on the core's news (an eventfd it signals when the pointer, the
     * clipboard, the state or the size change) rather than notice it on the
     * next poll, up to 100 ms later; the poll then runs less often
     * (`debug.rdpio.uinotify=0`: polling only, the old way).
     */
    private fun watchNotify() {
        if (!Native.ab("uinotify", true)) return
        val fd = Native.notifyFd()
        if (fd < 0) return
        // (A dup: the core keeps its own.)
        val pfd = try {
            ParcelFileDescriptor.fromFd(fd)
        } catch (_: Exception) {
            return
        }
        notifyFd = pfd
        val buf = ByteArray(8)
        Looper.getMainLooper().queue.addOnFileDescriptorEventListener(pfd.fileDescriptor, MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT) { f, events ->
            if (events and MessageQueue.OnFileDescriptorEventListener.EVENT_ERROR != 0) return@addOnFileDescriptorEventListener 0
            // (Read: the count back to 0; one wake-up for any number of pokes.)
            try {
                Os.read(f, buf, 0, 8)
            } catch (_: Exception) {
            }
            newsCame = true
            if (!ended) react(Native.status())
            MessageQueue.OnFileDescriptorEventListener.EVENT_INPUT
        }
    }

    /**
     * What the status says, from [poll] or at once on the core's news: the
     * end, and on screen the picture's size, the pointer, the clipboard, a
     * reconnect. The state; null once the stream has ended.
     */
    private fun react(st: String): String? {
        val head = st.substringBefore('\n').split('|')
        val state = head.getOrElse(0) { "" }
        if (state.startsWith("ended")) {
            leave(state.removePrefix("ended:").trim())
            return null
        }
        if (!visible) return state
        val w = head.getOrNull(1)?.toIntOrNull() ?: 0
        val h = head.getOrNull(2)?.toIntOrNull() ?: 0
        if (w > 0 && h > 0 && (w != streamW || h != streamH)) {
            streamW = w
            streamH = h
        }
        layoutPicture()
        input.hostTouch = head.getOrNull(5) == "1"
        val cg = head.getOrNull(3)?.toLongOrNull() ?: 0
        if (cg != cursorGen) {
            cursorGen = cg
            updateCursor()
        }
        val clg = head.getOrNull(4)?.toLongOrNull() ?: 0
        if (clg != clipGen) {
            clipGen = clg
            Native.clipboard()?.let { t ->
                lastClipFromHost = t
                (getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager).setPrimaryClip(ClipData.newPlainText("rdpio", t))
            }
        }
        if (state.startsWith("reconnecting")) {
            banner.text = "Connection lost — reconnecting…"
            banner.visibility = View.VISIBLE
            bannerUntil = System.currentTimeMillis() + 1000
        }
        return state
    }

    private val poll = object : Runnable {
        override fun run() {
            if (ended) return
            val st = Native.status()
            react(st) ?: return
            if (!visible) {
                // In the background only the end matters (the picture, the
                // pointer and the clipboard wait for the return): once a second.
                ui.postDelayed(this, 1000)
                return
            }
            Native.notice()?.let { say(it, 8000) }
            // The session's apps want the mic or camera: ask once, then follow.
            val cameraWanted = Native.cameraWanted()
            if (!askedPermissions && ((Native.micWanted() && !granted(Manifest.permission.RECORD_AUDIO)) ||
                    (cameraWanted.isNotEmpty() && !granted(Manifest.permission.CAMERA)))
            ) {
                askedPermissions = true
                requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO, Manifest.permission.CAMERA), 1)
            }
            cameraFeed.poll(cameraWanted, granted(Manifest.permission.CAMERA))
            // (A reconnect keeps it up: see react.)
            if (System.currentTimeMillis() > bannerUntil) banner.visibility = View.GONE
            meter.tick(showStats)
            if (pointerFast && SystemClock.uptimeMillis() - lastPointerAt > 1500) {
                pointerFast = false
                if (relaxed()) applyDisplay()
            }
            thermal.poll()?.let { cap ->
                applyRefresh()
                say(if (cap == 0) "Cool again: full frame rate" else "The tablet is warm: $cap fps until it cools", 4000)
            }
            followAuto()
            if (showStats) stats.text = st.substringAfter('\n') + "\n" + meter.line + "  " + thermal.line + (upscaler?.let { "\n" + it.line } ?: "")
            // Game mode below 100% (or not any more), or the upscaler switched: a new Surface.
            if (video.holder.surface?.isValid == true && wantUpscale() != (upscaler != null)) reattach()
            input.tick()
            // Ten times a second while someone's at it (a held finger's right
            // click, the pointer's shape, a banner); four when nobody is.
            // Woken by the core's news (the pointer, the clipboard, the
            // state come at once): ten only while a finger is down, four
            // while someone's at it, once a second when nobody is.
            val busy = SystemClock.uptimeMillis() - lastInputAt < 2000 || showStats || banner.visibility == View.VISIBLE
            ui.postDelayed(
                this,
                when {
                    notifyFd == null || !newsCame -> if (busy) 100L else 250L
                    fingerDown -> 100L
                    busy -> 250L
                    else -> 1000L
                },
            )
        }
    }

    // ---- Keys ----

    override fun dispatchKeyEvent(e: KeyEvent): Boolean = handleKey(e) || super.dispatchKeyEvent(e)

    /** For [KeyCapture]: a key it caught first that's the stream's. */
    fun capturing(e: KeyEvent): Boolean {
        if (!captureKeys || !isHardware(e)) return false
        // Tablet layout: only the keys Android would keep (Alt and Win
        // combinations); the rest still go through the input method.
        if (!keysAsText) return true
        return e.isMetaPressed || (e.metaState and KeyEvent.META_ALT_LEFT_ON) != 0 ||
            e.keyCode == KeyEvent.KEYCODE_META_LEFT || e.keyCode == KeyEvent.KEYCODE_META_RIGHT || e.keyCode == KeyEvent.KEYCODE_ALT_LEFT
    }

    /** For [KeyCapture]: true when the stream took the key. */
    fun captured(e: KeyEvent): Boolean = handleKey(e)

    private fun toggleCapture() {
        if (!KeyCapture.enabled) {
            say("Turn on \"rdpio: PC shortcuts\" (under Installed apps / Downloaded apps). If it's greyed out: App info → ⋮ → Allow restricted settings, then try again.", 12000)
            try {
                startActivity(Intent(android.provider.Settings.ACTION_ACCESSIBILITY_SETTINGS))
            } catch (_: Exception) {
            }
            return
        }
        captureKeys = !captureKeys
        getSharedPreferences("settings", MODE_PRIVATE).edit().putBoolean("captureKeys", captureKeys).apply()
        say(if (captureKeys) "Alt+Tab, the Windows key and Alt+F4 go to the PC" else "Alt+Tab and the Windows key work on the tablet")
    }

    /** A Shift+Esc is down: its release opens or closes the menu. */
    private var menuEsc = false

    /**
     * From a physical keyboard, whatever the scan code (a keyboard sending
     * BACK for Esc may report none): the back gesture comes from no device.
     */
    private fun fromKeyboard(e: KeyEvent): Boolean {
        val d = e.device ?: return false
        return e.deviceId != KeyCharacterMap.VIRTUAL_KEYBOARD && !d.isVirtual &&
            d.keyboardType == android.view.InputDevice.KEYBOARD_TYPE_ALPHABETIC
    }

    /** A key from a keyboard (not the on-screen one, not a remote control). */
    private fun isHardware(e: KeyEvent) =
        e.scanCode != 0 && e.deviceId != KeyCharacterMap.VIRTUAL_KEYBOARD && e.device?.isVirtual != true &&
            (e.flags and KeyEvent.FLAG_SOFT_KEYBOARD) == 0

    private fun setKeysAsText(on: Boolean) {
        keysAsText = on
        getSharedPreferences("settings", MODE_PRIVATE).edit().putBoolean("keysAsText", on).apply()
        typedKeys.clear()
        deadAccent = 0
        // (The input method may compose again, or not.)
        (getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager).restartInput(root)
        say(
            if (on) "Keyboard: the tablet's layout (characters as typed here, input methods work)"
            else "Keyboard: the PC's layout (keys by position, like a keyboard plugged into the PC)",
        )
    }

    /**
     * Tablet-layout mode: a character key is typed as the character it makes
     * here (this tablet's layout, dead keys and AltGr included). Null for a
     * key that goes to the PC as a key: shortcuts, arrows, Enter, the keypad.
     */
    private fun typeAsText(e: KeyEvent): Boolean? {
        if (e.action == KeyEvent.ACTION_UP) return if (typedKeys.remove(e.keyCode)) true else null
        if (e.action != KeyEvent.ACTION_DOWN) return null
        if (e.isCtrlPressed || e.isMetaPressed || (e.metaState and KeyEvent.META_ALT_LEFT_ON) != 0) return null
        if (e.keyCode in KeyEvent.KEYCODE_NUMPAD_0..KeyEvent.KEYCODE_NUMPAD_RIGHT_PAREN) return null
        val c = e.getUnicodeChar(e.metaState)
        if (c == 0) return null
        if ((c and KeyCharacterMap.COMBINING_ACCENT) != 0) {
            // A dead key: its accent goes on the next letter.
            deadAccent = c and KeyCharacterMap.COMBINING_ACCENT_MASK
            typedKeys.add(e.keyCode)
            return true
        }
        val accent = deadAccent
        if (c <= 0x20) {
            deadAccent = 0
            // (Space after a dead key types the accent itself.)
            if (c != 0x20 || accent == 0) return null
            Native.text(String(Character.toChars(accent)))
            typedKeys.add(e.keyCode)
            return true
        }
        var ch = c
        if (accent != 0) {
            deadAccent = 0
            val combined = KeyEvent.getDeadChar(accent, c)
            if (combined != 0) ch = combined else Native.text(String(Character.toChars(accent)))
        }
        Native.text(String(Character.toChars(ch)))
        typedKeys.add(e.keyCode)
        return true
    }

    /** True when the key was taken (for the PC or for us). */
    private fun handleKey(e: KeyEvent): Boolean {
        lastInputAt = SystemClock.uptimeMillis()
        val down = e.action == KeyEvent.ACTION_DOWN
        // Our shortcuts (never sent to the PC).
        if (e.isCtrlPressed && e.isShiftPressed && e.action != KeyEvent.ACTION_MULTIPLE) {
            val act: (() -> Unit)? = when (e.keyCode) {
                KeyEvent.KEYCODE_Q -> { { leave() } }
                KeyEvent.KEYCODE_M -> { { setMouseLocked(!mouseLocked) } }
                KeyEvent.KEYCODE_O -> { { toggleOffice() } }
                KeyEvent.KEYCODE_S -> { { toggleStats() } }
                KeyEvent.KEYCODE_TAB -> { { switchSession() } }
                else -> null
            }
            if (act != null) {
                if (down && e.repeatCount == 0) act()
                return true
            }
        }
        // A keyboard's Esc, however it arrives: some keyboards send it as
        // BACK (scan code 0 included), and Android turns an Esc nobody took
        // into a fallback BACK. Esc goes to the PC; Shift+Esc is the menu.
        val esc = e.keyCode == KeyEvent.KEYCODE_ESCAPE ||
            (e.keyCode == KeyEvent.KEYCODE_BACK && (fromKeyboard(e) || (e.flags and KeyEvent.FLAG_FALLBACK) != 0))
        if (esc && (e.isShiftPressed || menuEsc)) {
            // (Its release toggles, even with Shift already let go.)
            if (down && e.repeatCount == 0) menuEsc = true
            if (!down && menuEsc) {
                menuEsc = false
                menu.toggle()
            }
            return true
        }
        when (e.keyCode) {
            // The tablet's own volume, not the PC's.
            KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.KEYCODE_VOLUME_MUTE -> return false
            KeyEvent.KEYCODE_BACK -> {
                if (esc) return Native.key(1, KeyEvent.KEYCODE_ESCAPE, down)
                // The back gesture: the menu.
                if (!down) menu.toggle()
                return true
            }
        }
        if (e.action == KeyEvent.ACTION_MULTIPLE) {
            e.characters?.let { Native.text(it) }
            return true
        }
        if (e.action != KeyEvent.ACTION_DOWN && e.action != KeyEvent.ACTION_UP) return false
        if (keysAsText && isHardware(e)) typeAsText(e)?.let { return it }
        // A keyboard's Esc may arrive as BACK: it's Esc (scan code 1).
        if (Native.key(e.scanCode, e.keyCode, down)) return true
        // No PC key for it (a character from an on-screen keyboard): type it.
        val ch = e.unicodeChar
        if (down && ch != 0 && (ch and KeyCharacterMap.COMBINING_ACCENT) == 0) {
            Native.text(String(Character.toChars(ch)))
            return true
        }
        return false
    }

    /** The full-screen view that takes pointers, touches and typed text. */
    inner class Root(ctx: Context) : FrameLayout(ctx) {
        init {
            setBackgroundColor(Color.BLACK)
        }

        override fun onLayout(changed: Boolean, l: Int, t: Int, r: Int, b: Int) {
            super.onLayout(changed, l, t, r, b)
            if (changed) {
                post { layoutPicture() }
                menu.relayout()
            }
        }

        override fun onInterceptTouchEvent(e: MotionEvent): Boolean {
            // A tap outside the open menu closes it (and still reaches the PC).
            if (menu.isOpen && e.actionMasked == MotionEvent.ACTION_DOWN && !menu.hits(e.x.toInt(), e.y.toInt())) menu.close()
            return false
        }

        override fun onTouchEvent(e: MotionEvent): Boolean {
            noteInput()
            notePointer(e)
            fingerDown = e.actionMasked != MotionEvent.ACTION_UP && e.actionMasked != MotionEvent.ACTION_CANCEL
            if (e.actionMasked == MotionEvent.ACTION_DOWN && newsCame) {
                // (The poll may be a second away: back to its fastest now, for a held finger.)
                ui.removeCallbacks(poll)
                ui.postDelayed(poll, 100)
            }
            return input.pointer(e) || input.touch(e)
        }

        override fun onGenericMotionEvent(e: MotionEvent): Boolean {
            noteInput()
            notePointer(e)
            return input.pointer(e) || super.onGenericMotionEvent(e)
        }

        override fun onCapturedPointerEvent(e: MotionEvent): Boolean {
            noteInput()
            notePointer(e)
            return input.captured(e)
        }

        override fun onPointerCaptureChange(hasCapture: Boolean) {
            if (!hasCapture && mouseLocked && hasWindowFocus()) {
                // Taken away by the system (a dialog, the shade): the lock is off.
                mouseLocked = false
            }
        }

        override fun onResolvePointerIcon(event: MotionEvent, pointerIndex: Int): PointerIcon? = icon ?: super.onResolvePointerIcon(event, pointerIndex)

        override fun dispatchKeyEventPreIme(e: KeyEvent): Boolean {
            // PC layout: a keyboard's keys go to the PC as they are, before
            // the tablet's input method can type, compose or autocorrect
            // them. (Tablet layout: the input method sees them first, so
            // Japanese, Chinese or Korean input works.)
            if (!keysAsText && isHardware(e) && handleKey(e)) return true
            // A keyboard's Esc sent as BACK, in either layout: past here,
            // Android 13+ hands BACK to the back callback (the menu).
            val keyboardBack = e.keyCode == KeyEvent.KEYCODE_BACK &&
                (fromKeyboard(e) || (e.flags and KeyEvent.FLAG_FALLBACK) != 0)
            if (keyboardBack && handleKey(e)) return true
            return super.dispatchKeyEventPreIme(e)
        }

        override fun onCheckIsTextEditor() = true

        override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
            // (A "password" field keeps input methods to plain typing; the
            // tablet-layout mode lets them compose.)
            outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
                if (keysAsText) 0 else InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
            outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_ACTION_NONE
            return object : BaseInputConnection(this, false) {
                // A word the keyboard is still composing: typed once it's
                // committed (or finished), never twice.
                private var composing = ""

                override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
                    composing = text.toString()
                    return true
                }

                override fun finishComposingText(): Boolean {
                    if (composing.isNotEmpty()) type(composing)
                    composing = ""
                    return true
                }

                override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
                    composing = ""
                    type(text)
                    return true
                }

                private fun type(text: CharSequence) {
                    // Enter from the on-screen keyboard is a key, not a newline character.
                    text.split('\n').forEachIndexed { i, part ->
                        if (i > 0) tap(66)
                        if (part.isNotEmpty()) Native.text(part)
                    }
                }

                override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                    if (composing.isNotEmpty()) {
                        composing = composing.dropLast(beforeLength)
                        return true
                    }
                    repeat(beforeLength.coerceAtMost(64)) { tap(67) }
                    repeat(afterLength.coerceAtMost(64)) { tap(112) }
                    return true
                }

                override fun sendKeyEvent(event: KeyEvent): Boolean = dispatchKeyEvent(event)

                override fun performEditorAction(actionCode: Int): Boolean {
                    tap(66)
                    return true
                }

                private fun tap(keyCode: Int) {
                    Native.key(0, keyCode, true)
                    Native.key(0, keyCode, false)
                }
            }
        }
    }
}

/** Game or Office resolution: Auto (the size follows the connection). */
private const val AUTO = 0
