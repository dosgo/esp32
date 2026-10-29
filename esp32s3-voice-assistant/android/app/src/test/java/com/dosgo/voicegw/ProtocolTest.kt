package com.dosgo.voicegw

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 纯 JVM 单元测试 —— 不需要真机，`gradlew testDebugUnitTest` 就能跑。
 *
 * 只测不依赖 Android API 的部分：协议常量、唤醒词、JSON、PCM 转换。
 * 模型推理（ASR/TTS）必须有真机 + arm64 的 so，测不了。
 */
class ProtocolTest {

    // ---------------------------------------------------------------- 帧常量

    @Test
    fun `一帧正好 32ms 1024 字节`() {
        assertEquals(32, Protocol.FRAME_MS)
        assertEquals(1024, Protocol.FRAME_BYTES)
        // 和固件 board.rs 的 FRAME_SAMPLES 对齐：512 采样 × 2 字节
        assertEquals(Protocol.FRAME_BYTES, Protocol.SAMPLE_RATE * 2 * Protocol.FRAME_MS / 1000)
    }

    // ---------------------------------------------------------------- 唤醒词

    @Test
    fun `精确唤醒词能命中`() {
        assertTrue(Protocol.isWakeWord("你好小智"))
        assertTrue(Protocol.isWakeWord("小智小智"))
        assertTrue(Protocol.isWakeWord("小智"))
        // 句子里带唤醒词也应命中（ASR 常带标点）
        assertTrue(Protocol.isWakeWord("你好，小智。"))
        // 设备端 WakeNet 的唤醒词也要认
        assertTrue(Protocol.isWakeWord("小特小特"))
        assertTrue(Protocol.isWakeWord("小特"))
    }

    @Test
    fun `同音字能命中`() {
        // 实测本地 SenseVoice 会把「小智」识别成「小芝」
        assertTrue(Protocol.isWakeWord("你好，小芝。"))
        assertTrue(Protocol.isWakeWord("小志小志"))
        assertTrue(Protocol.isWakeWord("晓智你在吗"))
    }

    @Test
    fun `普通句子不误唤醒`() {
        assertFalse(Protocol.isWakeWord(""))
        assertFalse(Protocol.isWakeWord("北京今天天气怎么样"))
        assertFalse(Protocol.isWakeWord("现在几点了"))
        assertFalse(Protocol.isWakeWord("播放一首周杰伦的歌"))
    }

    // ---------------------------------------------------------------- JSON

    @Test
    fun `json 生成与解析往返`() {
        val msg = jsonMsg("stt", "text" to "北京今天天气怎么样？")
        assertEquals("stt", jsonStr(msg, "type"))
        assertEquals("北京今天天气怎么样？", jsonStr(msg, "text"))
    }

    @Test
    fun `json 里带引号和反斜杠不破坏结构`() {
        val msg = jsonMsg("llm", "text" to """他说"你好"\路径""")
        assertEquals("llm", jsonStr(msg, "type"))
        assertEquals("""他说"你好"\路径""", jsonStr(msg, "text"))
    }

    @Test
    fun `json 缺字段返回 null`() {
        val msg = jsonMsg("hello")
        assertEquals("hello", jsonStr(msg, "type"))
        assertEquals(null, jsonStr(msg, "format"))
    }

    @Test
    fun `能解析固件发来的 hello`() {
        // 固件 main.rs 里实际发的这一行，注意数字字段不带引号
        val hello = """{"type":"hello","version":2,"format":"pcm","sample_rate":16000,"channels":1,"bits":16,"frame_ms":32}"""
        assertEquals("hello", jsonStr(hello, "type"))
        assertEquals("2", jsonStr(hello, "version"))
        assertEquals("pcm", jsonStr(hello, "format"))
        assertEquals("16000", jsonStr(hello, "sample_rate"))
        assertEquals("16", jsonStr(hello, "bits"))
        assertEquals("32", jsonStr(hello, "frame_ms"))
        // 末尾的数字字段（后面直接跟 }）也要能读出来
        assertEquals("32", jsonStr(hello, "frame_ms"))
    }

    @Test
    fun `数字字段不会被相邻字段带偏`() {
        val json = """{"a":1,"b":22,"c":"33"}"""
        assertEquals("1", jsonStr(json, "a"))
        assertEquals("22", jsonStr(json, "b"))
        assertEquals("33", jsonStr(json, "c"))
    }

    // ---------------------------------------------------------------- PCM

    @Test
    fun `同采样率直接转换不改变波形`() {
        val src = floatArrayOf(0f, 0.5f, -0.5f, 1f, -1f)
        val pcm = AudioPcm.to16kPcm(src, 16000)
        assertEquals(src.size * 2, pcm.size)

        val back = AudioPcm.toFloatSamples(pcm)
        // 量化误差允许 1/32768
        for (i in src.indices) {
            assertEquals(src[i].toDouble(), back[i].toDouble(), 1.0 / 32767)
        }
    }

    @Test
    fun `22050 降采样到 16000 长度正确`() {
        val src = FloatArray(22050) { 0.1f }
        val pcm = AudioPcm.to16kPcm(src, 22050)
        assertEquals(16000, pcm.size / 2)
    }

    @Test
    fun `超范围 float 不会溢出成爆音`() {
        // 不做 clamp 的话，1.5 * 32767 = 49150 → toByte 回绕成负数，听起来是爆音
        val pcm = AudioPcm.to16kPcm(floatArrayOf(1.5f, -1.5f, 3f), 16000)
        val back = AudioPcm.toFloatSamples(pcm)
        assertEquals(1.0, back[0].toDouble(), 1.0 / 32767)
        assertEquals(-1.0, back[1].toDouble(), 1.0 / 32767)
        assertEquals(1.0, back[2].toDouble(), 1.0 / 32767)
    }

    @Test
    fun `空输入返回空`() {
        assertArrayEquals(ByteArray(0), AudioPcm.to16kPcm(FloatArray(0), 16000))
        assertArrayEquals(ByteArray(0), AudioPcm.to16kPcm(FloatArray(10) { 0.1f }, 0))
    }

    // ---------------------------------------------------------------- VAD

    @Test
    fun `静音帧 RMS 为 0`() {
        assertEquals(0, AudioPcm.rms(ByteArray(1024)))
    }

    @Test
    fun `满幅方波 RMS 接近 32767`() {
        val n = 512
        val pcm = ByteArray(n * 2)
        for (i in 0 until n) {
            val v = if (i % 2 == 0) 32767 else -32768
            pcm[i * 2] = (v and 0xFF).toByte()
            pcm[i * 2 + 1] = ((v shr 8) and 0xFF).toByte()
        }
        val r = AudioPcm.rms(pcm)
        assertTrue("RMS=$r 应该接近 32767", r > 32000)
    }

    @Test
    fun `小音量应该低于 VAD 门限`() {
        // 振幅 80 的正弦，RMS ≈ 57，低于门限 90 → 判定为没人说话
        val n = 512
        val pcm = ByteArray(n * 2)
        for (i in 0 until n) {
            val v = (80 * Math.sin(2 * Math.PI * 440 * i / 16000.0)).toInt()
            pcm[i * 2] = (v and 0xFF).toByte()
            pcm[i * 2 + 1] = ((v shr 8) and 0xFF).toByte()
        }
        assertTrue(AudioPcm.rms(pcm) < Protocol.VAD_RMS_THRESHOLD)
    }

    @Test
    fun `正常说话音量应该高于 VAD 门限`() {
        val n = 512
        val pcm = ByteArray(n * 2)
        for (i in 0 until n) {
            val v = (6000 * Math.sin(2 * Math.PI * 300 * i / 16000.0)).toInt()
            pcm[i * 2] = (v and 0xFF).toByte()
            pcm[i * 2 + 1] = ((v shr 8) and 0xFF).toByte()
        }
        assertTrue(AudioPcm.rms(pcm) > Protocol.VAD_RMS_THRESHOLD)
    }
}
