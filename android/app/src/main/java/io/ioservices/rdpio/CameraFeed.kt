package io.ioservices.rdpio

import android.annotation.SuppressLint
import android.content.Context
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.util.Log
import android.util.Range
import android.view.Surface

/**
 * The tablet's camera for a session: Camera2 draws straight into a hardware
 * H.264 encoder's input Surface (no picture is copied anywhere), and each
 * encoded frame goes to the Rust core ([Native.cameraFrame]), which sends it.
 *
 * Runs on its own thread; [poll] (from the stream screen's 100 ms status
 * poll) starts, restarts and stops it as the session asks.
 */
class CameraFeed(private val context: Context) {
    private val thread = HandlerThread("camera").apply { start() }
    private val handler = Handler(thread.looper)
    private var running: String? = null
    private var keyframes = 0
    private var camera: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var encoder: MediaCodec? = null
    private var inputSurface: Surface? = null
    /** SPS/PPS, put before a keyframe that comes without them. */
    private var config: ByteArray? = null
    /** When the encoder last gave a frame (elapsedRealtime), and whether the
     *  camera or encoder failed: Android can take the camera away mid-call
     *  (another app, the app in the background, a HAL error), and the session
     *  goes on asking for it, so the watchdog in [poll] restarts it. */
    @Volatile private var lastOutput = 0L
    @Volatile private var broken = false
    private var startedAt = 0L
    private var restarts = 0

    /** How many cameras this tablet has. */
    fun count(): Int = try {
        (context.getSystemService(Context.CAMERA_SERVICE) as CameraManager).cameraIdList.size
    } catch (_: Exception) {
        0
    }

    /** Follow what the session wants ("" = nothing; see [Native.cameraWanted]). */
    fun poll(wanted: String, allowed: Boolean) {
        val parts = wanted.split(',')
        val spec = if (wanted.isEmpty() || !allowed) null else parts.take(5).joinToString(",")
        val now = SystemClock.elapsedRealtime()
        if (spec != running) {
            running = spec
            restarts = 0
            restart(spec, parts, now)
        } else if (spec != null) {
            if (lastOutput > startedAt) restarts = 0
            // Frames come at the camera's rate even of a still scene, so
            // seconds without one (or a failure) mean it was lost: start it
            // again, less often while it keeps failing (3, 6, 12, 24 s).
            val stalled = now - maxOf(lastOutput, startedAt) >= STALL_MS
            if ((broken || stalled) && now - startedAt >= STALL_MS shl minOf(restarts, 3)) {
                Log.w(TAG, "camera ${if (broken) "failed" else "stalled"}: restarting it")
                restarts++
                restart(spec, parts, now)
            }
        }
        val k = parts.getOrNull(5)?.toIntOrNull() ?: 0
        if (spec != null && k != keyframes) {
            keyframes = k
            handler.post { requestKeyframe() }
        }
    }

    private fun restart(spec: String?, parts: List<String>, now: Long) {
        startedAt = now
        lastOutput = 0
        broken = false
        handler.post { stopNow() }
        if (spec != null) {
            val (w, h, fps, bitrate) = parts.take(4).map { it.toIntOrNull() ?: 0 }
            handler.post { startNow(w, h, fps, bitrate) }
        }
    }

    fun close() {
        running = null
        handler.post {
            stopNow()
            thread.quitSafely()
        }
    }

    @SuppressLint("MissingPermission")
    private fun startNow(width: Int, height: Int, fps: Int, bitrate: Int) {
        try {
            val manager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
            // The front camera (the one facing whoever's on the call).
            val id = manager.cameraIdList.firstOrNull {
                manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_FRONT
            } ?: manager.cameraIdList.firstOrNull() ?: return
            val chars = manager.getCameraCharacteristics(id)
            Log.i(TAG, "camera $id: ${width}x$height @ $fps fps, sensor at ${chars.get(CameraCharacteristics.SENSOR_ORIENTATION)}°")

            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
                setInteger(MediaFormat.KEY_FRAME_RATE, fps)
                // A keyframe every 2 s anyway (a host that missed one recovers).
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
                setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
                // No B-frames (each would hold a frame back), realtime, frames out as encoded.
                setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
                setInteger(MediaFormat.KEY_PRIORITY, 0)
                setInteger(MediaFormat.KEY_LATENCY, 1)
                setInteger(MediaFormat.KEY_PREPEND_HEADER_TO_SYNC_FRAMES, 1)
            }
            val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
            codec.setCallback(object : MediaCodec.Callback() {
                override fun onInputBufferAvailable(c: MediaCodec, index: Int) {}

                override fun onOutputBufferAvailable(c: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                    try {
                        val buf = c.getOutputBuffer(index)
                        if (buf != null && info.size > 0) {
                            lastOutput = SystemClock.elapsedRealtime()
                            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                                config = ByteArray(info.size).also { buf.position(info.offset); buf.get(it) }
                            } else {
                                val key = info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0
                                val header = config
                                if (key && header != null && !startsWithHeader(buf, info.offset)) {
                                    // (An encoder that doesn't repeat them: put them in front.)
                                    val whole = java.nio.ByteBuffer.allocateDirect(header.size + info.size)
                                    whole.put(header)
                                    buf.position(info.offset)
                                    buf.limit(info.offset + info.size)
                                    whole.put(buf)
                                    Native.cameraFrame(whole, 0, whole.position(), true)
                                } else {
                                    Native.cameraFrame(buf, info.offset, info.size, key)
                                }
                            }
                        }
                        c.releaseOutputBuffer(index, false)
                    } catch (e: IllegalStateException) {
                        // (Stopped meanwhile.)
                    }
                }

                override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                    Log.w(TAG, "camera encoder: $e")
                    if (encoder === c) broken = true
                }

                override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {}
            }, handler)
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            val surface = codec.createInputSurface()
            codec.start()
            encoder = codec
            inputSurface = surface

            manager.openCamera(id, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    if (encoder !== codec) {
                        device.close()
                        return
                    }
                    camera = device
                    @Suppress("DEPRECATION")
                    device.createCaptureSession(listOf(surface), object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(s: CameraCaptureSession) {
                            if (camera !== device) {
                                s.close()
                                return
                            }
                            session = s
                            val request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                addTarget(surface)
                                // A steady frame rate (the encoder's), not slower in dim light.
                                bestFpsRange(chars, fps)?.let { set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, it) }
                            }
                            s.setRepeatingRequest(request.build(), null, handler)
                        }

                        override fun onConfigureFailed(s: CameraCaptureSession) {
                            Log.w(TAG, "camera session couldn't be configured")
                            if (camera === device) broken = true
                        }
                    }, handler)
                }

                override fun onDisconnected(device: CameraDevice) {
                    Log.w(TAG, "camera taken away")
                    device.close()
                    if (camera === device || (camera == null && encoder === codec)) broken = true
                    if (camera === device) camera = null
                }

                override fun onError(device: CameraDevice, error: Int) {
                    Log.w(TAG, "camera error $error")
                    device.close()
                    if (camera === device || (camera == null && encoder === codec)) broken = true
                    if (camera === device) camera = null
                }
            }, handler)
        } catch (e: Exception) {
            Log.w(TAG, "camera: $e")
            stopNow()
            broken = true
        }
    }

    private fun stopNow() {
        try {
            session?.close()
        } catch (_: Exception) {
        }
        session = null
        try {
            camera?.close()
        } catch (_: Exception) {
        }
        camera = null
        try {
            encoder?.stop()
        } catch (_: Exception) {
        }
        try {
            encoder?.release()
        } catch (_: Exception) {
        }
        encoder = null
        inputSurface?.release()
        inputSurface = null
        config = null
    }

    private fun requestKeyframe() {
        try {
            encoder?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
        } catch (_: Exception) {
        }
    }

    private fun startsWithHeader(buf: java.nio.ByteBuffer, offset: Int): Boolean {
        // Annex-B: 00 00 00 01 or 00 00 01, then an SPS (NAL type 7).
        for (start in listOf(4, 3)) {
            if (buf.limit() > offset + start && buf.get(offset + start).toInt() and 0x1F == 7) return true
        }
        return false
    }

    private fun bestFpsRange(chars: CameraCharacteristics, fps: Int): Range<Int>? {
        val ranges = chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES) ?: return null
        return ranges.filter { it.upper == fps }.maxByOrNull { it.lower } ?: ranges.filter { it.upper >= fps }.minByOrNull { it.upper }
    }

    companion object {
        const val TAG = "rdpio"
        /** No frame for this long: the camera was lost. */
        const val STALL_MS = 3000L
    }
}
