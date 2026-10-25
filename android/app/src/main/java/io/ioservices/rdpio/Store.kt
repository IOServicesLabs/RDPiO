package io.ioservices.rdpio

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import org.json.JSONArray
import org.json.JSONObject
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** One PC to connect to, and how. */
data class Pc(
    val host: String,
    val port: Int = 47998,
    /** Sealed with the Keystore key (see [Secret]); "" when not remembered. */
    val password: String = "",
    val office: Boolean = false,
    /** 0: this screen's fastest. */
    val fps: Int = 0,
    /** Percent of this screen's resolution (100, 80, 64). */
    val resolution: Int = 100,
    /** Display scale for the PC (percent); 0: from this screen's density. */
    val scale: Int = 0,
    /** "auto", "hevc" or "h264". */
    val codec: String = "auto",
) {
    fun toJson(): JSONObject = JSONObject()
        .put("host", host).put("port", port).put("password", password).put("office", office)
        .put("fps", fps).put("resolution", resolution).put("scale", scale).put("codec", codec)

    companion object {
        fun fromJson(o: JSONObject) = Pc(
            host = o.optString("host"),
            port = o.optInt("port", 47998),
            password = o.optString("password"),
            office = o.optBoolean("office"),
            fps = o.optInt("fps"),
            resolution = o.optInt("resolution", 100),
            scale = o.optInt("scale"),
            codec = o.optString("codec", "auto"),
        )
    }
}

/** Saved PCs, most recent first. */
object Store {
    private const val FILE = "rdpio"
    private const val KEY = "pcs"

    fun load(ctx: Context): List<Pc> {
        val raw = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE).getString(KEY, null) ?: return emptyList()
        return try {
            val a = JSONArray(raw)
            (0 until a.length()).map { Pc.fromJson(a.getJSONObject(it)) }.filter { it.host.isNotBlank() }
        } catch (e: Exception) {
            emptyList()
        }
    }

    fun save(ctx: Context, pcs: List<Pc>) {
        val a = JSONArray()
        pcs.forEach { a.put(it.toJson()) }
        ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE).edit().putString(KEY, a.toString()).apply()
    }

    /** [pc] first, replacing an entry for the same host and port. */
    fun remember(ctx: Context, pc: Pc) {
        save(ctx, listOf(pc) + load(ctx).filterNot { it.host == pc.host && it.port == pc.port })
    }

    fun forget(ctx: Context, pc: Pc) {
        save(ctx, load(ctx).filterNot { it.host == pc.host && it.port == pc.port })
    }
}

/** The Windows 365 account: its refresh token and (if remembered) password, both sealed. */
object CloudStore {
    private const val FILE = "rdpio-w365"
    private fun prefs(ctx: Context) = ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    fun refresh(ctx: Context): String? = prefs(ctx).getString("refresh", null)?.let { Secret.open(it) }
    fun user(ctx: Context): String = prefs(ctx).getString("user", "") ?: ""
    fun password(ctx: Context): String? = prefs(ctx).getString("password", null)?.let { Secret.open(it) }

    fun signedIn(ctx: Context, user: String, refresh: String) {
        prefs(ctx).edit().putString("user", user).putString("refresh", Secret.seal(refresh)).apply()
    }

    fun rememberPassword(ctx: Context, password: String?) {
        prefs(ctx).edit().apply { if (password == null) remove("password") else putString("password", Secret.seal(password)) }.apply()
    }

    fun signOut(ctx: Context) {
        prefs(ctx).edit().clear().apply()
        dir(ctx).listFiles()?.forEach { it.delete() }
    }

    // Imported Cloud PCs: each file as it came, named by its key.
    private fun dir(ctx: Context) = java.io.File(ctx.filesDir, "cloudpcs").apply { mkdirs() }

    fun imported(ctx: Context): List<ByteArray> = dir(ctx).listFiles()?.map { it.readBytes() } ?: emptyList()

    fun saveImported(ctx: Context, key: String, bytes: ByteArray) {
        java.io.File(dir(ctx), "$key.rdp").writeBytes(bytes)
    }

    fun forgetImported(ctx: Context, key: String) {
        java.io.File(dir(ctx), "$key.rdp").delete()
    }
}

/** Passwords at rest: AES-GCM under a key that never leaves the Android Keystore. */
object Secret {
    private const val ALIAS = "rdpio-hyper-passwords"

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .build()
        )
        return gen.generateKey()
    }

    fun seal(text: String): String {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key())
        return Base64.encodeToString(c.iv + c.doFinal(text.toByteArray()), Base64.NO_WRAP)
    }

    fun open(blob: String): String? = try {
        val b = Base64.decode(blob, Base64.NO_WRAP)
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, b, 0, 12))
        String(c.doFinal(b, 12, b.size - 12))
    } catch (e: Exception) {
        null
    }
}
