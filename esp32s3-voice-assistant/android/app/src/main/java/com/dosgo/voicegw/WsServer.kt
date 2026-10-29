package com.dosgo.voicegw

import android.util.Log
import org.java_websocket.WebSocket
import org.java_websocket.handshake.ClientHandshake
import org.java_websocket.server.WebSocketServer
import java.io.ByteArrayOutputStream
import java.net.InetSocketAddress
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * WebSocket 服务端 —— ESP32 连进来，这里是整套语音链路的调度中心。
 *
 * 协议和 Python 版 gateway.py 一致：16kHz/16bit/单声道 PCM，32ms/1024 字节一帧。
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

    /**
     * 下行保活。设备在待机时每 8 秒发一次 hb，但它自己也有
     * 「30 秒收不到任何下行就判连接死」的保护，所以这里除了回 pong，
     * 还主动每 8 秒发一个 ping，双保险。
     */
    private val heartbeat = Executors.newSingleThreadScheduledExecutor { r ->
        Thread(r, "voice-heartbeat").apply { isDaemon = true }
    }

    private data class Utterance(val pcm: ByteArray, val speechMs: Int)

    private inner class Session {
        val buf = ByteArrayOutputStream()
        var speechMs = 0
        var silenceMs = 0

        /**
         * 连续超过能量门限的累计时长。用来区分「真的在说话」和「碰了一下桌子」：
         * 环境里的短促噪音每 1~2 秒冒一次，如果单帧就判 inSpeech，
         * 静音计时会被永远清零，网关永远等不到「说完」。
         */
        var hotMs = 0
        var inSpeech = false
        var awake = false
        var lastTalk = 0L

        /** 跨线程读写：WebSocket 回调线程 / worker / abort 都要看它 */
        @Volatile
        var busy = false

        /** 用户按 PWR 键打断：让正在跑的 LLM/TTS 尽快停 */
        @Volatile
        var abortRequested = false

        val history = mutableListOf<Pair<String, String>>()

        fun resetUtterance() {
            buf.reset()
            speechMs = 0
            silenceMs = 0
            hotMs = 0
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
        val s = sessions.remove(conn)
        if (s != null) s.abortRequested = true
        logger("设备断开 ${conn.remoteSocketAddress}")
    }

    override fun onError(conn: WebSocket?, ex: Exception) {
        Log.e(TAG, "ws error", ex)
        conn?.let { sessions[it]?.abortRequested = true }
        logger("连接异常：${ex.message}")
    }

    override fun onStart() {
        logger("监听 0.0.0.0:$port")
        heartbeat.scheduleAtFixedRate({
            for ((ws, _) in sessions) {
                if (!ws.isOpen) continue
                runCatching { ws.send(jsonMsg("ping")) }
            }
        }, 8, 8, TimeUnit.SECONDS)
    }

    // ---------------------------------------------------------------- 收消息

    override fun onMessage(conn: WebSocket, message: String) {
        val s = sessions[conn] ?: return
        val type = jsonStr(message, "type") ?: return
        when (type) {
            // 设备每 8 秒发一次 hb，等一个 pong 刷新它的 last_rx。
            // 不回的话设备 30 秒就主动断开重连 —— 表现是「放着不动自己断」。
            "hb", "ping" -> runCatching { conn.send(jsonMsg("pong")) }
            "hello" -> {
                val fmt = jsonStr(message, "format") ?: "?"
                val sr = jsonStr(message, "sample_rate") ?: "?"
                val frame = jsonStr(message, "frame_ms") ?: "?"
                val bits = jsonStr(message, "bits") ?: "?"
                logger("hello: $fmt ${sr}Hz/${bits}bit/${frame}ms 一帧")

                // 格式不对的话后面全是杂音/破音，不如在这里就说清楚
                if (fmt != "pcm" || sr != Protocol.SAMPLE_RATE.toString() ||
                    frame != Protocol.FRAME_MS.toString()
                ) {
                    logger(
                        "⚠️ 格式不匹配：固件发 ${fmt}/${sr}Hz/${frame}ms，" +
                            "本机要 pcm/${Protocol.SAMPLE_RATE}Hz/${Protocol.FRAME_MS}ms。" +
                            "改固件 src/config.rs 或 board.rs 的采样率/帧长"
                    )
                }
                runCatching {
                    conn.send(
                        jsonMsg(
                            "hello", "transport" to "websocket",
                            "format" to "pcm", "sample_rate" to Protocol.SAMPLE_RATE.toString(),
                            "frame_ms" to Protocol.FRAME_MS.toString(),
                        )
                    )
                }
            }
            "abort" -> {
                s.abortRequested = true
                synchronized(s) { s.resetUtterance() }
                logger("收到打断，停止当前应答")
            }
            "goodbye" -> conn.close()
        }
    }

    override fun onMessage(conn: WebSocket, message: ByteBuffer) {
        val s = sessions[conn] ?: return
        val data = ByteArray(message.remaining())
        message.get(data)
        if (data.isEmpty()) return

        var fire: Utterance? = null
        synchronized(s) {
            // 上一轮还在处理：这期间的音频一律不收，避免把一句话劈成两半
            if (s.busy) return

            s.buf.write(data)
            // 缓冲区上限：别让异常客户端把内存吃干净（30 秒音频封顶）
            if (s.buf.size() > Protocol.SAMPLE_RATE * 2 * 30) {
                s.resetUtterance()
                return
            }

            // 按实际字节数折时长，不能假设「一个 WS 消息 = 一帧」：
            // 设备正常是一帧 1024B，但 test_client / 旧固件可能合并多帧。
            val ms = ((data.size * 1000) / (Protocol.SAMPLE_RATE * 2)).coerceAtLeast(1)
            val e = AudioPcm.rms(data)

            if (e > Protocol.VAD_RMS_THRESHOLD) {
                s.hotMs += ms
                s.silenceMs = 0
                if (!s.inSpeech) {
                    // 连续 96ms 够响才算开口：碰桌子、敲键盘只有一两帧
                    if (s.hotMs >= Protocol.VAD_ONSET_MS) {
                        s.inSpeech = true
                        s.speechMs = s.hotMs
                    }
                } else {
                    s.speechMs += ms
                }
            } else {
                s.hotMs = 0
                if (s.inSpeech) {
                    s.silenceMs += ms
                } else {
                    // 还没开口：只留最近 400ms 当起音前回滚。
                    // 不裁的话，待机 10 秒后说第一句话，送给 ASR 的是
                    // 「10 秒静音 + 一句话」，又慢又容易识别错。
                    trimIdleBuffer(s)
                }
            }

            val done = (s.inSpeech && s.silenceMs >= Protocol.VAD_SILENCE_MS) ||
                s.speechMs >= Protocol.VAD_MAX_UTTERANCE_MS

            if (done) {
                fire = Utterance(s.buf.toByteArray(), s.speechMs)
                s.resetUtterance()
                // 在锁内占住 busy，避免同一连接的下一条消息在 submit 之前
                // 又凑出一段语音、提交两个 worker
                s.busy = true
            }
        }

        val utt = fire ?: return
        try {
            worker.submit {
                try {
                    processUtterance(conn, s, utt)
                } catch (t: Throwable) {
                    Log.e(TAG, "处理失败", t)
                } finally {
                    s.busy = false
                }
            }
        } catch (t: Throwable) {
            s.busy = false
            Log.e(TAG, "提交任务失败", t)
        }
    }

    // ---------------------------------------------------------------- 处理管线

    private fun processUtterance(conn: WebSocket, s: Session, utt: Utterance) {
        if (utt.speechMs < Protocol.VAD_MIN_SPEECH_MS) {
            logger("忽略碎片：有效语音 ${utt.speechMs}ms，不到 ${Protocol.VAD_MIN_SPEECH_MS}ms")
            return
        }

        logger(
            "一段语音：有效语音 ${"%.2f".format(utt.speechMs / 1000.0)}s，" +
                "缓冲 ${"%.2f".format(utt.pcm.size / 2.0 / Protocol.SAMPLE_RATE)}s"
        )

        val text = if (asr.isReady) asr.transcribe(utt.pcm) else ""
        if (text.isEmpty()) {
            logger("识别为空（模型没加载 / 没人说话 / 噪声）")
            // 不发送 stt：设备留在 Listening，10 秒静音后自己回 Idle。
            // 如果这里发了 stt，设备会切 Thinking，然后一直等不到 TTS。
            return
        }
        logger("识别：$text")

        val now = System.currentTimeMillis()
        val wake = Protocol.isWakeWord(text)

        if (!s.awake) {
            if (!wake) {
                // 【关键】不要在这里发 stt！固件收到 stt 会切 Thinking，
                // 没命中唤醒词就没有后续 TTS，设备会卡在 Thinking 直到 30 秒断线。
                logger("未命中唤醒词，忽略（不发送 stt）")
                return
            }
            enterConversation(conn, s, text, now)
            return
        }

        if (now - s.lastTalk > Protocol.AWAKE_TIMEOUT_S * 1000L) {
            s.awake = false
            if (!wake) {
                logger("超时回到待机，忽略这句")
                return
            }
            enterConversation(conn, s, text, now)
            return
        }

        s.lastTalk = now
        s.abortRequested = false
        runCatching { conn.send(jsonMsg("stt", "text" to text)) }
        respond(conn, s, text)
    }

    private fun enterConversation(conn: WebSocket, s: Session, text: String, now: Long) {
        s.awake = true
        s.lastTalk = now
        s.abortRequested = false
        logger("唤醒成功")
        // 先 stt（设备切 Thinking），再 ack（设备切 Listening），然后 TTS start 切 Speaking
        runCatching { conn.send(jsonMsg("stt", "text" to text)) }
        runCatching { conn.send(jsonMsg("ack")) }
        respond(conn, s, "（用户刚叫了你，用一句话简短回应，比如『我在』）")
    }

    private fun respond(conn: WebSocket, s: Session, userText: String) {
        s.history.add("user" to userText)
        while (s.history.size > 12) s.history.removeAt(0)

        val cfg = llmConfig()
        val sid = speakerIdProvider()

        // LLM 边出字、TTS 边合成、发送端边下发。三条流水线并行，
        // 这样第一句说完就能出声，不用等整段回答生成完。
        val textQueue = LinkedBlockingQueue<String>()
        val ttsFinished = CountDownLatch(1)
        val t0 = android.os.SystemClock.elapsedRealtime()
        var started = false
        var sentBytes = 0L
        var firstChunkMs = -1L
        var ttsFailed = false
        var engineLabel = "TTS"

        val ttsThread = Thread({
            var engine: Tts? = null
            try {
                while (true) {
                    val sentence = try {
                        textQueue.take()
                    } catch (_: InterruptedException) {
                        break
                    }
                    if (sentence == TTS_POISON) break
                    if (s.abortRequested) continue
                    try {
                        if (engine == null) engine = ttsProvider()
                        val t = engine
                        engineLabel = t.engineName()
                        if (!t.isReady) {
                            ttsFailed = true
                            logger("TTS 不可用，跳过这一句")
                            continue
                        }
                        val ok = t.synthStream(sentence, speakerId = sid) { pcm ->
                            if (s.abortRequested || !conn.isOpen) {
                                false
                            } else {
                                if (!started) {
                                    started = true
                                    firstChunkMs = android.os.SystemClock.elapsedRealtime() - t0
                                    sendJson(conn, jsonMsg("tts", "state" to "start"))
                                }
                                sentBytes += streamFrames(conn, s, pcm, sentBytes, t0)
                                !s.abortRequested && conn.isOpen
                            }
                        }
                        if (!ok) ttsFailed = true
                    } catch (t: Throwable) {
                        ttsFailed = true
                        logger("合成异常：${t.javaClass.simpleName} ${t.message}")
                    }
                }
            } finally {
                ttsFinished.countDown()
            }
        }, "voice-tts").apply {
            isDaemon = true
            start()
        }

        var pending = ""
        var reply = ""
        var anyQueued = false
        try {
            reply = Llm.chatStream(cfg, s.history) { delta ->
                pending += delta
                while (true) {
                    val (sentence, rest) = cutSentence(pending)
                    if (sentence.isEmpty()) break
                    textQueue.put(sentence)
                    anyQueued = true
                    pending = rest
                }
                !s.abortRequested
            }
        } catch (t: Throwable) {
            logger("LLM 流式异常：${t.javaClass.simpleName} ${t.message}")
        }

        if (s.abortRequested) {
            textQueue.put(TTS_POISON)
            runCatching { ttsFinished.await(10, TimeUnit.SECONDS) }
            logger("应答已被打断")
            sendJson(conn, jsonMsg("tts", "state" to "stop"))
            return
        }

        if (reply.isBlank()) reply = "抱歉，网络好像不太好。"
        val tail = pending.trim()
        if (tail.isNotEmpty()) {
            textQueue.put(tail)
            anyQueued = true
        }
        if (!anyQueued && reply.isNotBlank()) {
            // 极端情况：接口没按流式返回任何 delta，至少把整段话念出来
            textQueue.put(reply)
            anyQueued = true
        }
        textQueue.put(TTS_POISON)

        logger("回答：$reply")
        sendJson(conn, jsonMsg("llm", "text" to reply))
        s.history.add("assistant" to reply)

        runCatching { ttsFinished.await() }
        val totalSec = sentBytes / 2.0 / Protocol.SAMPLE_RATE
        if (started) {
            logger(
                "合成 ${"%.1f".format(totalSec)}s 音频（$engineLabel 音色 $sid，" +
                    "首块 ${firstChunkMs}ms${if (ttsFailed) "，部分失败" else ""}）"
            )
        } else {
            logger("没有合成出音频（音色 $sid，TTS 失败=$ttsFailed）")
        }
        // 无论成功、失败还是被打断，都发 stop：设备靠它把状态切回 Idle
        sendJson(conn, jsonMsg("tts", "state" to "stop"))
    }

    /**
     * 从流式文本里切出可以合成的一句。和中英文标点都要认。
     * 没标点时够长（24 字）就硬切，别让用户干等。
     */
    private fun cutSentence(text: String): Pair<String, String> {
        val hardCut = 24
        val commaMin = 6
        for (i in text.indices) {
            val c = text[i]
            if (c in "。！？!?…\n") {
                var j = i + 1
                while (j < text.length && text[j] in "”’』」）)】》") j++
                return text.substring(0, j).trim() to text.substring(j)
            }
            if (c in "，,；;、：:" && i + 1 >= commaMin) {
                return text.substring(0, i + 1).trim() to text.substring(i + 1)
            }
        }
        if (text.length >= hardCut) {
            return text.substring(0, hardCut).trim() to text.substring(hardCut)
        }
        return "" to text
    }

    /**
     * 把一块 PCM 按 32ms 帧发出去，并按「已发送音频时长」节流。
     *
     * 为什么要节流：合成速度可能快于实时（PC 上就很快），不限速会把设备端
     * 播放队列撑满丢帧；限太狠又会插静音。这里让发送进度最多领先播放
     * 进度 200ms，既不断流也不溢出。
     *
     * @return 实际发出的字节数
     */
    private fun streamFrames(
        conn: WebSocket,
        s: Session,
        pcm: ByteArray,
        sentBefore: Long,
        startMs: Long,
    ): Long {
        val frame = Protocol.FRAME_BYTES
        var off = 0
        var sent = 0L
        while (off < pcm.size) {
            if (s.abortRequested || !conn.isOpen) break
            val n = minOf(frame, pcm.size - off)
            val chunk = if (n == frame) {
                pcm.copyOfRange(off, off + n)
            } else {
                ByteArray(frame).also { System.arraycopy(pcm, off, it, 0, n) }
            }
            val sentOk = runCatching { conn.send(chunk) }.isSuccess
            if (!sentOk) break
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

    private fun trimIdleBuffer(s: Session) {
        val keep = Protocol.SAMPLE_RATE * 2 * Protocol.PRE_ROLL_MS / 1000
        if (keep <= 0 || s.buf.size() <= keep * 3) return
        val bytes = s.buf.toByteArray()
        s.buf.reset()
        s.buf.write(bytes, bytes.size - keep, keep)
    }

    private fun sendJson(conn: WebSocket, json: String) {
        runCatching { conn.send(json) }
    }

    fun shutdownAll() {
        heartbeat.shutdownNow()
        worker.shutdownNow()
        sessions.keys.forEach { runCatching { it.close() } }
        sessions.clear()
        runCatching { stop(1000) }
    }

    companion object {
        private const val TAG = "WsServer"
        /** 文本队列的结束标记，正常 TTS 文本不会包含 NUL */
        private const val TTS_POISON = "\u0000"
    }
}
