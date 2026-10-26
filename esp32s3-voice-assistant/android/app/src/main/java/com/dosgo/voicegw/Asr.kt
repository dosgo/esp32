package com.dosgo.voicegw

import android.util.Log
import com.k2fsa.sherpa.onnx.OfflineModelConfig
import com.k2fsa.sherpa.onnx.OfflineRecognizer
import com.k2fsa.sherpa.onnx.OfflineRecognizerConfig
import com.k2fsa.sherpa.onnx.OfflineSenseVoiceModelConfig
import java.io.File

/**
 * SenseVoice 语音识别（sherpa-onnx 官方 AAR）
 *
 * 模型目录里需要两个文件：
 *   model.int8.onnx   （229MB，和电脑上跑的是同一份）
 *   tokens.txt
 *
 * 用法：一次性 load，之后反复 transcribe。每次 transcribe 会新建一个 stream，
 * 用完立刻 release，否则 native 内存会一直涨。
 */
class Asr(private val modelDir: File) {

    private var rec: OfflineRecognizer? = null

    val isReady: Boolean
        get() = rec != null

    fun modelExists(): Boolean =
        File(modelDir, "model.int8.onnx").exists() && File(modelDir, "tokens.txt").exists()

    /** 加载模型。耗时 1-3 秒，必须在后台线程调用。 */
    fun load(): Boolean {
        if (rec != null) return true
        if (!modelExists()) {
            Log.e(TAG, "模型缺失：$modelDir")
            return false
        }
        return try {
            val config = OfflineRecognizerConfig(
                modelConfig = OfflineModelConfig(
                    senseVoice = OfflineSenseVoiceModelConfig(
                        model = File(modelDir, "model.int8.onnx").absolutePath,
                        language = "zh",
                        useInverseTextNormalization = true,
                    ),
                    tokens = File(modelDir, "tokens.txt").absolutePath,
                    numThreads = 4,
                    debug = false,
                    provider = "cpu",
                ),
            )
            // 【关键】不要传 AssetManager！
            // 模型在 SD 卡上、用的是绝对路径，一旦 assetManager 非空，
            // native 层会去 asset 里找这个绝对路径，找不到直接 abort 掉整个进程
            // （sherpa-onnx: "Load '...' failed" → SIGABRT，logcat 里没有任何 Java 栈）。
            rec = OfflineRecognizer(config = config)
            Log.i(TAG, "SenseVoice 加载完成")
            true
        } catch (e: Throwable) {
            Log.e(TAG, "加载失败", e)
            rec = null
            false
        }
    }

    /**
     * @param pcm 16kHz / 16bit / 单声道，小端字节序
     * @return 识别出的文字，失败返回空串
     */
    fun transcribe(pcm: ByteArray): String {
        val r = rec ?: return ""
        if (pcm.size < 2) return ""

        val samples = AudioPcm.toFloatSamples(pcm)

        var stream: com.k2fsa.sherpa.onnx.OfflineStream? = null
        return try {
            stream = r.createStream()
            stream.acceptWaveform(samples, Protocol.SAMPLE_RATE)
            r.decode(stream)
            r.getResult(stream).text.trim()
        } catch (e: Throwable) {
            Log.e(TAG, "识别失败", e)
            ""
        } finally {
            stream?.release()
        }
    }

    fun release() {
        rec?.release()
        rec = null
    }

    companion object {
        private const val TAG = "Asr"
    }
}
