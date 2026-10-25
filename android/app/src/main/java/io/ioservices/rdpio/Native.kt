package io.ioservices.rdpio

import android.view.Surface

/** The Rust core (android/rust): the Hyper and Windows 365 clients. One of each open at most; one shown. */
object Native {
    init {
        System.loadLibrary("rdpio")
        init()
    }

    @JvmStatic private external fun init()

    /** Codecs this device decodes in hardware, best first ("hevc,h264"). */
    @JvmStatic external fun codecs(): String

    /** Sign in (blocking: call off the UI thread). "" when it worked, else why not. */
    @JvmStatic external fun connect(
        host: String, port: Int, password: String,
        width: Int, height: Int, refreshHz: Int, scale: Int,
        office: Boolean, codec: String,
    ): String

    @JvmStatic external fun disconnect()

    /** The Surface to show the stream on; null when it's destroyed (returns once it's let go). */
    @JvmStatic external fun setSurface(surface: Surface?)
    @JvmStatic external fun setPaused(paused: Boolean)
    @JvmStatic external fun setOffice(office: Boolean)
    @JvmStatic external fun office(): Boolean
    @JvmStatic external fun setView(width: Int, height: Int)
    @JvmStatic external fun setRefresh(hz: Int)
    /** Auto resolution: the stream's scale (%) to use now, given the one in use ([current] when nothing says to move). */
    @JvmStatic external fun autoScale(current: Int, office: Boolean): Int

    /** A hardware key; false when it has no PC scancode. */
    @JvmStatic external fun key(scanCode: Int, keyCode: Int, down: Boolean): Boolean
    @JvmStatic external fun text(text: String)
    /** The pointer at x, y in 0..65535 across the picture. */
    @JvmStatic external fun mouseAbs(x: Int, y: Int)
    @JvmStatic external fun mouseRel(dx: Int, dy: Int)
    /** 0 left, 1 right, 2 middle, 3 back, 4 forward. */
    @JvmStatic external fun button(button: Int, down: Boolean)
    /** WHEEL_DELTA units: 120 a notch. */
    @JvmStatic external fun wheel(delta: Int, horizontal: Boolean)
    /** A finger as Windows touch: phase 0 down, 1 move, 2 up, 3 cancel; x, y 0..65535. */
    @JvmStatic external fun touch(id: Int, phase: Int, x: Int, y: Int)
    @JvmStatic external fun ctrlAltDel()
    @JvmStatic external fun releaseAll()

    /** "state|width|height|cursorGen|clipboardGen|touch\nstats" (touch: 1 when the PC takes fingers as touch). */
    @JvmStatic external fun status(): String
    /** width, height, hotX, hotY (u16 LE) + RGBA; null while hidden. */
    @JvmStatic external fun cursor(): ByteArray?
    @JvmStatic external fun clipboard(): String?
    @JvmStatic external fun sendClipboard(text: String)
    @JvmStatic external fun notice(): String?
    /** An eventfd that turns readable when [status] has news (pointer, clipboard, state, size); -1 if none. */
    @JvmStatic external fun notifyFd(): Int

    // The tablet's mic and camera, for the session's apps.
    @JvmStatic external fun setPermissions(mic: Boolean, camera: Boolean)
    @JvmStatic external fun setCameraCount(count: Int)
    /** A session wants the mic and it isn't allowed yet: ask. */
    @JvmStatic external fun micWanted(): Boolean
    /** "width,height,fps,bitrate,generation,keyframes" while a session wants the camera, else "". */
    @JvmStatic external fun cameraWanted(): String
    /** One encoded frame (Annex-B H.264): `size` bytes at `offset` in a direct buffer. */
    @JvmStatic external fun cameraFrame(buffer: java.nio.ByteBuffer, offset: Int, size: Int, keyframe: Boolean)

    // Windows 365 (blocking calls: off the UI thread).
    /** "url\nredirect": the Microsoft sign-in page, and the address it ends at. */
    @JvmStatic external fun w365SignInUrl(): String
    /** With the redirect's code, or a kept refresh token (`refresh`): "ok\nuser\nrefresh token" or "error\nwhy". */
    @JvmStatic external fun w365SignIn(code: String, refresh: Boolean): String
    @JvmStatic external fun w365SignOut()
    @JvmStatic external fun w365User(): String
    /** "ok\nname\tkey\n…" (key: an imported one's, else empty) or "error\nwhy". */
    @JvmStatic external fun w365List(): String
    /** A Cloud PC from a `.rdp` (or Windows App cache) file: "ok\nkey\nname" or "error\nwhy". */
    @JvmStatic external fun w365Import(file: ByteArray): String
    @JvmStatic external fun w365Forget(key: String)
    /** "" when it worked, else why not. Becomes the current session; a Hyper one stays open, paused. */
    @JvmStatic external fun w365Connect(index: Int, password: String, width: Int, height: Int): String
    /** The app's files folder (the PCs' local addresses are kept there). */
    @JvmStatic external fun setDataDir(dir: String)
    /** A hidden on/off for measuring one improvement (`adb shell setprop debug.rdpio.<name> 0|1`). */
    @JvmStatic external fun ab(name: String, default: Boolean): Boolean
    /** Windows' display scale (%) for Windows 365: the next connection's, and the open one's at once. */
    @JvmStatic external fun setDesktopScale(percent: Int)

    /** The open sessions, the current one first: "hyper", "w365" or "w365,hyper"… */
    @JvmStatic external fun sessions(): String
    /** Show the other open session (then hand it the Surface); false when there's none. */
    @JvmStatic external fun switchSession(): Boolean
}
