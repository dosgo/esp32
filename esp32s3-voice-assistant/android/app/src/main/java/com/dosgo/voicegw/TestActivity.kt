package com.dosgo.voicegw

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import java.io.ByteArrayOutputStream
import java.io.File
import java.util.concurrent.Executors

/**
 * 本地测试页 —— 不接 ESP32 也能验证整条链路。
 *
 * 为什么要这个页面：板子还没到、或者板子连不上时，一直没法确认
 * 「ASR 能不能识别中文」「TTS 出来是什么声音」「大模型通不通」。
 * 这里直接用手机麦克风和喇叭跑一遍，把问题分层定位：
 *
 *   识别不对 → ASR 模型 / 麦克风 / 采样率
 *   回答不对 → 大模型配置
 *   没声音   → TTS 模型 / 音频规则文件 / 手机媒体音量
 *
 * 模型和网关服务共用同一个实例（见 Models），不会重复占 229MB。
 */
class TestActivity : AppCompatActivity() {

    private lateinit var tvStatus: TextView
    private lateinit var tvLevel: TextView
    private lateinit var tvAsr: TextView
    private lateinit var tvTts: TextView
    private lateinit var tvLlm: TextView
    private lateinit var tvLog: TextView
    private lateinit var etSay: EditText
    private lateinit var etSpeaker: EditText
    private lateinit var tvVoiceInfo: TextView
    private lateinit var btnRecord: Button
    private lateinit var btnChain: Button
    private lateinit var tvDl: TextView
    private lateinit var etMirror: EditText
    private lateinit var btnDownload: Button
    private lateinit var btnDlCancel: Button

    /** 所有重活都在这个线程里跑，绝不阻塞 UI */
    private val worker = Executors.newSingleThreadExecutor { r ->
        Thread(r, "test-worker").apply { isDaemon = true }
    }

    private val logBuf = StringBuilder()

    @Volatile
    private var recording = false

    /** 本次录音结束后要不要接着跑大模型 + 合成 */
    @Volatile
    private var chainMode = false

    /** 下载中 / 用户点了取消 */
    @Volatile
    private var downloading = false

    @Volatile
    private var dlCancelled = false

    private var recThread: Thread? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_test)

        tvStatus = findViewById(R.id.tvStatus)
        tvLevel = findViewById(R.id.tvLevel)
        tvAsr = findViewById(R.id.tvAsr)
        tvTts = findViewById(R.id.tvTts)
        tvLlm = findViewById(R.id.tvLlm)
        tvLog = findViewById(R.id.tvLog)
        etSay = findViewById(R.id.etSay)
        etSpeaker = findViewById(R.id.etSpeaker)
        tvVoiceInfo = findViewById(R.id.tvVoiceInfo)
        etSpeaker.setText(currentSpeaker().toString())
        updateVoiceInfo()
        etSpeaker.setOnFocusChangeListener { _, hasFocus ->
            if (!hasFocus) {
                currentSpeaker()
                updateVoiceInfo()
            }
        }

        findViewById<Button>(R.id.btnPrevVoice).setOnClickListener { stepVoice(-1) }
        findViewById<Button>(R.id.btnNextVoice).setOnClickListener { stepVoice(1) }
        btnRecord = findViewById(R.id.btnRecord)
        btnChain = findViewById(R.id.btnChain)
        tvDl = findViewById(R.id.tvDl)
        etMirror = findViewById(R.id.etMirror)
        btnDownload = findViewById(R.id.btnDownload)
        btnDlCancel = findViewById(R.id.btnDlCancel)

        findViewById<Button>(R.id.btnCheck).setOnClickListener { checkModels() }
        findViewById<Button>(R.id.btnSay).setOnClickListener { synthAndPlay(etSay.text.toString()) }
        findViewById<Button>(R.id.btnLlm).setOnClickListener { testLlm() }

        btnRecord.setOnClickListener {
            if (recording) stopRecording() else startRecording(chain = false)
        }
        btnChain.setOnClickListener {
            if (recording) stopRecording() else startRecording(chain = true)
        }

        btnDownload.setOnClickListener { startDownload() }
        btnDlCancel.setOnClickListener {
            dlCancelled = true
            log("正在取消…（已下载的部分会保留，下次续传）")
        }

        log("打开测试页。模型目录：${Models.baseDir(this).absolutePath}")
        // 进页面就顺手把文件清单刷一遍，不用先点按钮
        tvStatus.text = Models.report(this)
        renderDownloadStatus()
    }

    override fun onDestroy() {
        recording = false
        runCatching { recThread?.join(1500) }
        worker.shutdownNow()
        super.onDestroy()
    }

    // ================================================================ 自检

    private fun checkModels() {
        tvStatus.text = Models.report(this)
        worker.execute {
            log("开始加载模型（首次 1-3 秒）…")
            val t0 = System.currentTimeMillis()
            val asr = Models.asr(this)
            val dtAsr = System.currentTimeMillis() - t0

            val t1 = System.currentTimeMillis()
            val tts = Models.tts(this)
            val dtTts = System.currentTimeMillis() - t1

            ui {
                tvStatus.text = Models.report(this)
                log("ASR 加载 ${if (asr.isReady) "成功" else "失败"}，${dtAsr}ms")
                log("TTS 加载 ${if (tts.isReady) "成功，音色 ${tts.numSpeakers()} 个" else "失败"}，${dtTts}ms")
                if (!asr.isReady) log("❌ ASR 不可用：看上面清单里 ❌ 的那两个文件")
                if (!tts.isReady) log("❌ TTS 不可用：看清单里 ← 必需 的那几个")
            }
        }
    }

    // ================================================================ 录音

    private fun startRecording(chain: Boolean) {
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            pendingChain = chain
            ActivityCompat.requestPermissions(
                this, arrayOf(Manifest.permission.RECORD_AUDIO), REQ_MIC,
            )
            return
        }

        chainMode = chain
        val buf = ByteArrayOutputStream()
        recording = true
        btnRecord.text = "停止"
        btnChain.text = "停止"
        tvLevel.text = "音量：准备中…"
        tvAsr.text = if (chain) "录音中…说完点「停止」" else "录音中…说完点「停止」"

        recThread = Thread { recordLoop(buf) }.apply { start() }
        log(if (chain) "开始录音（全链路模式）" else "开始录音（只识别）")
    }

    private var pendingChain = false

    private fun recordLoop(buf: ByteArrayOutputStream) {
        // 这里再查一次权限：startRecording 已经查过，但录音跑在独立线程里，
        // 静态检查看不出跨线程的保护，而且权限随时可能被用户在设置里收回。
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            recording = false
            ui {
                resetButtons()
                tvAsr.text = "❌ 没有麦克风权限（去系统设置里给「语音网关」开启）"
            }
            return
        }

        val sr = Protocol.SAMPLE_RATE
        val minBuf = AudioRecord.getMinBufferSize(
            sr, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
        )
        if (minBuf <= 0) {
            recording = false
            ui { resetButtons(); tvAsr.text = "❌ 这台手机不支持 16kHz 单声道录音" }
            return
        }

        val ar = try {
            AudioRecord(
                MediaRecorder.AudioSource.MIC,
                sr,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
                minBuf * 4,
            )
        } catch (t: Throwable) {
            recording = false
            ui { resetButtons(); tvAsr.text = "❌ 录音初始化失败：${t.message}" }
            log("AudioRecord 创建失败：${t.message}")
            return
        }

        if (ar.state != AudioRecord.STATE_INITIALIZED) {
            ar.release()
            recording = false
            ui { resetButtons(); tvAsr.text = "❌ 麦克风被占用（别的 App 在录音？）" }
            return
        }

        ar.startRecording()
        val tmp = ByteArray(3200)   // 100ms 一块，用来刷音量显示
        var lastLevel = 0L

        try {
            while (recording) {
                val n = ar.read(tmp, 0, tmp.size)
                if (n > 0) {
                    buf.write(tmp, 0, n)
                    val now = System.currentTimeMillis()
                    if (now - lastLevel > 120) {
                        lastLevel = now
                        val rms = AudioPcm.rms(tmp.copyOf(n))
                        val peak = if (rms > Protocol.VAD_RMS_THRESHOLD) "🔊" else "🔈"
                        ui { tvLevel.text = "音量：$peak $rms（门限 ${Protocol.VAD_RMS_THRESHOLD}，超过才会被当成说话）" }
                    }
                } else if (n < 0) {
                    log("录音读取出错 code=$n")
                    break
                }
            }
        } finally {
            runCatching { ar.stop() }
            runCatching { ar.release() }
        }

        val pcm = buf.toByteArray()
        ui { resetButtons() }
        val sec = pcm.size / 2.0 / Protocol.SAMPLE_RATE
        log("录音结束，共 ${"%.2f".format(sec)} 秒")
        if (pcm.size < Protocol.SAMPLE_RATE * 2 * 0.2) {
            ui { tvAsr.text = "录得太短了（${"%.2f".format(sec)} 秒），说完整一句再停" }
            return
        }

        if (chainMode) runChain(pcm) else runAsrOnly(pcm)
    }

    private fun stopRecording() {
        recording = false
        ui {
            btnRecord.text = "只识别"
            btnChain.text = "全链路（听到回答）"
            tvLevel.text = "音量：—（门限 ${Protocol.VAD_RMS_THRESHOLD}）"
        }
    }

    private fun resetButtons() {
        btnRecord.text = "只识别"
        btnChain.text = "全链路（听到回答）"
    }

    // ================================================================ 识别

    private fun runAsrOnly(pcm: ByteArray) {
        worker.execute {
            ui { tvAsr.text = "识别中…" }
            val asr = Models.asr(this)
            if (!asr.isReady) {
                ui { tvAsr.text = "❌ ASR 模型没加载，先点「① 检查模型文件」" }
                return@execute
            }
            val t0 = System.currentTimeMillis()
            val text = asr.transcribe(pcm)
            val dt = System.currentTimeMillis() - t0

            ui {
                if (text.isEmpty()) {
                    tvAsr.text = "识别结果为空（${dt}ms）\n可能：说话太小 / 麦克风权限 / 模型不对"
                } else {
                    val wake = if (Protocol.isWakeWord(text)) "✅ 命中唤醒词" else "（不是唤醒词）"
                    tvAsr.text = "$text\n\n耗时 ${dt}ms · ${wake}"
                }
            }
            log("ASR ${dt}ms → ${text.ifEmpty { "<空>" }}")
        }
    }

    // ================================================================ 全链路

    private fun runChain(pcm: ByteArray) {
        worker.execute {
            val asr = Models.asr(this)
            if (!asr.isReady) {
                ui { tvAsr.text = "❌ ASR 模型没加载，先点「① 检查模型文件」" }
                return@execute
            }

            // --- 1. 识别 ---
            ui { tvAsr.text = "① 识别中…" }
            val t0 = System.currentTimeMillis()
            val text = asr.transcribe(pcm)
            val dtAsr = System.currentTimeMillis() - t0
            if (text.isEmpty()) {
                ui { tvAsr.text = "① 识别为空（${dtAsr}ms），后面没法继续" }
                log("全链路中断：识别为空")
                return@execute
            }
            ui { tvAsr.text = text + "\n\n原始音频 ${"%.2f".format(pcm.size / 2.0 / Protocol.SAMPLE_RATE)}s · 识别 ${dtAsr}ms" }
            log("① 识别 ${dtAsr}ms → $text")

            // --- 2. 大模型 ---
            ui { tvLlm.text = "② 等大模型…" }
            val t1 = System.currentTimeMillis()
            val reply = Llm.chat(llmConfig(), listOf("user" to text))
            val dtLlm = System.currentTimeMillis() - t1
            ui {
                tvLlm.text = reply
                // 回填到合成框：觉得哪里念得不对，可以直接改字再点「合成并播放」
                etSay.setText(reply)
            }
            log("② 大模型 ${dtLlm}ms → $reply")

            // --- 3. 合成 + 播放 ---
            ui { tvTts.text = "③ 合成中…" }
            val tts = Models.tts(this)
            if (!tts.isReady) {
                ui { tvTts.text = "❌ TTS 模型没加载" }
                return@execute
            }
            val t2 = System.currentTimeMillis()
            val out = tts.synth(reply, speakerId = currentSpeaker())
            val dtTts = System.currentTimeMillis() - t2
            if (out.isEmpty()) {
                ui { tvTts.text = "❌ 合成返回空音频" }
                log("③ 合成返回空")
                return@execute
            }
            val audioSec = out.size / 2.0 / Protocol.SAMPLE_RATE
            ui {
                tvTts.text = "音频 ${"%.2f".format(audioSec)}s · 合成 ${dtTts}ms\n" +
                    "总耗时 ${dtAsr + dtLlm + dtTts}ms"
            }
            log("③ 合成 ${dtTts}ms，${"%.2f".format(audioSec)}s 音频")

            play(out)
        }
    }

    // ================================================================ 合成

    private fun synthAndPlay(text: String) {
        if (text.isBlank()) {
            Toast.makeText(this, "先输入要合成的文字", Toast.LENGTH_SHORT).show()
            return
        }
        worker.execute {
            ui { tvTts.text = "合成中…" }
            val tts = Models.tts(this)
            if (!tts.isReady) {
                ui { tvTts.text = "❌ TTS 模型没加载，先点「① 检查模型文件」" }
                return@execute
            }
            speak(tts, text, currentSpeaker())
        }
    }

    /**
     * 流式合成 + 播放。
     *
     * 手机上这模型约 1.0x 实时，整句算完再播要干等好几秒；
     * 流式的话第一块算完就出声，后面边算边续（AudioTrack.write 会阻塞，天然限速）。
     */
    private fun speak(tts: Tts, text: String, sid: Int) {
        val track = newTrack()
        if (track == null) {
            ui { tvTts.text = "❌ 创建播放器失败" }
            return
        }
        val t0 = android.os.SystemClock.elapsedRealtime()
        var firstMs = -1L
        var bytes = 0L
        var writeFail = false
        try {
            track.play()
            val ok = tts.synthStream(text, sid) { chunk ->
                if (firstMs < 0) {
                    firstMs = android.os.SystemClock.elapsedRealtime() - t0
                    ui { tvTts.text = "首块 ${firstMs}ms，边算边播中…" }
                }
                bytes += chunk.size
                val w = track.write(chunk, 0, chunk.size)
                if (w <= 0) writeFail = true
                w > 0
            }
            if (!ok) {
                ui { tvTts.text = "❌ 流式合成失败，看日志里 Tts 的报错" }
                return
            }
            // 把缓冲区里剩下的播完（约 100ms），否则尾巴被切
            Thread.sleep(250)
            val total = android.os.SystemClock.elapsedRealtime() - t0
            val sec = bytes / 2.0 / Protocol.SAMPLE_RATE
            ui {
                tvTts.text = "${text.length} 字 → 音频 ${"%.2f".format(sec)}s\n" +
                    "首块 ${firstMs}ms（等这么久就出声了），总耗时 ${total}ms，" +
                    "实时率 ${"%.2f".format(total / 1000.0 / sec)}x\n" +
                    "${tts.engineName()} 音色 $sid / ${tts.numSpeakers()}" +
                    if (writeFail) "（播放写入异常）" else ""
            }
            log("TTS ${tts.engineName()} sid=$sid 首块 ${firstMs}ms 总 ${total}ms，${"%.2f".format(sec)}s 音频")
        } catch (t: Throwable) {
            log("播放失败：${t.message}")
        } finally {
            runCatching { track.stop() }
            runCatching { track.release() }
        }
    }

    /** 16kHz / 单声道 / 16bit 的播放器，缓冲开大一点免得频繁欠载 */
    private fun newTrack(): AudioTrack? {
        val minBuf = AudioTrack.getMinBufferSize(
            Protocol.SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_MONO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        return try {
            AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                        .build()
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setSampleRate(Protocol.SAMPLE_RATE)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                        .build()
                )
                .setBufferSizeInBytes(maxOf(minBuf, 32768))
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build()
        } catch (t: Throwable) {
            log("AudioTrack 创建失败：${t.message}")
            null
        }
    }

    // ================================================================ 下载

    /** 列出每个文件的就位情况，以及有没有下到一半的 .part */
    private fun renderDownloadStatus() {
        val specs = ModelDownloader.specs(this)
        val sb = StringBuilder()
        var missingMb = 0.0
        var allOk = true

        for (sp in specs) {
            val ok = sp.dest.exists() && sp.dest.length() > 0
            sb.append(if (ok) "✅" else "⬜").append(' ')
                .append(sp.group).append('/').append(sp.local)
            if (ok) {
                sb.append("  ").append("%.1f".format(sp.dest.length() / 1024.0 / 1024.0)).append("MB")
            } else {
                allOk = false
                missingMb += sp.expectMb
                if (sp.expectMb > 0) sb.append("  缺，约 %.0fMB".format(sp.expectMb))
            }
            sb.append('\n')
        }

        val partials = specs.mapNotNull { sp ->
            val f = File(sp.dest.parentFile, sp.dest.name + ".part")
            if (f.exists() && f.length() > 0) f to sp.local else null
        }
        if (partials.isNotEmpty()) {
            sb.append("\n未完成（会自动续传）：\n")
            for ((f, name) in partials) {
                sb.append("  ").append(name).append("  ")
                    .append("%.0f".format(f.length() / 1024.0 / 1024.0)).append("MB\n")
            }
        }

        sb.append("\n").append(if (allOk) "全部就位" else "还差约 %.0fMB".format(missingMb))
        tvDl.text = sb.toString()
    }

    private fun startDownload() {
        if (downloading) return
        downloading = true
        dlCancelled = false
        btnDownload.isEnabled = false
        btnDlCancel.isEnabled = true
        tvDl.text = "准备下载…"

        val mirror = etMirror.text.toString()
        worker.execute {
            val ok = try {
                ModelDownloader.downloadAll(
                    this,
                    customMirror = mirror,
                    cancelled = { dlCancelled },
                    onProgress = { pr -> ui { tvDl.text = formatProgress(pr) } },
                    onLog = { line -> log(line) },
                )
            } catch (t: Throwable) {
                log("下载异常：${t.javaClass.simpleName} ${t.message}")
                false
            }

            downloading = false
            ui {
                btnDownload.isEnabled = true
                btnDlCancel.isEnabled = false
                renderDownloadStatus()
                tvStatus.text = Models.report(this)
            }

            if (ok) {
                Models.invalidate()
                log("模型已就位，开始加载验证")
                checkModels()
            }
        }
    }

    private fun formatProgress(p: ModelDownloader.Progress): String {
        val sb = StringBuilder()
        sb.append("【").append(p.index).append('/').append(p.count).append("】")
            .append(p.label).append('\n')

        if (p.totalBytes > 0) {
            sb.append("%.1f / %.1f MB".format(p.doneBytes / 1048576.0, p.totalBytes / 1048576.0))
                .append("   ").append(p.percent).append("%\n")
            val speed = p.speedBps / 1048576.0
            sb.append("速度 %.2f MB/s".format(speed))
            if (p.speedBps > 0) {
                val left = (p.totalBytes - p.doneBytes) / p.speedBps
                sb.append("   剩余 ").append(left / 60).append(':')
                    .append("%02d".format(left % 60))
            }
            sb.append('\n')
            val bars = (p.percent / 4).coerceIn(0, 25)
            repeat(bars) { sb.append('█') }
            repeat(25 - bars) { sb.append('░') }
        } else {
            sb.append("%.1f MB（服务器没给总长）".format(p.doneBytes / 1048576.0))
        }
        return sb.toString()
    }

    // ================================================================ 播放

    private fun play(pcm: ByteArray) {
        val minBuf = AudioTrack.getMinBufferSize(
            Protocol.SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_MONO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        val track = try {
            AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                        .build()
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setSampleRate(Protocol.SAMPLE_RATE)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                        .build()
                )
                .setBufferSizeInBytes(maxOf(minBuf, 16384))
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build()
        } catch (t: Throwable) {
            log("AudioTrack 创建失败：${t.message}")
            return
        }

        try {
            track.play()
            var off = 0
            while (off < pcm.size) {
                val n = track.write(pcm, off, pcm.size - off)
                if (n <= 0) break
                off += n
            }
            // 等缓冲里的数据播完，否则 release 会把尾巴切掉
            val ms = (pcm.size / 2.0 / Protocol.SAMPLE_RATE * 1000).toLong()
            Thread.sleep(ms + 150)
        } catch (t: Throwable) {
            log("播放失败：${t.message}")
        } finally {
            runCatching { track.stop() }
            runCatching { track.release() }
        }
    }

    // ================================================================ 大模型

    private fun testLlm() {
        worker.execute {
            val cfg = llmConfig()
            ui {
                tvLlm.text = "请求中…（${cfg.baseUrl} / ${cfg.model}）"
            }
            if (cfg.apiKey.isBlank()) {
                ui { tvLlm.text = "❌ 还没填 API Key，回上一页填好保存" }
                return@execute
            }
            val t0 = System.currentTimeMillis()
            val reply = Llm.chat(cfg, listOf("user" to "你好，用一句话回答"))
            val dt = System.currentTimeMillis() - t0
            ui { tvLlm.text = "$reply\n\n耗时 ${dt}ms" }
            log("大模型 ${dt}ms → $reply")
        }
    }

    /** 在 151 个女声里前后翻，翻到就顺手合成一遍直接听 */
    private fun stepVoice(delta: Int) {
        val list = VoiceCatalog.females(this)
        if (list.isEmpty()) {
            Toast.makeText(this, "没有女声列表资源", Toast.LENGTH_SHORT).show()
            return
        }
        val cur = etSpeaker.text.toString().toIntOrNull() ?: 0
        val next = VoiceCatalog.step(list, cur, delta) ?: return
        etSpeaker.setText(next.sid.toString())
        updateVoiceInfo()
        synthAndPlay(etSay.text.toString())
    }

    private fun updateVoiceInfo() {
        val list = VoiceCatalog.females(this)
        if (list.isEmpty()) {
            tvVoiceInfo.text = "音色信息：没有女声列表（首次需要联网/内置资源）"
            return
        }
        val sid = etSpeaker.text.toString().toIntOrNull() ?: 0
        val idx = VoiceCatalog.indexOf(list, sid)
        tvVoiceInfo.text = if (idx >= 0) {
            "女声 ${idx + 1}/${list.size} · sid=$sid · 基频 ${"%.0f".format(list[idx].f0)}Hz"
        } else {
            "sid=$sid 不在女声列表里（共 ${list.size} 个女声可翻）"
        }
    }

    /** 界面上的音色编号，同时写进设置（网关那边也读这个值） */
    private fun currentSpeaker(): Int {
        val v = etSpeaker.text.toString().toIntOrNull() ?: 0
        val sid = v.coerceAtLeast(0)
        getSharedPreferences(GatewayService.PREFS, Context.MODE_PRIVATE)
            .edit().putInt("ttsSpeaker", sid).apply()
        return sid
    }

    private fun llmConfig(): Llm.Config {
        val p = getSharedPreferences(GatewayService.PREFS, Context.MODE_PRIVATE)
        return Llm.Config(
            apiKey = p.getString("apiKey", "") ?: "",
            baseUrl = p.getString("baseUrl", "https://api.siliconflow.cn/v1")
                ?: "https://api.siliconflow.cn/v1",
            model = p.getString("model", "Qwen/Qwen2.5-7B-Instruct")
                ?: "Qwen/Qwen2.5-7B-Instruct",
        )
    }

    // ================================================================ 权限 / 工具

    override fun onRequestPermissionsResult(
        requestCode: Int, permissions: Array<out String>, grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == REQ_MIC) {
            if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
                startRecording(pendingChain)
            } else {
                Toast.makeText(this, "没有麦克风权限没法录音", Toast.LENGTH_LONG).show()
                log("麦克风权限被拒绝")
            }
        }
    }

    private fun log(line: String) {
        runOnUiThread {
            logBuf.append(line).append('\n')
            val lines = logBuf.lines()
            if (lines.size > 200) {
                logBuf.setLength(0)
                logBuf.append(lines.takeLast(200).joinToString("\n"))
            }
            tvLog.text = logBuf.toString()
        }
    }

    /** 后台线程里改 UI 的统一入口 */
    private fun ui(block: () -> Unit) = runOnUiThread(block)

    companion object {
        private const val REQ_MIC = 200
    }
}
