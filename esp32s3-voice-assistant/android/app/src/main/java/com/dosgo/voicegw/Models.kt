package com.dosgo.voicegw

import android.content.Context
import java.io.File

/**
 * 模型单例 —— 全 App 只加载一份。
 *
 * 【为什么必须有这个】
 * SenseVoice 的 int8 模型是 229MB。如果网关服务加载一份、测试页又加载一份，
 * 光模型就 460MB+，中低端机直接 OOM 被系统杀掉。
 * 所以 Asr / Tts 的实例统一从这里取，谁先要谁触发加载，后来者拿同一个对象。
 *
 * 两个线程同时点「启动网关」和「录音识别」时，loading 由 lock 保证只跑一次。
 */
object Models {

    private val lock = Any()

    @Volatile
    private var asrInst: Asr? = null

    @Volatile
    private var ttsInst: Tts? = null

    /** 模型放的根目录：优先外部私有目录（adb push 方便），没有就退到内部 */
    fun baseDir(ctx: Context): File =
        ctx.getExternalFilesDir(null) ?: ctx.filesDir

    fun asrDir(ctx: Context): File = File(baseDir(ctx), "asr")

    fun ttsDir(ctx: Context): File = File(baseDir(ctx), "tts")

    /**
     * 取 ASR，没有就加载（耗时 1-3 秒，必须在后台线程调）。
     *
     * 每一步都先写进 run.log 再执行 —— 模型加载走 C++，
     * native 层崩了进程直接消失，日志文件里最后一行就是崩在哪。
     */
    fun asr(ctx: Context): Asr = synchronized(lock) {
        asrInst ?: run {
            val dir = asrDir(ctx)
            val model = File(dir, "model.int8.onnx")
            CrashLog.step("加载 ASR：$dir")
            CrashLog.step("  model.int8.onnx=${model.length() / 1048576}MB " +
                "tokens=${File(dir, "tokens.txt").length()}B " +
                "可用堆=${Runtime.getRuntime().maxMemory() / 1048576}MB")
            val inst = Asr(dir)
            val ok = try {
                inst.load()
            } catch (t: Throwable) {
                CrashLog.step("ASR load 抛异常：${t.javaClass.name} ${t.message}")
                false
            }
            CrashLog.step("ASR 加载完成 ok=$ok isReady=${inst.isReady}")
            asrInst = inst
            inst
        }
    }

    /** 取 TTS，没有就加载（耗时 1-2 秒，必须在后台线程调） */
    fun tts(ctx: Context): Tts = synchronized(lock) {
        ttsInst ?: run {
            val dir = ttsDir(ctx)
            CrashLog.step("加载 TTS：$dir")
            val probe = Tts(dir)
            CrashLog.step("  引擎=${probe.detectEngine()} 文件清单见「运行日志」自检")
            val inst = probe
            val ok = try {
                inst.load()
            } catch (t: Throwable) {
                CrashLog.step("TTS load 抛异常：${t.javaClass.name} ${t.message}")
                false
            }
            CrashLog.step("TTS 加载完成 ok=$ok isReady=${inst.isReady} speakers=${inst.numSpeakers()}")
            ttsInst = inst
            inst
        }
    }

    fun asrLoaded(): Boolean = asrInst?.isReady == true

    fun ttsLoaded(): Boolean = ttsInst?.isReady == true

    /** 只在服务/界面都退出时调用 */
    fun releaseAll() = synchronized(lock) {
        asrInst?.release()
        ttsInst?.release()
        asrInst = null
        ttsInst = null
    }

    /** 模型文件被替换（比如刚下载完）时调用，下次访问会重新加载 */
    fun invalidate() = releaseAll()

    /** 只丢弃 TTS：单独下载音色模型后用它，别把已经加载好的 ASR 也释放掉 */
    fun invalidateTts() = synchronized(lock) {
        ttsInst?.release()
        ttsInst = null
    }

    /** 模型文件清单检查，给测试页和启动日志用 */
    data class FileCheck(val name: String, val path: File, val required: Boolean) {
        val ok: Boolean get() = path.exists() && path.length() > 0
        val sizeMb: Double get() = path.length() / 1024.0 / 1024.0
    }

    fun checkAsrFiles(ctx: Context): List<FileCheck> = listOf(
        FileCheck("model.int8.onnx", File(asrDir(ctx), "model.int8.onnx"), true),
        FileCheck("tokens.txt", File(asrDir(ctx), "tokens.txt"), true),
    )

    /**
     * TTS 文件清单。两套引擎布局不同：
     *   tts/kokoro/  Kokoro 24kHz（推荐）
     *   tts/ 或 tts/vits/  老 VITS 8kHz
     */
    fun checkTtsFiles(ctx: Context): List<FileCheck> {
        val base = ttsDir(ctx)
        val fanchen = File(base, "fanchen")
        if (File(fanchen, "model.onnx").exists()) {
            return listOf(
                FileCheck("model.onnx", File(fanchen, "model.onnx"), true),
                FileCheck("lexicon.txt", File(fanchen, "lexicon.txt"), true),
                FileCheck("tokens.txt", File(fanchen, "tokens.txt"), true),
                FileCheck("phone.fst", File(fanchen, "phone.fst"), false),
                FileCheck("date.fst", File(fanchen, "date.fst"), false),
                FileCheck("number.fst", File(fanchen, "number.fst"), false),
            )
        }
        val kok = File(base, "kokoro")
        // 有 Kokoro 就看 Kokoro，否则看老布局
        if (File(kok, "model.onnx").exists() || File(kok, "voices.bin").exists()) {
            return listOf(
                FileCheck("model.onnx", File(kok, "model.onnx"), true),
                FileCheck("voices.bin", File(kok, "voices.bin"), true),
                FileCheck("tokens.txt", File(kok, "tokens.txt"), true),
                FileCheck("lexicon-zh.txt", File(kok, "lexicon-zh.txt"), true),
                // 下面 3 个缺了只是数字/日期读得不对
                FileCheck("phone-zh.fst", File(kok, "phone-zh.fst"), false),
                FileCheck("date-zh.fst", File(kok, "date-zh.fst"), false),
                FileCheck("number-zh.fst", File(kok, "number-zh.fst"), false),
            )
        }
        val vits = if (File(base, "model.onnx").exists()) base else File(base, "vits")
        return listOf(
            FileCheck("model.onnx", File(vits, "model.onnx"), true),
            FileCheck("tokens.txt", File(vits, "tokens.txt"), true),
            FileCheck("lexicon.txt", File(vits, "lexicon.txt"), true),
            FileCheck("phone.fst", File(vits, "phone.fst"), false),
            FileCheck("date.fst", File(vits, "date.fst"), false),
            FileCheck("number.fst", File(vits, "number.fst"), false),
        )
    }

    /** 生成一段人能看懂的自检报告 */
    fun report(ctx: Context): String {
        val sb = StringBuilder()
        sb.append("ASR 目录：").append(asrDir(ctx).absolutePath).append('\n')
        for (f in checkAsrFiles(ctx)) {
            sb.append("  ").append(if (f.ok) "✅" else "❌").append(' ')
                .append(f.name)
            if (f.ok) sb.append("  ").append("%.1fMB".format(f.sizeMb))
            if (!f.ok && f.required) sb.append("   ← 必需，缺了不能识别")
            sb.append('\n')
        }

        sb.append("TTS 目录：").append(ttsDir(ctx).absolutePath).append('\n')
        for (f in checkTtsFiles(ctx)) {
            sb.append("  ").append(if (f.ok) "✅" else "—").append(' ')
                .append(f.name)
            if (f.ok) sb.append("  ").append("%.1fMB".format(f.sizeMb))
            if (!f.ok && f.required) sb.append("   ← 必需，缺了不能合成")
            sb.append('\n')
        }

        sb.append('\n')
        sb.append("ASR 模型：").append(if (asrLoaded()) "已加载" else "未加载").append('\n')
        sb.append("TTS 模型：").append(if (ttsLoaded()) "已加载（${ttsInst?.engineName()}）" else "未加载")
        return sb.toString()
    }
}
