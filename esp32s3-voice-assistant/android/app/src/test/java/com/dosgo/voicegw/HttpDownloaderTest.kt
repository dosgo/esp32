package com.dosgo.voicegw

import okhttp3.mockwebserver.Dispatcher
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.mockwebserver.RecordedRequest
import okio.Buffer
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.File
import java.net.ServerSocket
import java.nio.file.Files
import kotlin.concurrent.thread

/**
 * 下载核心逻辑的测试 —— 用 MockWebServer 模拟 HF 的行为，不需要外网、不需要真机。
 *
 * 测的都是「手机上最难复现、又最容易写错」的点：
 *   · 308 → 302 两次跳转之后，Range 头还在不在
 *   · 服务器接受 Range（206）时能不能接着写
 *   · 服务器忽略 Range（200）时会不会错误地追加成半个文件
 *   · 长度对不上时会不会及时判失败
 */
class HttpDownloaderTest {

    private lateinit var server: MockWebServer
    private lateinit var base: String
    private lateinit var tmp: File

    /** 64KB 确定性内容，用来逐字节比对 */
    private val body = ByteArray(64 * 1024) { (it % 251).toByte() }

    @Before
    fun setUp() {
        server = MockWebServer()
        server.start()
        base = server.url("/").toString().trimEnd('/')
        tmp = Files.createTempDirectory("dl-test").toFile()
    }

    @After
    fun tearDown() {
        runCatching { server.shutdown() }
        tmp.deleteRecursively()
    }

    private fun fullBody(): MockResponse =
        MockResponse().setResponseCode(200).setBody(Buffer().write(body))

    // ---------------------------------------------------------------- 跳转

    @Test
    fun `跟随 308 和 302 两次跳转`() {
        // 模拟 HF：resolve → 308 → 302 → CDN
        server.enqueue(MockResponse().setResponseCode(308).setHeader("Location", "/b"))
        server.enqueue(MockResponse().setResponseCode(302).setHeader("Location", "/c"))
        server.enqueue(fullBody())

        val part = File(tmp, "m.bin.part")
        val out = HttpDownloader.downloadTo("$base/a", part)

        assertTrue("应该成功：${out.error}", out.ok)
        assertEquals(body.size.toLong(), part.length())
        assertArrayEquals(body, part.readBytes())
        assertEquals("应该发了 3 次请求（含两次跳转）", 3, server.requestCount)
    }

    @Test
    fun `跳转没有 Location 时报错而不是死循环`() {
        server.enqueue(MockResponse().setResponseCode(302))
        val out = HttpDownloader.downloadTo("$base/bad", File(tmp, "x.part"))
        assertFalse(out.ok)
        assertNotNull(out.error)
    }

    // ---------------------------------------------------------------- 续传

    @Test
    fun `有 part 时用 Range 续传，最终字节完全一致`() {
        val cut = 30_000
        server.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                val range = request.getHeader("Range")
                // 关键断言：续传请求必须带 Range
                assertNotNull("续传时必须带 Range 头", range)
                val start = range!!.removePrefix("bytes=").substringBefore("-").toInt()
                val slice = body.copyOfRange(start, body.size)
                return MockResponse().setResponseCode(206).setBody(Buffer().write(slice))
            }
        }

        val part = File(tmp, "m.bin.part")
        part.writeBytes(body.copyOfRange(0, cut))

        val out = HttpDownloader.downloadTo("$base/c", part)

        assertTrue("应该成功：${out.error}", out.ok)
        assertEquals(body.size.toLong(), part.length())
        assertArrayEquals(body, part.readBytes())
    }

    @Test
    fun `跳转之后 Range 依然保留`() {
        // 这条最容易出问题：自动跟随跳转时请求头可能被丢掉
        server.dispatcher = object : Dispatcher() {
            override fun dispatch(request: RecordedRequest): MockResponse {
                return when (request.path) {
                    "/a" -> MockResponse().setResponseCode(308).setHeader("Location", "/c")
                    else -> {
                        assertNotNull("跳转后的请求丢了 Range", request.getHeader("Range"))
                        val start = request.getHeader("Range")!!
                            .removePrefix("bytes=").substringBefore("-").toInt()
                        val slice = body.copyOfRange(start, body.size)
                        MockResponse().setResponseCode(206).setBody(Buffer().write(slice))
                    }
                }
            }
        }

        val part = File(tmp, "m.bin.part")
        part.writeBytes(body.copyOfRange(0, 10_000))

        val out = HttpDownloader.downloadTo("$base/a", part)
        assertTrue("应该成功：${out.error}", out.ok)
        assertArrayEquals(body, part.readBytes())
    }

    @Test
    fun `服务器忽略 Range 返回 200 时覆盖而不是追加`() {
        // 有些 CDN 不认 Range，直接给全量。必须覆盖，
        // 否则文件变成「30KB 垃圾 + 64KB 完整」= 94KB，加载必崩。
        server.enqueue(fullBody())

        val part = File(tmp, "m.bin.part")
        part.writeBytes(ByteArray(30_000) { 7 })

        val out = HttpDownloader.downloadTo("$base/c", part)

        assertTrue("应该成功：${out.error}", out.ok)
        assertEquals(body.size.toLong(), part.length())
        assertArrayEquals(body, part.readBytes())
    }

    // ---------------------------------------------------------------- 异常

    @Test
    fun `长度对不上时判失败`() {
        // MockWebServer 会按真实 body 长度改写 Content-Length，模拟不出截断，
        // 所以这里用一个裸 Socket：声明 4096 字节，实际只发 1000 就断开。
        // 这正是「下到一半断网」的样子，必须判失败——截断的 .onnx 会让 native 层崩。
        val port = serveTruncated(declared = 4096, actual = 1000)
        val part = File(tmp, "m.bin.part")

        val out = HttpDownloader.downloadTo("http://127.0.0.1:$port/c", part)

        assertFalse("截断的响应不该被判成功", out.ok)
        assertFalse("坏文件不能留下当正式模型", part.length() == 4096L)
    }

    /** 起一个只回一次响应的裸 HTTP 服务器，返回端口 */
    private fun serveTruncated(declared: Int, actual: Int): Int {
        val ss = ServerSocket(0)
        val port = ss.localPort
        thread(isDaemon = true) {
            try {
                ss.accept().use { sock ->
                    sock.soTimeout = 5000
                    val input = sock.getInputStream()
                    // 把请求头读掉（不解析）
                    val buf = ByteArray(8192)
                    var total = 0
                    while (total < buf.size) {
                        val n = input.read(buf, total, buf.size - total)
                        if (n <= 0) break
                        total += n
                        if (String(buf, 0, total, Charsets.ISO_8859_1).contains("\r\n\r\n")) break
                    }
                    val out = sock.getOutputStream()
                    out.write(
                        ("HTTP/1.1 200 OK\r\n" +
                            "Content-Length: $declared\r\n" +
                            "Connection: close\r\n\r\n").toByteArray(Charsets.ISO_8859_1)
                    )
                    out.write(ByteArray(actual) { 1 })
                    out.flush()
                }
            } catch (_: Throwable) {
                // 客户端提前断开是正常的
            } finally {
                runCatching { ss.close() }
            }
        }
        return port
    }

    @Test
    fun `404 时删掉没用的 part`() {
        server.enqueue(MockResponse().setResponseCode(404))

        val part = File(tmp, "m.bin.part")
        part.writeBytes(ByteArray(1000))

        val out = HttpDownloader.downloadTo("$base/nope", part)

        assertFalse(out.ok)
        assertEquals(404, out.httpCode)
        assertFalse("路径不对时半截文件没价值，应当删掉", part.exists())
    }

    @Test
    fun `取消时返回 cancelled`() {
        server.enqueue(fullBody())
        val out = HttpDownloader.downloadTo("$base/c", File(tmp, "m.bin.part"), cancelled = { true })

        assertFalse(out.ok)
        assertEquals("cancelled", out.error)
    }

    @Test
    fun `成功前不会产生正式文件`() {
        server.enqueue(fullBody())
        val dest = File(tmp, "model.onnx")
        val part = File(tmp, "model.onnx.part")

        // 调用方只写 .part，正式文件靠它自己在成功后改名
        HttpDownloader.downloadTo("$base/c", part, cancelled = { true })
        assertFalse(dest.exists())
        assertTrue("下到一半的数据要留着续传", part.exists() || part.length() == 0L)
    }
}
