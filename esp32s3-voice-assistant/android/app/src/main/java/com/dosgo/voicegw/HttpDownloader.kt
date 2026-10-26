package com.dosgo.voicegw

import java.io.File
import java.io.FileOutputStream
import java.net.HttpURLConnection
import java.net.URL

/**
 * 真正干活的下载实现 —— 不碰任何 Android API，所以能在普通 JVM 单元测试里跑。
 *
 * 单独拆出来的原因：这里有几个特别容易写错、又特别难在手机上复现的点，
 * 必须能自动化测（见 `HttpDownloaderTest`）：
 *
 *   1. HF 的 `resolve/` 会连跳两次（308 → 302 → CDN），而 Android 的
 *      HttpURLConnection 对 308 支持不可靠，跟随跳转时也不保证保留请求头。
 *      Range 一丢，断点续传就退化成每次从头下。
 *   2. 服务器可能接受 Range（206），也可能直接给全量（200）。
 *      200 时必须**覆盖**而不是追加，否则文件会变成"半截旧文件 + 全新文件"。
 *   3. 长度不对的 .onnx 会让 native 层直接崩，报错毫无线索，必须提前卡住。
 */
object HttpDownloader {

    data class Outcome(
        val ok: Boolean,
        val httpCode: Int,
        /** 服务器给的总字节数，-1 表示没给 */
        val totalBytes: Long,
        val error: String? = null,
    )

    private const val MAX_REDIRECTS = 6
    private const val BUF_SIZE = 64 * 1024

    /**
     * 把 [url] 下到 [part]（可续传）。成功返回 ok=true，文件已完整。
     *
     * 失败时**保留** [part]，下次调用会自动从它的长度继续。
     * 只有两种情况会删 [part]：服务器明确 4xx（路径不对，半截没用）、长度校验不过（数据坏了）。
     */
    fun downloadTo(
        url: String,
        part: File,
        cancelled: () -> Boolean = { false },
        onProgress: (done: Long, total: Long) -> Unit = { _, _ -> },
    ): Outcome {
        part.parentFile?.mkdirs()
        var existing = if (part.exists()) part.length() else 0L

        var conn: HttpURLConnection? = null
        try {
            conn = openWithRedirects(url, existing)
            val code = conn.responseCode

            if (code !in 200..299) {
                conn.disconnect()
                if (code in 400..499) {
                    part.delete()
                    existing = 0
                }
                return Outcome(false, code, -1, "HTTP $code")
            }

            // 206 = 服务器接受 Range，可以接着写；200 = 从头给，必须覆盖
            val append = code == 206 && existing > 0
            if (!append) existing = 0

            val remaining = conn.contentLengthLong
            val totalBytes = if (remaining > 0) existing + remaining else -1L

            var written = existing
            var lastPost = 0L
            var lastFlush = 0L
            val buf = ByteArray(BUF_SIZE)

            FileOutputStream(part, append).use { out ->
                conn.inputStream.use { input ->
                    while (true) {
                        if (cancelled()) {
                            out.flush()
                            conn.disconnect()
                            return Outcome(false, code, totalBytes, "cancelled")
                        }
                        val n = input.read(buf)
                        if (n < 0) break
                        out.write(buf, 0, n)
                        written += n

                        val now = System.currentTimeMillis()
                        if (now - lastFlush > 1000) {
                            out.flush()
                            lastFlush = now
                        }
                        if (now - lastPost > 300) {
                            lastPost = now
                            onProgress(written, totalBytes)
                        }
                    }
                }
            }
            conn.disconnect()
            onProgress(written, totalBytes)

            if (totalBytes > 0 && part.length() != totalBytes) {
                part.delete()
                return Outcome(false, code, totalBytes, "大小不匹配（${part.length()} / $totalBytes）")
            }
            if (part.length() == 0L) {
                part.delete()
                return Outcome(false, code, totalBytes, "下载到 0 字节")
            }
            return Outcome(true, code, totalBytes)
        } catch (t: Throwable) {
            runCatching { conn?.disconnect() }
            return Outcome(false, -1, -1, "${t.javaClass.simpleName}: ${t.message}")
        }
    }

    /**
     * 手动跟随 3xx，每一跳都重新带上 Range。
     *
     * 关掉 `instanceFollowRedirects` 就是为了这个：自动跳转不保证把 Range 带过去。
     */
    private fun openWithRedirects(url: String, existing: Long): HttpURLConnection {
        var current = url
        var hop = 0
        while (hop++ < MAX_REDIRECTS) {
            val conn = (URL(current).openConnection() as HttpURLConnection).apply {
                instanceFollowRedirects = false
                connectTimeout = 20_000
                readTimeout = 60_000
                setRequestProperty("User-Agent", "voicegw-android/1.0")
                if (existing > 0) setRequestProperty("Range", "bytes=$existing-")
            }
            val code = conn.responseCode
            if (code in 300..399) {
                val loc = conn.getHeaderField("Location")
                conn.disconnect()
                if (loc.isNullOrBlank()) throw IllegalStateException("$code 没有 Location")
                current = URL(URL(current), loc).toString()
                continue
            }
            return conn
        }
        throw IllegalStateException("跳转次数过多：$url")
    }
}
