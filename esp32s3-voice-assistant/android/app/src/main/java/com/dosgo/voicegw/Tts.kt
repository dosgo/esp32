package com.dosgo.voicegw

import android.util.Log
import com.k2fsa.sherpa.onnx.OfflineTts
import com.k2fsa.sherpa.onnx.OfflineTtsConfig
import com.k2fsa.sherpa.onnx.OfflineTtsKokoroModelConfig
import com.k2fsa.sherpa.onnx.OfflineTtsModelConfig
import com.k2fsa.sherpa.onnx.OfflineTtsVitsModelConfig
import java.io.File

/**
 * 中文语音合成 —— 支持两套模型，按目录自动识别。
 *
 * | 引擎 | 目录 | 采样率 | 体积 | 音色 | 实测速度（PC 4 核） |
 * |---|---|---|---|---|---|
 * | **Kokoro** | `tts/kokoro/` | **24kHz** | 340MB | 54 个 | 0.26x 实时 |
 * | VITS | `tts/` 或 `tts/vits/` | 8kHz | 41MB | 174 个 | 0.16x 实时 |
 *
 * 为什么换 Kokoro：原来那套 VITS 是 **8kHz** 训练的（电话音质），
 * 采样率训练时就定死了，参数怎么调都上不去。Kokoro 是 24kHz，听感差别很大，
 * 代价是模型 340MB、手机上合成略慢。
 *
 * 选哪个由 `tts/engine.txt` 决定（内容写 `kokoro` 或 `vits`）：
 *   · 不写 / 写 vits —— **默认**，用 16kHz 的 vits-zh-ll，速度快，手机扛得住
 *   · 写 kokoro     —— 音质最好，但实测这台 Redmi 上 1.71x 实时（算得比播得慢），
 *                      只适合短句或高性能机型
 * 实测数据（Xiaomi 2602BRT18C，8 核）：
 *   Kokoro 24kHz  19 字 → 4.56s 音频，合成 7785ms（1.71x，不能用）
 *   vits-zh-ll    19 字 → 3.31s 音频，合成 ~1.1s（0.33x，可用）
 *
 * 【务必注意】模型在 SD 卡上（绝对路径）时，**不能**给 OfflineTts 传 AssetManager，
 * 否则 native 层会去 asset 里找这个绝对路径、失败后直接 abort 整个进程
 * （logcat 只有一行 "Load ... failed"，没有任何 Java 栈）。所以这里用 `config = config`。
 */
class Tts(private val modelDir: File) {

    enum class Engine { KOKORO, VITS, NONE }

    private var tts: OfflineTts? = null
    private var engine: Engine = Engine.NONE
    private var label = "无"

    /** Kokoro 单独一个子目录，避免和 VITS 的 model.onnx / tokens.txt 撞名 */
    private val kokoroDir = File(modelDir, "kokoro")

    /** VITS 目录：优先 tts/vits/，再兼容早期直接放 tts/ 根下的布局 */
    private val vitsDir = if (File(modelDir, "vits/model.onnx").exists()) {
        File(modelDir, "vits")
    } else {
        modelDir
    }

    val isReady: Boolean
        get() = tts != null

    fun numSpeakers(): Int = tts?.numSpeakers() ?: 0

    /** 当前用的引擎名（带真实采样率），给界面/日志显示 */
    fun engineName(): String = label

    fun sampleRate(): Int = tts?.sampleRate() ?: 0

    private val hasKokoro: Boolean
        get() = File(kokoroDir, "model.onnx").exists() && File(kokoroDir, "voices.bin").exists()

    private val hasVits: Boolean
        get() = File(vitsDir, "model.onnx").exists()

    /** engine.txt 里写 kokoro 才用 Kokoro，否则优先快的 VITS */
    private fun preferKokoro(): Boolean =
        runCatching { File(modelDir, "engine.txt").readText().trim() }.getOrNull()
            ?.equals("kokoro", ignoreCase = true) == true

    fun detectEngine(): Engine = when {
        preferKokoro() && hasKokoro -> Engine.KOKORO
        hasVits -> Engine.VITS
        hasKokoro -> Engine.KOKORO   // 只有 Kokoro 也能用
        else -> Engine.NONE
    }

    fun modelExists(): Boolean = detectEngine() != Engine.NONE

    fun load(): Boolean {
        if (tts != null) return true
        val e = detectEngine()
        if (e == Engine.NONE) {
            Log.e(TAG, "没有可用的 TTS 模型：$modelDir")
            return false
        }
        return try {
            val config = when (e) {
                Engine.KOKORO -> kokoroConfig()
                else -> vitsConfig()
            }
            tts = OfflineTts(config = config)
            engine = e
            val sr = tts?.sampleRate() ?: 0
            label = when (e) {
                Engine.KOKORO -> "kokoro ${sr / 1000}kHz ${numSpeakers()}音色"
                Engine.VITS -> "vits ${sr / 1000}kHz ${numSpeakers()}音色"
                Engine.NONE -> "无"
            }
            Log.i(TAG, "TTS 加载完成：$label")
            true
        } catch (t: Throwable) {
            Log.e(TAG, "加载失败", t)
            tts = null
            engine = Engine.NONE
            false
        }
    }

    private fun kokoroConfig(): OfflineTtsConfig {
        val rules = listOf("phone-zh.fst", "date-zh.fst", "number-zh.fst")
            .map { File(kokoroDir, it) }
            .filter { it.exists() }
            .joinToString(",") { it.absolutePath }

        return OfflineTtsConfig(
            model = OfflineTtsModelConfig(
                kokoro = OfflineTtsKokoroModelConfig(
                    model = File(kokoroDir, "model.onnx").absolutePath,
                    voices = File(kokoroDir, "voices.bin").absolutePath,
                    tokens = File(kokoroDir, "tokens.txt").absolutePath,
                    lexicon = File(kokoroDir, "lexicon-zh.txt").absolutePath,
                ),
                numThreads = 4,
                debug = false,
                provider = "cpu",
            ),
            ruleFsts = rules,
            ruleFars = "",
            maxNumSentences = 1,
        )
    }

    private fun vitsConfig(): OfflineTtsConfig {
        // 中文 VITS 必须挂这几个 fst，否则「7点」「13800138000」会读错甚至整段跳过
        val rules = listOf("phone.fst", "date.fst", "number.fst", "new_heteronym.fst")
            .map { File(vitsDir, it) }
            .filter { it.exists() }
            .joinToString(",") { it.absolutePath }

        return OfflineTtsConfig(
            model = OfflineTtsModelConfig(
                vits = OfflineTtsVitsModelConfig(
                    model = File(vitsDir, "model.onnx").absolutePath,
                    lexicon = File(vitsDir, "lexicon.txt").absolutePath,
                    tokens = File(vitsDir, "tokens.txt").absolutePath,
                ),
                numThreads = 4,
                debug = false,
                provider = "cpu",
            ),
            ruleFsts = rules,
            ruleFars = "",   // 不引 173MB 的 rule.far
            maxNumSentences = 1,
        )
    }

    /**
     * 合成语音
     * @return 16kHz / 16bit / 单声道 小端 PCM；失败返回空数组
     */
    fun synth(text: String, speakerId: Int = 0, speed: Float = 1.0f): ByteArray {
        val t = tts ?: return ByteArray(0)
        if (text.isBlank()) return ByteArray(0)
        return try {
            // 模型自带采样率（Kokoro 24k / VITS 8k），统一重采样到设备要的 16k。
            // sid 必须夹进合法范围，越界时 sherpa 会打警告并回落到 0，但夹一下更干净。
            val sid = speakerId.coerceIn(0, (t.numSpeakers() - 1).coerceAtLeast(0))
            val audio = t.generate(text, sid, speed)
            val samples = audio.samples
            if (samples.isEmpty()) return ByteArray(0)
            AudioPcm.to16kPcm(samples, audio.sampleRate)
        } catch (e: Throwable) {
            Log.e(TAG, "合成失败", e)
            ByteArray(0)
        }
    }

    /**
     * 流式合成：每生成一块就回调一次，用于「边算边播」。
     *
     * 为什么需要：这台 Redmi 上 16kHz 模型约 1.0x 实时——整句算完再播，用户要干等 3 秒；
     * 改成流式后，第一块（约 0.3~0.6 秒音频）算完就能出声，后面的块边算边续。
     *
     * @param onChunk 拿到一块 16k/16bit/单声道 PCM，返回 false 可中止
     * @return 是否成功（false 表示模型没加载或合成异常）
     */
    fun synthStream(
        text: String,
        speakerId: Int = 0,
        speed: Float = 1.0f,
        onChunk: (ByteArray) -> Boolean,
    ): Boolean {
        val t = tts ?: return false
        if (text.isBlank()) return false
        return try {
            val sid = speakerId.coerceIn(0, (t.numSpeakers() - 1).coerceAtLeast(0))
            val sr = t.sampleRate()

            // 回调必须是 Java 类（见 TtsChunkCallback 注释）：
            // native 按 "invoke([F)Ljava/lang/Integer;" 这个签名找方法，
            // Kotlin lambda 编译成 invoke(Object)，签名对不上会直接 SIGABRT。
            val cb = TtsChunkCallback { samples ->
                if (samples.isEmpty()) {
                    true
                } else {
                    val pcm = AudioPcm.to16kPcm(samples, sr)
                    pcm.isEmpty() || onChunk(pcm)
                }
            }
            t.generateWithCallback(text, sid, speed, cb)
            true
        } catch (e: Throwable) {
            Log.e(TAG, "流式合成失败", e)
            false
        }
    }

    fun release() {
        tts?.free()
        tts = null
        engine = Engine.NONE
    }

    companion object {
        private const val TAG = "Tts"
    }
}
