package io.ioservices.rdpio

import android.content.Context
import android.graphics.SurfaceTexture
import android.opengl.EGL14
import android.opengl.EGLConfig
import android.opengl.EGLContext
import android.opengl.EGLDisplay
import android.opengl.EGLSurface
import android.opengl.GLES11Ext
import android.opengl.GLES20.*
import android.opengl.GLES30
import android.os.Build
import android.os.Debug
import android.os.Handler
import android.os.HandlerThread
import android.os.PerformanceHintManager
import android.os.Process
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.CountDownLatch

/**
 * A stream smaller than the screen, made full size on the GPU with Snapdragon
 * Game Super Resolution 1 (Qualcomm's single-pass upscaler for Adreno:
 * a 12-tap Lanczos-like filter with adaptive sharpening; the shader is
 * Qualcomm's, unchanged, in assets/ with its BSD-3-Clause licence).
 *
 * The decoder draws into [input] (a SurfaceTexture); each new picture is
 * copied to an RGBA texture at the stream's size (SGSR gathers texels, which
 * the decoder's external texture can't), then upscaled onto the screen's
 * Surface. On its own thread; [release] when the screen's Surface goes.
 */
class Upscaler(context: Context, output: Surface) {
    private val thread = HandlerThread("rdpio-upscale", Process.THREAD_PRIORITY_DISPLAY).apply { start() }
    private val gl = Handler(thread.looper)

    /** Where the decoder draws (valid once constructed). */
    lateinit var input: Surface
        private set

    private var display: EGLDisplay = EGL14.EGL_NO_DISPLAY
    private var ctx: EGLContext = EGL14.EGL_NO_CONTEXT
    private var window: EGLSurface = EGL14.EGL_NO_SURFACE
    private lateinit var frames: SurfaceTexture
    private var oes = 0
    private var rgb = 0
    private var fbo = 0
    private var copy = 0
    private var sgsr = 0

    // Uniform locations, looked up once (not by name every frame).
    private var copyTex = 0
    private var copySt = 0
    private var sgsrTex = 0
    private var sgsrView = 0

    /** The screen Surface's size, asked again once a second (not every frame). */
    private val size = IntArray(2)
    private var sizedAt = 0L

    // GPU time per frame (EXT_disjoint_timer_query), where the GPU has it.
    private var timer = false
    private val queries = IntArray(4)
    private var queryNext = 0
    private val queryBusy = BooleanArray(4)
    private var gpuNs = 0L
    private var gpuFrames = 0
    private val st = FloatArray(16)
    private val quad = ByteBuffer.allocateDirect(8 * 4).order(ByteOrder.nativeOrder()).asFloatBuffer()
        .apply { put(floatArrayOf(-1f, -1f, 1f, -1f, -1f, 1f, 1f, 1f)).position(0) }

    /** The stream's size (the RGBA texture's); 0 until known. */
    @Volatile private var inW = 0
    @Volatile private var inH = 0
    private var texW = 0
    private var texH = 0
    @Volatile private var released = false

    // For the stats line.
    private var drawn = 0
    /** Pictures latched past unshown: a newer one was already queued behind them. */
    private var stale = 0
    /** The time of the picture last drawn (none yet: MIN_VALUE). */
    private var drawnTs = Long.MIN_VALUE
    /**
     * Only the newest picture queued is drawn: after a stall the decoder's
     * pictures come in a clump, and drawing each in turn (a full GSR pass
     * and a swap each) showed the newest last, frames late. The older ones
     * are latched past unshown — they're already decoded, nothing refers to
     * the drawn copy (`debug.rdpio.newest=0`: each drawn, the old way). Read
     * once a second.
     */
    private var newest = true
    private var drawNs = 0L
    private var since = SystemClock.elapsedRealtime()
    @Volatile var line = "upscale: Snapdragon GSR"
        private set

    /**
     * Tell the GPU the old contents are dead before each pass (Adreno renders
     * in tiles: otherwise it may load the old picture into tile memory first,
     * a full-screen read for nothing). Read once a second
     * (`debug.rdpio.invalidate=0`: the old way).
     */
    private var invalidate = true

    // A performance hint session (ADPF, Android 12+) for this thread: its
    // clock set to meet the frame interval, not guessed by the governor from
    // load after the fact (`debug.rdpio.upscalehint=0`: none, the old way).
    // (Typed Any: the classes aren't on Android 11.)
    private val hints: Any? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) context.getSystemService(PerformanceHintManager::class.java) else null
    private var hint: Any? = null
    private var hintFailed = false
    private var hintTarget = 0L
    /** The frame interval, ns (the stream's rate: [setFrameRate]). */
    @Volatile private var targetNs = 1_000_000_000L / 60
    /** The GPU time of the latest frame the GPU finished (a frame or two behind the CPU). */
    private var lastGpuNs = 0L

    init {
        val shader = context.assets.open(SHADER).bufferedReader().use { it.readText() }
        val ready = CountDownLatch(1)
        var failure: Exception? = null
        gl.post {
            try {
                setUp(output, shader)
            } catch (e: Exception) {
                failure = e
            }
            ready.countDown()
        }
        ready.await()
        failure?.let {
            release()
            throw it
        }
    }

    /** The stream's size: the decoder's pictures, before upscaling. */
    fun setInputSize(w: Int, h: Int) {
        if (w <= 0 || h <= 0 || (w == inW && h == inH)) return
        inW = w
        inH = h
        gl.post { if (!released) frames.setDefaultBufferSize(w, h) }
    }

    /** Frames a second the stream comes at: the performance hint's target. */
    fun setFrameRate(fps: Float) {
        if (fps <= 0f) return
        targetNs = (1e9 / fps).toLong()
        gl.post { if (!released) followHint() }
    }

    /** Let go of everything (after the decoder has let go of [input]). */
    fun release() {
        released = true
        gl.post {
            closeHint()
            if (::frames.isInitialized) frames.release()
            if (::input.isInitialized) input.release()
            if (display != EGL14.EGL_NO_DISPLAY) {
                EGL14.eglMakeCurrent(display, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT)
                if (window != EGL14.EGL_NO_SURFACE) EGL14.eglDestroySurface(display, window)
                if (ctx != EGL14.EGL_NO_CONTEXT) EGL14.eglDestroyContext(display, ctx)
                EGL14.eglTerminate(display)
            }
            thread.quitSafely()
        }
    }

    private fun setUp(output: Surface, shader: String) {
        display = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY)
        val version = IntArray(2)
        check(EGL14.eglInitialize(display, version, 0, version, 1)) { "EGL: no display" }
        val configs = arrayOfNulls<EGLConfig>(1)
        val count = IntArray(1)
        val want = intArrayOf(
            EGL14.EGL_RED_SIZE, 8, EGL14.EGL_GREEN_SIZE, 8, EGL14.EGL_BLUE_SIZE, 8, EGL14.EGL_ALPHA_SIZE, 0,
            EGL14.EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL14.EGL_SURFACE_TYPE, EGL14.EGL_WINDOW_BIT, EGL14.EGL_NONE,
        )
        check(EGL14.eglChooseConfig(display, want, 0, configs, 0, 1, count, 0) && count[0] > 0) { "EGL: no RGB8 ES3 config" }
        ctx = EGL14.eglCreateContext(display, configs[0], EGL14.EGL_NO_CONTEXT, intArrayOf(EGL14.EGL_CONTEXT_CLIENT_VERSION, 3, EGL14.EGL_NONE), 0)
        check(ctx != EGL14.EGL_NO_CONTEXT) { "EGL: no ES3 context" }
        window = EGL14.eglCreateWindowSurface(display, configs[0], output, intArrayOf(EGL14.EGL_NONE), 0)
        check(window != EGL14.EGL_NO_SURFACE) { "EGL: no window surface" }
        check(EGL14.eglMakeCurrent(display, window, window, ctx)) { "EGL: can't make current" }
        // Never wait for the display: a new picture replaces one not yet shown.
        EGL14.eglSwapInterval(display, 0)

        copy = program(COPY_VS, COPY_FS)
        // (GSR gathers texels: ES 3.1; and the edge-direction weights on.)
        val gsr = shader.replace("#version 300 es", "#version 310 es").replace("// #define UseEdgeDirection", "#define UseEdgeDirection")
        sgsr = program(SGSR_VS, gsr)
        copyTex = glGetUniformLocation(copy, "tex")
        copySt = glGetUniformLocation(copy, "st")
        sgsrTex = glGetUniformLocation(sgsr, "ps0")
        sgsrView = glGetUniformLocation(sgsr, "ViewportInfo[0]")
        timer = glGetString(GL_EXTENSIONS)?.contains("GL_EXT_disjoint_timer_query") == true
        if (timer) GLES30.glGenQueries(queries.size, queries, 0)

        val t = IntArray(3)
        glGenTextures(2, t, 0)
        oes = t[0]
        rgb = t[1]
        glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oes)
        glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR)
        glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR)
        glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE)
        glTexParameteri(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE)
        glGenFramebuffers(1, t, 2)
        fbo = t[2]

        frames = SurfaceTexture(oes)
        frames.setOnFrameAvailableListener({ draw() }, gl)
        input = Surface(frames)
        invalidate = Native.ab("invalidate", true)
        followHint()
    }

    /** Open, retarget or close the hint session as the switch and the frame rate say (on this thread: it's for this thread). */
    private fun followHint() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        if (!Native.ab("upscalehint", true)) return closeHint()
        val target = targetNs
        val h = hint as PerformanceHintManager.Session?
        if (h == null) {
            if (hintFailed) return
            hint = try {
                (hints as PerformanceHintManager?)?.createHintSession(intArrayOf(Process.myTid()), target)
            } catch (_: Exception) {
                null
            }
            hintTarget = target
            if (hint == null) {
                hintFailed = true
                Log.i(TAG, "upscale: no performance hint session")
            } else {
                Log.i(TAG, "upscale: performance hints on, %.1f ms a frame".format(target / 1e6))
            }
        } else if (target != hintTarget) {
            hintTarget = target
            try {
                h.updateTargetWorkDuration(target)
            } catch (_: Exception) {
            }
        }
    }

    private fun closeHint() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) (hint as PerformanceHintManager.Session?)?.close()
        hint = null
    }

    /**
     * The frame's work, for the hint session: [wallNs] from the start to the
     * swap, [cpuNs] this thread's CPU time in it. Android 15+ also takes the
     * GPU's part: the latest finished frame's (GPU times arrive a frame or two
     * late and are never waited for), counted after the CPU's (the GPU starts
     * on a frame at the swap's flush).
     */
    private fun reportHint(startNs: Long, wallNs: Long, cpuNs: Long) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        val h = hint as PerformanceHintManager.Session? ?: return
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.VANILLA_ICE_CREAM && lastGpuNs > 0) {
                h.reportActualWorkDuration(
                    android.os.WorkDuration().apply {
                        setWorkPeriodStartTimestampNanos(startNs)
                        setActualTotalDurationNanos(wallNs.coerceAtLeast(1) + lastGpuNs)
                        setActualCpuDurationNanos(cpuNs.coerceAtLeast(0))
                        setActualGpuDurationNanos(lastGpuNs)
                    },
                )
            } else {
                h.reportActualWorkDuration(wallNs.coerceAtLeast(1))
            }
        } catch (_: Exception) {
        }
    }

    /** The RGBA texture at the stream's size (anew when it changes). */
    private fun sizeTexture(w: Int, h: Int) {
        if (w == texW && h == texH) return
        glBindTexture(GL_TEXTURE_2D, rgb)
        glTexImage2D(GL_TEXTURE_2D, 0, GLES30.GL_RGBA8, w, h, 0, GL_RGBA, GL_UNSIGNED_BYTE, null)
        // (GSR's centre sample is bilinear; its taps are gathers.)
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR)
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR)
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE)
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE)
        glBindFramebuffer(GL_FRAMEBUFFER, fbo)
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, rgb, 0)
        texW = w
        texH = h
    }

    private fun draw() {
        if (released) return
        val t0 = SystemClock.elapsedRealtimeNanos()
        // (ADPF wants the start on CLOCK_MONOTONIC, and the thread's own CPU time.)
        val start = System.nanoTime()
        val cpu0 = if (hint != null) Debug.threadCpuTimeNanos() else 0L
        frames.updateTexImage()
        if (newest) {
            // (Each queued picture posts its own call here: the ones latched
            // past below find nothing new and draw nothing.)
            var ts = frames.timestamp
            if (ts != 0L && ts == drawnTs) return
            for (i in 0 until MAX_LATCH) {
                frames.updateTexImage()
                val t = frames.timestamp
                if (t == ts) break
                ts = t
                stale++
            }
            drawnTs = ts
        }
        frames.getTransformMatrix(st)
        val (w, h) = inW to inH
        if (w == 0 || h == 0) return
        val now = SystemClock.elapsedRealtime()
        if (now - sizedAt >= 1000) {
            EGL14.eglQuerySurface(display, window, EGL14.EGL_WIDTH, size, 0)
            EGL14.eglQuerySurface(display, window, EGL14.EGL_HEIGHT, size, 1)
            sizedAt = now
        }
        readTimers()
        val q = queryNext
        val timing = timer && !queryBusy[q]
        if (timing) GLES30.glBeginQuery(GL_TIME_ELAPSED_EXT, queries[q])
        glDisable(GL_BLEND)
        glEnableVertexAttribArray(0)
        glVertexAttribPointer(0, 2, GL_FLOAT, false, 0, quad)
        glActiveTexture(GL_TEXTURE0)

        sizeTexture(w, h)
        // 1: the decoder's picture (cropped, oriented) into the RGBA texture.
        glBindFramebuffer(GL_FRAMEBUFFER, fbo)
        // (Every texel is drawn anew: nothing to load into tile memory first.)
        if (invalidate) GLES30.glInvalidateFramebuffer(GL_FRAMEBUFFER, 1, FBO_COLOR, 0)
        glViewport(0, 0, w, h)
        glUseProgram(copy)
        glBindTexture(GLES11Ext.GL_TEXTURE_EXTERNAL_OES, oes)
        glUniform1i(copyTex, 0)
        glUniformMatrix4fv(copySt, 1, false, st, 0)
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4)

        // 2: Snapdragon GSR onto the screen.
        glBindFramebuffer(GL_FRAMEBUFFER, 0)
        if (invalidate) GLES30.glInvalidateFramebuffer(GL_FRAMEBUFFER, 1, WINDOW_COLOR, 0)
        glViewport(0, 0, size[0], size[1])
        glUseProgram(sgsr)
        glBindTexture(GL_TEXTURE_2D, rgb)
        glUniform1i(sgsrTex, 0)
        glUniform4f(sgsrView, 1f / w, 1f / h, w.toFloat(), h.toFloat())
        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4)
        // (Depth and stencil, if the config has any: never written back.)
        if (invalidate) GLES30.glInvalidateFramebuffer(GL_FRAMEBUFFER, 2, WINDOW_DEPTH_STENCIL, 0)
        if (timing) {
            GLES30.glEndQuery(GL_TIME_ELAPSED_EXT)
            queryBusy[q] = true
            queryNext = (q + 1) % queries.size
        }
        EGL14.eglSwapBuffers(display, window)

        val took = SystemClock.elapsedRealtimeNanos() - t0
        if (hint != null) reportHint(start, took, Debug.threadCpuTimeNanos() - cpu0)
        drawn++
        drawNs += took
        if (now - since >= 1000) {
            val gpu = if (gpuFrames > 0) "  %.2f ms GPU".format(gpuNs / 1e6 / gpuFrames) else ""
            line = "upscale: Snapdragon GSR ${w}x$h → ${size[0]}x${size[1]}  " +
                "${drawn * 1000 / (now - since)} fps  " + "%.2f ms CPU".format(drawNs / 1e6 / drawn.coerceAtLeast(1)) + gpu +
                (if (invalidate) "  inval" else "") + (if (hint != null) "  hint" else "") +
                (if (newest) "  stale $stale" else "")
            if (Native.ab("perflog", false)) Log.i(TAG, line)
            invalidate = Native.ab("invalidate", true)
            newest = Native.ab("newest", true)
            followHint()
            drawn = 0
            stale = 0
            drawNs = 0
            gpuNs = 0
            gpuFrames = 0
            since = now
        }
    }

    /** Collect the GPU times of frames the GPU has finished (never waiting for one). */
    private fun readTimers() {
        if (!timer) return
        val v = IntArray(1)
        glGetIntegerv(GL_GPU_DISJOINT_EXT, v, 0)
        val disjoint = v[0] != 0
        for (i in queries.indices) {
            if (!queryBusy[i]) continue
            GLES30.glGetQueryObjectuiv(queries[i], GLES30.GL_QUERY_RESULT_AVAILABLE, v, 0)
            if (v[0] == 0) continue
            GLES30.glGetQueryObjectuiv(queries[i], GLES30.GL_QUERY_RESULT, v, 0)
            queryBusy[i] = false
            if (!disjoint) {
                lastGpuNs = v[0].toLong() and 0xFFFFFFFFL
                gpuNs += lastGpuNs
                gpuFrames++
            }
        }
    }

    private fun program(vs: String, fs: String): Int {
        val p = glCreateProgram()
        for ((type, src) in listOf(GL_VERTEX_SHADER to vs, GL_FRAGMENT_SHADER to fs)) {
            val s = glCreateShader(type)
            glShaderSource(s, src)
            glCompileShader(s)
            val ok = IntArray(1)
            glGetShaderiv(s, GL_COMPILE_STATUS, ok, 0)
            check(ok[0] != 0) { "shader: ${glGetShaderInfoLog(s)}" }
            glAttachShader(p, s)
            glDeleteShader(s)
        }
        glBindAttribLocation(p, 0, "pos")
        glLinkProgram(p)
        val ok = IntArray(1)
        glGetProgramiv(p, GL_LINK_STATUS, ok, 0)
        check(ok[0] != 0) { "program: ${glGetProgramInfoLog(p)}" }
        return p
    }

    companion object {
        private const val TAG = "rdpio"
        /** Pictures latched past at most per draw (a SurfaceTexture queues a few at most). */
        private const val MAX_LATCH = 8
        const val SHADER = "sgsr1_shader_mobile_edge_direction.frag"
        private const val EGL_OPENGL_ES3_BIT = 0x40
        private const val GL_TIME_ELAPSED_EXT = 0x88BF
        private const val GL_GPU_DISJOINT_EXT = 0x8FBB
        private val FBO_COLOR = intArrayOf(GL_COLOR_ATTACHMENT0)
        // (The window's own buffers go by GL_COLOR, GL_DEPTH and GL_STENCIL.)
        private val WINDOW_COLOR = intArrayOf(0x1800)
        private val WINDOW_DEPTH_STENCIL = intArrayOf(0x1801, 0x1802)

        private const val COPY_VS = """#version 300 es
layout(location = 0) in vec2 pos;
uniform mat4 st;
out vec2 uv;
void main() {
    uv = (st * vec4(pos * 0.5 + 0.5, 0.0, 1.0)).xy;
    gl_Position = vec4(pos, 0.0, 1.0);
}"""

        private const val COPY_FS = """#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision mediump float;
uniform samplerExternalOES tex;
in vec2 uv;
out vec4 color;
void main() {
    color = texture(tex, uv);
}"""

        /** Feeds GSR's fragment shader its texture coordinate. */
        private const val SGSR_VS = """#version 310 es
layout(location = 0) in vec2 pos;
layout(location = 0) out highp vec4 in_TEXCOORD0;
void main() {
    in_TEXCOORD0 = vec4(pos * 0.5 + 0.5, 0.0, 0.0);
    gl_Position = vec4(pos, 0.0, 1.0);
}"""

        /** Make one, or null (logged) where the GPU can't. */
        fun tryCreate(context: Context, output: Surface): Upscaler? = try {
            Upscaler(context, output)
        } catch (e: Exception) {
            Log.w(TAG, "upscaler unavailable: ${e.message}")
            null
        }
    }
}
