package com.dosgo.voicegw

import android.util.Log
import org.java_websocket.WebSocket
import org.java_websocket.handshake.ClientHandshake
import org.java_websocket.server.WebSocketServer
import java.io.ByteArrayOutputStream
import java.net.InetSocketAddress
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors

/**
 * WebSocket 服务端 —— ESP32 连进来，这里是整套语音链路的调度中心。
 *
 * 协议和 Python 版 gateway.py 完全一致，所以固件一行都不用改。
 *
 * 线程模型：
 *   · WebSocket 回调线程只做「累积音频 + 能量 VAD」，绝不阻塞
 *   · 识别、调大模型、合成、下发全丢给单线程 executor 串行执行
 *     （ASR/TTS 模型各只有一份，并发会抢 native 资源）
 */
class VoiceWsServer(
    port: Int,
    private val asr: Asr,
    /** 按需取 TTS：第一次要合成时才加载，把 40MB 和 native 初始化从启动路径挪开 */
    private val ttsProvider: () -> Tts,
    /** 当前选中的音色编号，从设置里读；换了音色不用重启服务 */
    private val speakerIdProvider: () -> Int,
    private val llmConfig: () -> Llm.Config,
    private val logger: (String) -> Unit,
) : WebSocketServer(InetSocketAddress(port)) {

    private val worker = Executors.newSingleThreadExecutor { r ->
        Thread(r, "voice-worker").apply { isDaemon = true }
    }

    private inner class Session {
        val buf = ByteArrayOutputStream()
        var speechMs = 0
        var silenceMs = 0
        var inSpeech = false
        var awake = false
        var lastTalk = 0L
        var busy = false
        val history = mutableListOf<Pair<String, String>>()

        fun resetUtterance() {
            buf.reset()
            speechMs = 0
            silenceMs = 0
            inSpeech = false
        }
    }

    private val sessions = ConcurrentHashMap<WebSocket, Session>()

    // ---------------------------------------------------------------- 生命周期

    override fun onOpen(conn: WebSocket, handshake: ClientHandshake) {
        sessions[conn] = Session()
        logger("设备接入 ${conn.remoteSocketAddress}")
    }

    override fun onClose(conn: WebSocket, code: Int, reason: String?, remote: Boolean) {
        sessions.remove(conn)
        logger("设备断开 ${conn.remoteSocketAddress}")
    }

    override fun onError(conn: WebSocket?, ex: Exception) {
        Log.e(TAG, "ws error", ex)
        logger("连接异常：${ex.message}")
    }

    override fun onStart() {
        logger("监听 0.0.0.0:$port")
    }

    // ---------------------------------------------------------------- 收消息

    override fun onMessage(conn: WebSocket, message: String) {
        val s = sessions[conn] ?: return
        val type = jsonStr(message, "type") ?: return
        when (type) {
            "hello" -> {
                val fmt = jsonStr(message, "format") ?: "?"
                val sr = jsonStr(message, "sample_rate") ?: "?"
                val frame = jsonStr(message, "frame_ms") ?: "?"
                val bits = jsonStr(message, "bits") ?: "?"
                logger("hello: $fmt ${sr}Hz/${bits}bit/${frame}ms 一帧")

                // 格式不对的话后面全是杂音，不如在这里就说清楚
                if (fmt != "pcm" || sr != Protocol.SAMPLE_RATE.toString() ||
                    frame != Protocol.FRAME_MS.toString()
                ) {
                    logger(
                        "⚠️ 格式不匹配：固件发 ${fmt}/${sr}Hz/${frame}ms，" +
                            "本机要 pcm/${Protocol.SAMPLE_RATE}Hz/${Protocol.FRAME_MS}ms。" +
                            "改固件 src/config.rs 或 board.rs 的采样率/帧长"
                    )
                }
                conn.send(
                    jsonMsg(
                        "hello", "transport" to "websocket",
                        "format" to "pcm", "sample_rate" to Protocol.SAMPLE_RATE.toString(),
                        "frame_ms" to Protocol.FRAME_MS.toString(),
                    )
                )
            }
            "abort" -> synchronized(s) { s.resetUtterance() }
            "goodbye" -> conn.close()
        }
    }

    override fun onMessage(conn: WebSocket, message: ByteBuffer) {
        val s = sessions[conn] ?: return
        val data = ByteArray(message.remaining())
        message.get(data)
        if (data.isEmpty()) return

        var fire: ByteArray? = null
        synchronized(s) {
            s.buf.write(data)
            // 缓冲区上限：别让异常客户端把内存吃干净（30 秒音频封顶）
            if (s.buf.size() > Protocol.SAMPLE_RATE * 2 * 30) {
                s.resetUtterance()
                return
            }

            val e = AudioPcm.rms(data)
            if (e > Protocol.VAD_RMS_THRESHOLD) {
                s.speechMs += Protocol.FRAME_MS
                s.silenceMs = 0
                s.inSpeech = true
            } else if (s.inSpeech) {
                s.silenceMs += Protocol.FRAME_MS
            }

            val done = (s.inSpeech && s.silenceMs >= Protocol.VAD_SILENCE_MS) ||
                s.speechMs >= Protocol.VAD_MAX_UTTERANCE_MS

            if (done) {
                fire = s.buf.toByteArray()
                s.resetUtterance()
            }
        }

        val pcm = fire ?: return
        if (s.busy) return              // 上一轮还在处理，丢掉这段
        s.busy = true
        worker.submit {
            try {
                processUtterance(conn, s, pcm)
            } catch (t: Throwable) {
                Log.e(TAG, "处理失败", t)
            } finally {
                s.busy = false
            }
        }
    }

    // ---------------------------------------------------------------- 处理管线

    private fun processUtterance(conn: WebSocket, s: Session, pcm: ByteArray) {
        val minBytes = Protocol.SAMPLE_RATE * 2 * Protocol.VAD_MIN_SPEECH_MS / 1000
        if (pcm.size < minBytes) return

        logger("一段语音 ${"%.2f".format(pcm.size / 2.0 / Protocol.SAMPLE_RATE)}s，送去识别")

        val text = if (asr.isReady) asr.transcribe(pcm) else ""
        if (text.isEmpty()) {
            logger("识别为空（模型没加载 / 没人说话 / 噪声）")
            return
        }
        logger("识别：$text")
        conn.send(jsonMsg("stt", "text" to text))

        if (!s.awake) {
            if (Protocol.isWakeWord(text)) {
                s.awake = true
                s.lastTalk = System.currentTimeMillis()
                logger("唤醒成功")
                conn.send(jsonMsg("ack"))
                respond(conn, s, "（用户刚叫了你，用一句话简短回应，比如『我在』）")
            } else {
                logger("未命中唤醒词，忽略")
            }
            return
        }

        if (System.currentTimeMillis() - s.lastTalk > Protocol.AWAKE_TIMEOUT_S * 1000L) {
            s.awake = false
            logger("超时回到待机")
            return
        }

        s.lastTalk = System.currentTimeMillis()
        respond(conn, s, text)
    }

    private fun respond(conn: WebSocket, s: Session, userText: String) {
        s.history.add("user" to userText)
        while (s.history.size > 12) s.history.removeAt(0)

        val reply = Llm.chat(llmConfig(), s.history)
        logger("回答：$reply")
        conn.send(jsonMsg("llm", "text" to reply))
        s.history.add("assistant" to reply)

        val tts = ttsProvider()
        if (!tts.isReady) {
            conn.send(jsonMsg("tts", "state" to "stop"))
            return
        }
        val sid = speakerIdProvider()

        // 流式合成：边算边发。整句算完再发的话，手机上要干等 3~8 秒才出声。
        conn.send(jsonMsg("tts", "state" to "start"))
        val t0 = android.os.SystemClock.elapsedRealtime()
        var sentBytes = 0L
        var firstChunkMs = -1L

        val ok = tts.synthStream(reply, speakerId = sid) { pcm ->
            if (firstChunkMs < 0) firstChunkMs = android.os.SystemClock.elapsedRealtime() - t0
            sentBytes += streamFrames(conn, pcm, sentBytes, t0)
            conn.isOpen
        }

        val totalSec = sentBytes / 2.0 / Protocol.SAMPLE_RATE
        logger(
            "合成 ${"%.1f".format(totalSec)}s 音频（${tts.engineName()} 音色 $sid，" +
                "首块 ${firstChunkMs}ms${if (ok) "" else "，中断"}）"
        )
        conn.send(jsonMsg("tts", "state" to "stop"))
    }

    /**
     * 把一块 PCM 按 60ms 帧发出去，并按「已发送音频时长」节流。
     *
     * 为什么要节流：合成速度可能快于实时（PC 上就很快），不限速会把设备端
     * 8 帧的播放队列撑满丢帧；限太狠又会插静音。这里让发送进度最多领先播放
     * 进度 200ms，既不断流也不溢出。
     *
     * @return 实际发出的字节数
     */
    private fun streamFrames(
        conn: WebSocket,
        pcm: ByteArray,
        sentBefore: Long,
        startMs: Long,
    ): Long {
        val frame = Protocol.FRAME_BYTES
        var off = 0
        var sent = 0L
        while (off < pcm.size) {
            if (!conn.isOpen) break
            val n = minOf(frame, pcm.size - off)
            val chunk = if (n == frame) {
                pcm.copyOfRange(off, off + n)
            } else {
                ByteArray(frame).also { System.arraycopy(pcm, off, it, 0, n) }
            }
            conn.send(chunk)
            off += n
            sent += n

            val audioMs = (sentBefore + sent) * 1000 / (Protocol.SAMPLE_RATE * 2)
            val elapsedMs = android.os.SystemClock.elapsedRealtime() - startMs
            val ahead = audioMs - elapsedMs
            if (ahead > 200) {
                try {
                    Thread.sleep(ahead - 200)
                } catch (_: InterruptedException) {
                    break
                }
            }
        }
        return sent
    }

    // ---------------------------------------------------------------- 工具

    fun shutdownAll() {
        worker.shutdownNow()
        sessions.keys.forEach { runCatching { it.close() } }
        sessions.clear()
        runCatching { stop(1000) }
    }

    companion object {
        private const val TAG = "WsServer"
    }
}
