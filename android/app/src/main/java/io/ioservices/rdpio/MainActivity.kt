package io.ioservices.rdpio

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.EditorInfo
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Spinner
import android.widget.Switch
import android.widget.TextView
import kotlin.concurrent.thread

/** The connect screen: saved PCs, and the form for a new one. */
class MainActivity : Activity() {
    private lateinit var host: EditText
    private lateinit var port: EditText
    private lateinit var password: EditText
    private lateinit var remember: Switch
    private lateinit var office: Switch
    private lateinit var fps: Spinner
    private lateinit var resolution: Spinner
    private lateinit var scale: Spinner
    private lateinit var codec: Spinner
    private lateinit var connect: Button
    private lateinit var message: TextView
    private lateinit var saved: LinearLayout
    private lateinit var resume: Button
    private lateinit var cloud: LinearLayout

    /** Windows 365: the signed-in account ("" when not), its Cloud PCs, and what's happening. */
    private var cloudUser = ""
    /** (name, key: an imported one's, else ""). */
    private var cloudPcs = listOf<Pair<String, String>>()
    private var cloudStatus = ""
    private var cloudError = ""

    private var fpsChoices = listOf(0)
    private val resolutionChoices = listOf(100, 80, 64, 50)
    private var scaleChoices = listOf(0)
    private var codecChoices = listOf("auto")
    private var busy = false

    private val ink get() = getColor(R.color.ink)
    private val panel get() = getColor(R.color.panel)
    private val line get() = getColor(R.color.line)
    private val textColor get() = getColor(R.color.text)
    private val muted get() = getColor(R.color.muted)
    private val accent get() = getColor(R.color.accent)

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private companion object {
        const val SIGN_IN = 2
        const val IMPORT = 3
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Native.setDataDir(filesDir.path)
        val (w, h) = Screen.size(this)
        val maxHz = Screen.maxRefresh(this)
        fpsChoices = listOf(0) + listOf(144, 120, 90, 60, 30).filter { it < maxHz }
        scaleChoices = listOf(0, 100, 125, 150, 175, 200, 225, 250)
        val hw = Native.codecs().split(',').filter { it.isNotBlank() }
        codecChoices = listOf("auto") + hw

        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(24), dp(28), dp(24), dp(28))
        }
        val content = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            layoutParams = LinearLayout.LayoutParams(dp(560).coerceAtMost(resources.displayMetrics.widthPixels - dp(32)), ViewGroup.LayoutParams.WRAP_CONTENT)
        }
        column.gravity = Gravity.CENTER_HORIZONTAL
        column.addView(content)

        content.addView(TextView(this).apply {
            text = "rdpio"
            textSize = 30f
            typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
            setTextColor(textColor)
        })
        content.addView(TextView(this).apply {
            text = "Hyper stream from your PC to this tablet · ${w}×${h} · up to $maxHz Hz · ${hw.joinToString(" / ").uppercase().ifEmpty { "no hardware decoder" }}"
            setTextColor(muted)
            textSize = 13f
            setPadding(0, dp(2), 0, dp(20))
        })

        resume = Button(this).apply {
            isAllCaps = false
            textSize = 16f
            setTextColor(ink)
            background = rounded(accent, 6)
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(52)).apply { bottomMargin = dp(12) }
            visibility = View.GONE
            setOnClickListener { startActivity(Intent(this@MainActivity, StreamActivity::class.java)) }
        }
        content.addView(resume)

        saved = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        content.addView(saved)

        val form = card()
        content.addView(form)
        form.addView(label("PC"))
        host = field("Name or address (Tailscale, LAN)", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI)
        form.addView(host)
        form.addView(label("Port"))
        port = field("47998", InputType.TYPE_CLASS_NUMBER).apply { setText("47998") }
        form.addView(port)
        form.addView(label("Hyper password"))
        password = field("Set on the PC with rdpio --hyper-set-password", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD).apply {
            imeOptions = EditorInfo.IME_ACTION_GO
            setOnEditorActionListener { _, id, _ -> if (id == EditorInfo.IME_ACTION_GO) { go(null); true } else false }
        }
        form.addView(password)
        remember = switch("Remember the password", true)
        form.addView(remember)
        office = switch("Office mode (crisp text, 60 fps) instead of game mode", false)
        form.addView(office)

        form.addView(label("Frame rate"))
        fps = spinner(fpsChoices.map { if (it == 0) "This screen's fastest ($maxHz Hz)" else "$it fps" })
        form.addView(fps)
        form.addView(label("Resolution"))
        resolution = spinner(resolutionChoices.map { pct -> "${w * pct / 100}×${h * pct / 100}" + if (pct == 100) " (this screen)" else "" })
        form.addView(resolution)
        form.addView(label("Text size on the PC"))
        scale = spinner(scaleChoices.map { if (it == 0) "Automatic (${Screen.autoScale(this)}%)" else "$it%" })
        form.addView(scale)
        form.addView(label("Video codec"))
        codec = spinner(codecChoices.map { if (it == "auto") "Automatic" else it.uppercase() })
        form.addView(codec)

        connect = Button(this).apply {
            text = "Connect"
            isAllCaps = false
            textSize = 16f
            setTextColor(ink)
            background = rounded(accent, 6)
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(52)).apply { topMargin = dp(20) }
            setOnClickListener { go(null) }
        }
        form.addView(connect)
        message = TextView(this).apply {
            setTextColor(getColor(R.color.danger))
            textSize = 14f
            setPadding(0, dp(12), 0, 0)
            visibility = View.GONE
        }
        form.addView(message)

        cloud = card()
        content.addView(cloud)

        content.addView(TextView(this).apply {
            text = "On the PC: install rdpio and its Hyper service (rdpio --hyper-service-install, once, as admin), " +
                "set the password with rdpio --hyper-set-password, and use the same rdpio version as this app (${packageManager.getPackageInfo(packageName, 0).versionName}).\n\n" +
                "Keyboard: Ctrl+Shift+M locks the mouse (games), Ctrl+Shift+O game/office, Ctrl+Shift+S stats, Ctrl+Shift+Tab switch session, Ctrl+Shift+Q disconnect, Shift+Esc the menu, Ctrl+Alt+End = Ctrl+Alt+Del."
            setTextColor(muted)
            textSize = 12f
            setPadding(dp(4), dp(18), dp(4), 0)
        })

        setContentView(ScrollView(this).apply {
            setBackgroundColor(ink)
            isFillViewport = true
            addView(column)
        })
        showSaved()
        Store.load(this).firstOrNull()?.let { fill(it) }
        cloudUser = CloudStore.user(this)
        CloudStore.imported(this).forEach { Native.w365Import(it) }
        val refresh = CloudStore.refresh(this)
        if (refresh != null) signIn(refresh, true) else showCloud()
    }

    override fun onResume() {
        super.onResume()
        showSaved()
        val open = Native.sessions().split(',').filter { it.isNotEmpty() }
        resume.visibility = if (open.isEmpty()) View.GONE else View.VISIBLE
        resume.text = "Back to " + open.joinToString(" / ") { if (it == "w365") "Windows 365" else "the Hyper stream" }
        intent?.getStringExtra("ended")?.let { why ->
            intent.removeExtra("ended")
            if (!why.startsWith("closed")) showMessage("Stream ended: $why")
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
    }

    private fun showSaved() {
        saved.removeAllViews()
        val pcs = Store.load(this)
        if (pcs.isEmpty()) return
        saved.addView(label("Saved PCs"))
        pcs.forEach { pc ->
            saved.addView(LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                background = rounded(panel, 6, line)
                setPadding(dp(16), dp(12), dp(16), dp(12))
                layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply { bottomMargin = dp(8) }
                addView(TextView(context).apply {
                    text = pc.host
                    textSize = 17f
                    setTextColor(textColor)
                })
                addView(TextView(context).apply {
                    text = listOfNotNull(
                        if (pc.office) "Office mode" else "Game mode",
                        if (pc.password.isNotEmpty()) "password saved" else "asks for the password",
                        if (pc.port != 47998) "port ${pc.port}" else null,
                    ).joinToString(" · ") + "   — tap to connect, hold to edit"
                    textSize = 12f
                    setTextColor(muted)
                })
                isClickable = true
                setOnClickListener {
                    fill(pc)
                    if (pc.password.isNotEmpty()) go(pc) else password.requestFocus()
                }
                setOnLongClickListener {
                    AlertDialog.Builder(this@MainActivity)
                        .setTitle(pc.host)
                        .setItems(arrayOf("Edit", "Forget")) { _, which ->
                            if (which == 0) fill(pc) else {
                                Store.forget(this@MainActivity, pc)
                                showSaved()
                            }
                        }
                        .show()
                    true
                }
            })
        }
    }

    private fun fill(pc: Pc) {
        host.setText(pc.host)
        port.setText(pc.port.toString())
        password.setText(if (pc.password.isNotEmpty()) Secret.open(pc.password) ?: "" else "")
        remember.isChecked = pc.password.isNotEmpty() || password.text.isEmpty()
        office.isChecked = pc.office
        fps.setSelection(fpsChoices.indexOf(pc.fps).coerceAtLeast(0))
        resolution.setSelection(resolutionChoices.indexOf(pc.resolution).coerceAtLeast(0))
        scale.setSelection(scaleChoices.indexOf(pc.scale).coerceAtLeast(0))
        codec.setSelection(codecChoices.indexOf(pc.codec).coerceAtLeast(0))
    }

    /** Sign in with the form (or [saved]'s settings), then open the stream. */
    private fun go(saved: Pc?) {
        if (busy) return
        val h = host.text.toString().trim()
        val pw = password.text.toString()
        if (h.isEmpty()) return showMessage("Enter the PC's name or address.")
        if (pw.isEmpty()) return showMessage("Enter the Hyper password.")
        val pc = Pc(
            host = h,
            port = port.text.toString().toIntOrNull() ?: 47998,
            password = if (remember.isChecked) Secret.seal(pw) else "",
            office = office.isChecked,
            fps = fpsChoices[fps.selectedItemPosition.coerceAtLeast(0)],
            resolution = resolutionChoices[resolution.selectedItemPosition.coerceAtLeast(0)],
            scale = scaleChoices[scale.selectedItemPosition.coerceAtLeast(0)],
            codec = codecChoices[codec.selectedItemPosition.coerceAtLeast(0)],
        )
        val (sw, sh) = Screen.size(this)
        // (Even sizes: the encoder works in 2x2 blocks of colour.)
        val w = sw * pc.resolution / 100 and 1.inv()
        val ht = sh * pc.resolution / 100 and 1.inv()
        val hz = if (pc.fps == 0) Screen.maxRefresh(this) else pc.fps
        val sc = if (pc.scale == 0) Screen.autoScale(this) else pc.scale
        busy = true
        connect.isEnabled = false
        connect.text = "Signing in…"
        message.visibility = View.GONE
        thread(name = "sign-in") {
            val err = Native.connect(pc.host, pc.port, pw, w, ht, hz, sc, pc.office, pc.codec)
            runOnUiThread {
                busy = false
                connect.isEnabled = true
                connect.text = "Connect"
                if (err.isEmpty()) {
                    Store.remember(this, pc)
                    startActivity(Intent(this, StreamActivity::class.java).putExtra("fps", hz).putExtra("host", pc.host))
                } else {
                    if (err.contains("wrong Hyper password") && saved != null) {
                        Store.remember(this, saved.copy(password = ""))
                        password.setText("")
                    }
                    showMessage(err)
                }
            }
        }
    }

    // ---- Windows 365 ----

    @Deprecated("Activity result")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == IMPORT && resultCode == RESULT_OK && data != null) return importFiles(data)
        if (requestCode != SIGN_IN || data == null) return
        val code = data.getStringExtra("code")
        if (code != null) signIn(code, false) else {
            cloudError = data.getStringExtra("error") ?: "Sign-in cancelled."
            showCloud()
        }
    }

    /** Sign in with a code from [SignInActivity], or again with the kept refresh token; then list the Cloud PCs. */
    private fun signIn(code: String, refresh: Boolean) {
        cloudStatus = "Signing in…"
        cloudError = ""
        showCloud()
        thread(name = "w365-sign-in") {
            val r = Native.w365SignIn(code, refresh).split('\n')
            if (r[0] != "ok") {
                runOnUiThread {
                    cloudStatus = ""
                    cloudError = r.getOrElse(1) { "Sign-in failed." }
                    if (refresh) cloudError = "Sign in again: $cloudError"
                    showCloud()
                }
                return@thread
            }
            val user = r.getOrElse(1) { "" }
            CloudStore.signedIn(this, user, r.getOrElse(2) { "" })
            runOnUiThread {
                cloudUser = user
                listCloud()
            }
        }
    }

    /** List the Cloud PCs (the imported ones, else the feed's). */
    private fun listCloud() {
        cloudStatus = "Finding your Cloud PCs…"
        cloudError = ""
        showCloud()
        thread(name = "w365-list") {
            val l = Native.w365List().split('\n')
            runOnUiThread {
                cloudStatus = ""
                if (l[0] == "ok") {
                    cloudPcs = l.drop(1).filter { it.isNotBlank() }.map { it.substringBefore('\t') to it.substringAfter('\t', "") }
                    if (cloudPcs.isEmpty()) cloudError = "No Cloud PCs on this account."
                } else {
                    cloudPcs = emptyList()
                    cloudError = l.getOrElse(1) { "Couldn't list the Cloud PCs." }
                }
                showCloud()
            }
        }
    }

    /** Cloud PCs from `.rdp` files (or the Windows App's cache files) the user picked. */
    private fun importFiles(data: Intent) {
        val uris = data.clipData?.let { c -> (0 until c.itemCount).map { c.getItemAt(it).uri } } ?: listOfNotNull(data.data)
        val bad = mutableListOf<String>()
        uris.forEach { uri ->
            val bytes = try {
                contentResolver.openInputStream(uri)?.use { it.readBytes() }
            } catch (_: Exception) {
                null
            }
            val r = bytes?.let { Native.w365Import(it).split('\n') }
            if (bytes != null && r?.getOrNull(0) == "ok") CloudStore.saveImported(this, r[1], bytes)
            else bad += r?.getOrNull(1) ?: "couldn't read the file"
        }
        listCloud()
        if (bad.isNotEmpty()) showMessage("Not imported: " + bad.distinct().joinToString("; "))
    }

    private fun showCloud() {
        cloud.removeAllViews()
        cloud.addView(label("Windows 365"))
        if (cloudUser.isEmpty() && cloudStatus.isEmpty()) {
            cloud.addView(note("Your Cloud PCs, with this tablet's mic and camera."))
            cloud.addView(button("Sign in") {
                @Suppress("DEPRECATION")
                startActivityForResult(Intent(this, SignInActivity::class.java), SIGN_IN)
            })
        } else {
            if (cloudUser.isNotEmpty()) cloud.addView(note(cloudUser))
            cloudPcs.forEachIndexed { i, (name, key) ->
                cloud.addView(LinearLayout(this).apply {
                    orientation = LinearLayout.VERTICAL
                    background = rounded(ink, 6, line)
                    setPadding(dp(16), dp(12), dp(16), dp(12))
                    layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply { topMargin = dp(8) }
                    addView(TextView(context).apply {
                        text = name
                        textSize = 17f
                        setTextColor(textColor)
                    })
                    addView(TextView(context).apply {
                        text = (if (CloudStore.password(this@MainActivity) != null) "password saved" else "asks for the password") + "   — tap to connect"
                        textSize = 12f
                        setTextColor(muted)
                    })
                    isClickable = true
                    setOnClickListener { connectCloud(i, name) }
                    if (key.isNotEmpty()) setOnLongClickListener {
                        AlertDialog.Builder(this@MainActivity)
                            .setTitle(name)
                            .setItems(arrayOf("Forget")) { _, _ ->
                                Native.w365Forget(key)
                                CloudStore.forgetImported(this@MainActivity, key)
                                listCloud()
                            }
                            .show()
                        true
                    }
                })
            }
            if (cloudStatus.isEmpty()) {
                cloud.addView(TextView(this).apply {
                    text = "Import .rdp file"
                    textSize = 14f
                    setTextColor(accent)
                    setPadding(0, dp(14), 0, 0)
                    setOnClickListener {
                        val pick = Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                            .setType("*/*").putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
                        @Suppress("DEPRECATION")
                        startActivityForResult(pick, IMPORT)
                    }
                })
                cloud.addView(TextView(this).apply {
                    text = "Sign out"
                    textSize = 14f
                    setTextColor(accent)
                    setPadding(0, dp(14), 0, 0)
                    setOnClickListener {
                        Native.w365SignOut()
                        CloudStore.signOut(this@MainActivity)
                        cloudUser = ""
                        cloudPcs = emptyList()
                        cloudError = ""
                        showCloud()
                    }
                })
            }
        }
        if (cloudStatus.isNotEmpty()) cloud.addView(note(cloudStatus))
        if (cloudError.isNotEmpty()) cloud.addView(TextView(this).apply {
            text = cloudError
            setTextColor(getColor(R.color.danger))
            textSize = 14f
            setPadding(0, dp(12), 0, 0)
        })
    }

    /** The Cloud PC's sign-in needs the account's password (RDSTLS): the kept one, or ask. */
    private fun connectCloud(index: Int, name: String) {
        if (busy) return
        CloudStore.password(this)?.let { return startCloud(index, name, it, true) }
        val pw = field("Password for $cloudUser", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD)
        val keep = switch("Remember the password", true)
        val box = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(8), dp(20), 0)
            addView(pw)
            addView(keep)
        }
        AlertDialog.Builder(this)
            .setTitle(name)
            .setView(box)
            .setPositiveButton("Connect") { _, _ ->
                val p = pw.text.toString()
                if (p.isNotEmpty()) {
                    if (keep.isChecked) CloudStore.rememberPassword(this, p)
                    startCloud(index, name, p, keep.isChecked)
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    private fun startCloud(index: Int, name: String, password: String, kept: Boolean) {
        val (sw, sh) = Screen.size(this)
        busy = true
        cloudStatus = "Connecting to $name…"
        cloudError = ""
        showCloud()
        Native.setDesktopScale(Screen.cloudScale(this))
        thread(name = "w365-connect") {
            val err = Native.w365Connect(index, password, sw and 1.inv(), sh and 1.inv())
            runOnUiThread {
                busy = false
                cloudStatus = ""
                if (err.isEmpty()) {
                    showCloud()
                    startActivity(Intent(this, StreamActivity::class.java).putExtra("fps", 60).putExtra("host", name))
                } else {
                    // A refused password isn't kept (the next tap asks again).
                    if (kept && Regex("password|logon|credential|denied", RegexOption.IGNORE_CASE).containsMatchIn(err)) {
                        CloudStore.rememberPassword(this, null)
                    }
                    cloudError = err
                    showCloud()
                }
            }
        }
    }

    private fun note(s: String) = TextView(this).apply {
        text = s
        setTextColor(muted)
        textSize = 14f
        setPadding(0, 0, 0, dp(4))
    }

    private fun button(s: String, f: () -> Unit) = Button(this).apply {
        text = s
        isAllCaps = false
        textSize = 16f
        setTextColor(ink)
        background = rounded(accent, 6)
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(52)).apply { topMargin = dp(12) }
        setOnClickListener { f() }
    }

    private fun showMessage(s: String) {
        message.text = s
        message.visibility = View.VISIBLE
    }

    // ---- Small view helpers ----

    private fun rounded(fill: Int, radiusDp: Int, stroke: Int? = null) = GradientDrawable().apply {
        setColor(fill)
        cornerRadius = dp(radiusDp).toFloat()
        stroke?.let { setStroke(dp(1), it) }
    }

    private fun card() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        background = rounded(panel, 6, line)
        setPadding(dp(18), dp(10), dp(18), dp(18))
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply { topMargin = dp(8) }
    }

    private fun label(s: String) = TextView(this).apply {
        text = s
        textSize = 12f
        setTextColor(muted)
        isAllCaps = true
        letterSpacing = 0.06f
        setPadding(0, dp(14), 0, dp(6))
    }

    private fun field(hint: String, type: Int) = EditText(this).apply {
        this.hint = hint
        inputType = type
        isSingleLine = true
        textSize = 16f
        setTextColor(textColor)
        setHintTextColor(muted)
        background = rounded(ink, 4, line)
        setPadding(dp(12), dp(10), dp(12), dp(10))
    }

    private fun switch(s: String, on: Boolean) = Switch(this).apply {
        text = s
        isChecked = on
        textSize = 14f
        setTextColor(textColor)
        setPadding(0, dp(14), 0, 0)
    }

    private fun spinner(items: List<String>) = Spinner(this).apply {
        adapter = ArrayAdapter(context, android.R.layout.simple_spinner_dropdown_item, items)
        background = rounded(ink, 4, line)
        setPadding(dp(4), dp(2), dp(4), dp(2))
        onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
            override fun onItemSelected(p: AdapterView<*>?, v: View?, pos: Int, id: Long) {
                (v as? TextView)?.setTextColor(textColor)
            }
            override fun onNothingSelected(p: AdapterView<*>?) {}
        }
    }
}
