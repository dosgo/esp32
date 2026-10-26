package com.dosgo.voicegw

import android.content.Context
import java.io.File

/**
 * 模型下载器 —— 在 App 里直接把模型拉下来，不用连电脑。
 *
 * 【为什么要多源】
 * 这些模型原本托管在 GitHub releases 和 HuggingFace。国内手机不挂代理时：
 *   · huggingface.co   经常连不上
 *   · github release   能连但很慢，release-assets 域名时通时不通
 *   · hf-mirror.com    国内镜像，多数情况可用，所以放第一个
 * 所以按顺序试，哪个通就用哪个；也允许用户填自己的地址（家里 NAS、自己服务器）。
 *
 * 【断点续传】
 * 下到一半断网是常事（228MB 那个尤其）。这里先写 `<文件名>.part`，
 * 下次启动用 HTTP Range 从 `.part` 的长度往后接着下，下完才改名成正式文件。
 * 这样"下到 180MB 断网"不用从零开始。
 *
 * 【文件大小校验】
 * 服务器给了 Content-Length 就一定核对长度，长度不对宁可删掉重下——
 * 一个被截断的 .onnx 加载时会直接崩在 native 层，报错信息毫无线索，
 * 比重新下 228MB 更浪费时间。
 */
object ModelDownloader {

    /** 默认镜像，按顺序尝试 */
    val DEFAULT_MIRRORS = listOf(
        "https://hf-mirror.com",     // 国内镜像，优先
        "https://huggingface.co",    // 官方，需要网络能通
    )

    private const val ASR_REPO = "csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"
    /** Kokoro 多语种 v1.0，中文 24kHz、54 个音色 */
    private const val KOKORO_REPO = "csukuangfj/kokoro-multi-lang-v1_0"

    /**
     * @param remote 远端文件名
     * @param local  存到本地的文件名（TTS 的 int8 模型在仓库里叫 vits-aishell3.int8.onnx，
     *               但代码里统一按 model.onnx 找，所以落地时改名）
     */
    data class Spec(
        val group: String,
        val remote: String,
        val local: String,
        val repo: String,
        val expectMb: Double,
        val dest: File,
    )

    fun specs(ctx: Context): List<Spec> {
        val asrDir = Models.asrDir(ctx)
        val ttsDir = Models.ttsDir(ctx)
        return listOf(
            Spec("ASR", "model.int8.onnx", "model.int8.onnx", ASR_REPO, 228.2, File(asrDir, "model.int8.onnx")),
            Spec("ASR", "tokens.txt", "tokens.txt", ASR_REPO, 0.3, File(asrDir, "tokens.txt")),

            // Kokoro 放 tts/kokoro/ 子目录，别和老 VITS 的 model.onnx / tokens.txt 撞名
            Spec("TTS", "model.onnx", "model.onnx", KOKORO_REPO, 310.5, File(ttsDir, "kokoro/model.onnx")),
            Spec("TTS", "voices.bin", "voices.bin", KOKORO_REPO, 26.9, File(ttsDir, "kokoro/voices.bin")),
            Spec("TTS", "lexicon-zh.txt", "lexicon-zh.txt", KOKORO_REPO, 2.3, File(ttsDir, "kokoro/lexicon-zh.txt")),
            Spec("TTS", "tokens.txt", "tokens.txt", KOKORO_REPO, 0.0, File(ttsDir, "kokoro/tokens.txt")),
            Spec("TTS", "phone-zh.fst", "phone-zh.fst", KOKORO_REPO, 0.1, File(ttsDir, "kokoro/phone-zh.fst")),
            Spec("TTS", "date-zh.fst", "date-zh.fst", KOKORO_REPO, 0.1, File(ttsDir, "kokoro/date-zh.fst")),
            Spec("TTS", "number-zh.fst", "number-zh.fst", KOKORO_REPO, 0.1, File(ttsDir, "kokoro/number-zh.fst")),
        )
    }

    data class Progress(
        val index: Int,
        val count: Int,
        val label: String,
        val doneBytes: Long,
        val totalBytes: Long,   // -1 表示服务器没给长度
        val speedBps: Long,
    ) {
        val percent: Int
            get() = if (totalBytes > 0) ((doneBytes * 100) / totalBytes).toInt() else -1
    }

    /**
     * 下载所有缺失文件。
     *
     * @param customMirror 用户填的自定义源，非空就放最前面
     * @param cancelled 每读一块调一次，返回 true 立刻停（保留 .part 供续传）
     * @return 全部就位返回 true
     */
    fun downloadAll(
        ctx: Context,
        customMirror: String?,
        cancelled: () -> Boolean,
        onProgress: (Progress) -> Unit,
        onLog: (String) -> Unit,
    ): Boolean {
        val bases = buildList {
            val custom = customMirror?.trim()?.trimEnd('/')
            if (!custom.isNullOrEmpty()) add(custom)
            addAll(DEFAULT_MIRRORS)
        }.distinct()

        val specs = specs(ctx)
        // 大文件先下：万一半路放弃，至少小的已经齐了
        val ordered = specs.sortedByDescending { it.expectMb }
        val missing = ordered.filter { !it.dest.exists() || it.dest.length() == 0L }

        if (missing.isEmpty()) {
            onLog("模型已经齐了，不用下载")
            return true
        }

        val totalMb = missing.sumOf { it.expectMb }
        onLog("需要下载 ${missing.size} 个文件，约 ${"%.0f".format(totalMb)}MB")
        onLog("镜像顺序：${bases.joinToString(" → ")}")

        var i = 0
        for (spec in missing) {
            i++
            if (cancelled()) {
                onLog("已取消（下次会从这里续传）")
                return false
            }
            val ok = downloadOne(spec, bases, i, missing.size, cancelled, onProgress, onLog)
            if (!ok) {
                if (cancelled()) {
                    onLog("已取消")
                } else {
                    onLog("❌ 卡在 ${spec.remote}，所有源都不通。")
                    onLog("   可以在上面填一个能访问的地址（自己服务器 / NAS / 局域网电脑）再试。")
                }
                return false
            }
        }
        onLog("✅ 全部就位")
        return true
    }

    private fun downloadOne(
        spec: Spec,
        bases: List<String>,
        index: Int,
        count: Int,
        cancelled: () -> Boolean,
        onProgress: (Progress) -> Unit,
        onLog: (String) -> Unit,
    ): Boolean {
        val part = File(spec.dest.parentFile, spec.dest.name + ".part")
        spec.dest.parentFile?.mkdirs()

        for (base in bases) {
            if (cancelled()) return false

            val url = "$base/${spec.repo}/resolve/main/${spec.remote}"
            val resumeMb = if (part.exists()) part.length() / 1024 / 1024 else 0
            onLog(
                "【$index/$count】${spec.local}" +
                    if (resumeMb > 0) "（续传，已有 ${resumeMb}MB）" else ""
            )

            var lastBytes = 0L
            var lastTime = 0L
            val outcome = HttpDownloader.downloadTo(
                url = url,
                part = part,
                cancelled = cancelled,
                onProgress = { done, total ->
                    val now = System.currentTimeMillis()
                    val speed = if (lastTime > 0 && now > lastTime) {
                        (done - lastBytes) * 1000 / (now - lastTime)
                    } else {
                        0L
                    }
                    lastBytes = done
                    lastTime = now
                    onProgress(Progress(index, count, spec.local, done, total, speed))
                },
            )

            if (cancelled()) return false
            if (!outcome.ok) {
                onLog("  ✗ ${outcome.error ?: "失败"}（$base）")
                continue
            }

            // 下好了才改名，失败时 .part 留着下次续传
            if (spec.dest.exists()) spec.dest.delete()
            if (!part.renameTo(spec.dest)) {
                part.copyTo(spec.dest, overwrite = true)
                part.delete()
            }
            onProgress(Progress(index, count, spec.local, spec.dest.length(), spec.dest.length(), 0))
            onLog("  ✓ ${spec.local}  ${"%.1f".format(spec.dest.length() / 1024.0 / 1024.0)}MB")
            return true
        }
        return false
    }

    /** 有些源可能只是慢，清掉半截文件从头再来用 */
    fun clearPartials(ctx: Context) {
        for (s in specs(ctx)) {
            File(s.dest.parentFile, s.dest.name + ".part").delete()
        }
    }
}
