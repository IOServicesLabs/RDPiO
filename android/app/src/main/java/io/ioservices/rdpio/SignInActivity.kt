package io.ioservices.rdpio

import android.app.Activity
import android.content.Intent
import android.graphics.Bitmap
import android.net.Uri
import android.os.Bundle
import android.webkit.CookieManager
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient

/**
 * The Microsoft sign-in page for Windows 365. It ends by sending the browser
 * to the redirect address with a `code`; we catch that and hand the code back
 * (result extra "code", or "error").
 */
class SignInActivity : Activity() {
    private var redirect = ""
    private var done = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val (url, redir) = Native.w365SignInUrl().split('\n', limit = 2).let { it[0] to it.getOrElse(1) { "" } }
        redirect = redir
        val web = WebView(this)
        web.settings.javaScriptEnabled = true
        web.settings.domStorageEnabled = true
        CookieManager.getInstance().setAcceptCookie(true)
        web.webViewClient = object : WebViewClient() {
            override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean =
                caught(request.url.toString())

            override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) {
                if (caught(url)) view.stopLoading()
            }
        }
        setContentView(web)
        web.loadUrl(url)
    }

    /** True when [url] is the redirect (and we've finished with it). */
    private fun caught(url: String): Boolean {
        if (redirect.isEmpty() || !url.startsWith(redirect)) return false
        if (done) return true
        done = true
        val u = Uri.parse(url)
        val code = u.getQueryParameter("code")
        val out = Intent()
        if (code.isNullOrEmpty()) {
            out.putExtra("error", u.getQueryParameter("error_description") ?: u.getQueryParameter("error") ?: "no code from Microsoft")
        } else {
            out.putExtra("code", code)
        }
        setResult(RESULT_OK, out)
        finish()
        return true
    }
}
