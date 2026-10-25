package io.ioservices.rdpio

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Rect
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.content.res.ColorStateList
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import kotlin.math.abs
import kotlin.math.max
import kotlin.math.min

/**
 * The stream's menu: a round button that floats over the picture (drag it
 * anywhere; it settles on the nearer side and stays there next time) and,
 * tapped, a card of actions beside it, each with its current state.
 */
class StreamMenu(private val root: FrameLayout) {
    private val ctx: Context = root.context
    private val density = ctx.resources.displayMetrics.density
    private fun dp(v: Int) = (v * density).toInt()
    private val prefs = ctx.getSharedPreferences("settings", Context.MODE_PRIVATE)

    private class Item(val label: String, val state: (() -> String)?, val keepOpen: Boolean, val danger: Boolean, val run: () -> Unit) {
        lateinit var stateView: TextView
    }

    private val items = mutableListOf<Item>()
    private val list = LinearLayout(ctx).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(0, 0, 0, dp(8))
    }
    private val panel = ScrollView(ctx).apply {
        background = GradientDrawable().apply { setColor(0xF0181C1F.toInt()); cornerRadius = dp(16).toFloat() }
        clipToOutline = true
        elevation = dp(8).toFloat()
        isVerticalScrollBarEnabled = false
        visibility = View.GONE
        addView(list)
    }
    private val button = Fab()
    private val size = dp(52)
    private val margin = dp(12)
    private val idle = Runnable { if (!isOpen) button.animate().alpha(0.45f).setDuration(400) }

    val isOpen get() = panel.visibility == View.VISIBLE

    init {
        root.addView(panel, FrameLayout.LayoutParams(dp(300), FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.START))
        root.addView(button, FrameLayout.LayoutParams(size, size, Gravity.TOP or Gravity.START))
        button.visibility = View.INVISIBLE
        root.post { place(); button.visibility = View.VISIBLE; wake() }
    }

    /** A heading over the next items. */
    fun section(title: String) {
        list.addView(TextView(ctx).apply {
            text = title.uppercase()
            textSize = 11f
            letterSpacing = 0.08f
            setTextColor(0x99FFFFFF.toInt())
            setPadding(dp(18), dp(if (list.childCount == 0) 14 else 10), dp(18), dp(4))
        })
    }

    /** An action; [state] (shown on the right) is read each time the card opens or something in it is tapped. */
    fun item(label: String, state: (() -> String)? = null, keepOpen: Boolean = state != null, danger: Boolean = false, run: () -> Unit) {
        val item = Item(label, state, keepOpen, danger, run)
        items += item
        list.addView(LinearLayout(ctx).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            minimumHeight = dp(46)
            setPadding(dp(18), 0, dp(18), 0)
            background = RippleDrawable(ColorStateList.valueOf(0x33FFFFFF), null, GradientDrawable().apply { setColor(Color.WHITE) })
            isClickable = true
            addView(TextView(ctx).apply {
                text = label
                textSize = 15f
                setTextColor(if (danger) 0xFFFF7A6E.toInt() else Color.WHITE)
            }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
            item.stateView = TextView(ctx).apply {
                textSize = 13f
                typeface = Typeface.DEFAULT_BOLD
                setPadding(dp(10), dp(3), dp(10), dp(3))
                visibility = if (state == null) View.GONE else View.VISIBLE
            }
            addView(item.stateView)
            setOnClickListener {
                item.run()
                if (item.keepOpen) refresh() else close()
            }
        })
    }

    fun toggle() = if (isOpen) close() else open()

    fun open() {
        refresh()
        panel.alpha = 0f
        panel.visibility = View.VISIBLE
        button.open = true
        button.alpha = 1f
        button.invalidate()
        panel.post {
            placePanel()
            panel.scaleX = 0.96f
            panel.scaleY = 0.96f
            panel.animate().alpha(1f).scaleX(1f).scaleY(1f).setDuration(140)
        }
    }

    fun close() {
        if (!isOpen) return
        panel.visibility = View.GONE
        button.open = false
        button.invalidate()
        wake()
    }

    /** A touch at the root's (x, y) lands on the menu (the button or the open card). */
    fun hits(x: Int, y: Int): Boolean {
        val r = Rect()
        button.getHitRect(r)
        if (r.contains(x, y)) return true
        if (!isOpen) return false
        panel.getHitRect(r)
        return r.contains(x, y)
    }

    /** The screen changed size (rotation, split screen): keep the button on it. */
    fun relayout() = root.post {
        place()
        if (isOpen) placePanel()
    }

    private fun refresh() = items.forEach { i ->
        val s = i.state?.invoke() ?: return@forEach
        i.stateView.text = s
        val on = s == "On"
        i.stateView.setTextColor(if (on) 0xFF0E1A12.toInt() else 0xCCFFFFFF.toInt())
        i.stateView.background = GradientDrawable().apply {
            setColor(if (on) 0xFF6FD08C.toInt() else 0x22FFFFFF)
            cornerRadius = dp(10).toFloat()
        }
    }

    /** The button where it was left: a side, and how far down (a fraction, so it survives rotation). */
    private fun place() {
        val w = root.width
        val h = root.height
        if (w == 0 || h == 0) return
        val right = prefs.getBoolean("menuRight", true)
        val frac = prefs.getFloat("menuY", 0.12f)
        button.x = if (right) (w - size - margin).toFloat() else margin.toFloat()
        button.y = (margin + frac * (h - size - 2 * margin)).coerceIn(margin.toFloat(), (h - size - margin).toFloat())
    }

    private fun settle() {
        val w = root.width
        val h = root.height
        val right = button.x + size / 2 > w / 2
        val y = button.y.coerceIn(margin.toFloat(), (h - size - margin).toFloat())
        prefs.edit()
            .putBoolean("menuRight", right)
            .putFloat("menuY", if (h > size + 2 * margin) (y - margin) / (h - size - 2 * margin) else 0f)
            .apply()
        button.animate().x(if (right) (w - size - margin).toFloat() else margin.toFloat()).y(y).setDuration(180)
        wake()
    }

    /** The card beside the button, on the side with the room, kept on screen. */
    private fun placePanel() {
        val w = root.width
        val h = root.height
        val pw = panel.width
        // (Taller than the screen: the card scrolls.)
        val ph = min(list.height, h - 2 * margin)
        if (panel.layoutParams.height != ph) panel.layoutParams = panel.layoutParams.apply { height = ph }
        val right = button.x + size / 2 > w / 2
        panel.pivotX = if (right) pw.toFloat() else 0f
        panel.pivotY = ph / 2f
        panel.x = if (right) button.x - pw - dp(10) else button.x + size + dp(10)
        panel.y = (button.y + size / 2 - ph / 2).coerceIn(margin.toFloat(), max(margin, h - ph - margin).toFloat())
    }

    /** Full strength for a while after use, then faded so it hides less of the picture. */
    private fun wake() {
        button.removeCallbacks(idle)
        button.animate().alpha(1f).setDuration(120)
        button.postDelayed(idle, 3000)
    }

    /** The round button: three dots (a cross when the card is open); drag to move, tap to open. */
    private inner class Fab : View(ctx) {
        var open = false
        private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xE0181C1F.toInt() }
        private val ring = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0x55FFFFFF
            style = Paint.Style.STROKE
            strokeWidth = dp(1).toFloat()
        }
        private val ink = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = Color.WHITE
            strokeWidth = dp(2).toFloat()
            strokeCap = Paint.Cap.ROUND
        }
        private val slop = ViewConfiguration.get(ctx).scaledTouchSlop
        private var downX = 0f
        private var downY = 0f
        private var startX = 0f
        private var startY = 0f
        private var dragging = false

        init {
            elevation = dp(6).toFloat()
            contentDescription = "Menu"
            isClickable = true
        }

        override fun onDraw(c: Canvas) {
            val r = width / 2f
            c.drawCircle(r, r, r - dp(1), fill)
            c.drawCircle(r, r, r - dp(1), ring)
            if (open) {
                val a = dp(7).toFloat()
                c.drawLine(r - a, r - a, r + a, r + a, ink)
                c.drawLine(r - a, r + a, r + a, r - a, ink)
            } else {
                val gap = dp(7).toFloat()
                for (i in -1..1) c.drawCircle(r + i * gap, r, dp(2).toFloat() + 0.5f, ink)
            }
        }

        override fun onTouchEvent(e: MotionEvent): Boolean {
            when (e.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    downX = e.rawX
                    downY = e.rawY
                    startX = x
                    startY = y
                    dragging = false
                    removeCallbacks(idle)
                    animate().cancel()
                    alpha = 1f
                }
                MotionEvent.ACTION_MOVE -> {
                    val dx = e.rawX - downX
                    val dy = e.rawY - downY
                    if (!dragging && (abs(dx) > slop || abs(dy) > slop)) {
                        dragging = true
                        close()
                    }
                    if (dragging) {
                        x = (startX + dx).coerceIn(0f, (root.width - size).toFloat())
                        y = (startY + dy).coerceIn(0f, (root.height - size).toFloat())
                    }
                }
                MotionEvent.ACTION_UP -> if (dragging) settle() else { performClick(); toggle() }
                MotionEvent.ACTION_CANCEL -> if (dragging) settle() else wake()
            }
            return true
        }

        override fun onHoverEvent(e: MotionEvent): Boolean {
            if (e.actionMasked == MotionEvent.ACTION_HOVER_ENTER) {
                removeCallbacks(idle)
                animate().alpha(1f).setDuration(120)
            } else if (e.actionMasked == MotionEvent.ACTION_HOVER_EXIT) {
                wake()
            }
            return super.onHoverEvent(e)
        }
    }
}
