package com.dosgo.voicegw

import android.content.Context
import android.util.Log
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * 运行日志 —— 专治「一点就崩，但看不到原因」。
 *
 * 为什么不用普通的日志：模型加载走的是 C++（sherpa-onnx / onnxruntime）。
 * 一旦 native 层 SIGSEGV 或 abort，整个进程直接消失，Java 的 try/catch 和
 * UncaughtExceptionHandler 都拦不住，logcat 又在手机上不好看。
 *
 * 做法：
 *   · 每个关键步骤**先写盘再执行**，这样 native 崩溃时，文件里最后一行就是崩在哪一步
 *   · 写到外部私有目录 `/sdcard/Android/data/com.dosgo.voicegw/files/run.log`，
 *     连电脑用 MTP 就能拷出来
 *   · 同时接管未捕获异常（Java 层崩溃），把堆栈也写进去
 */
object CrashLog {

    @Volatile
    private var file: File? = null

    private val ts = SimpleDateFormat("MM-dd HH:mm:ss.SSS", Locale.US)
    private val sb = StringBuilder()

    /** 在 Application 里调一次 */
    fun init(ctx: Context) {
        val dir = ctx.getExternalFilesDir(null) ?: ctx.filesDir
        val f = File(dir, "run.log")
        file = f

        // 上次的运行日志留一份，方便事后翻
        runCatching {
            if (f.exists() && f.length() > 0) {
                f.copyTo(File(dir, "run.prev.log"), overwrite = true)
            }
        }
        runCatching { f.writeText("") }
        sb.setLength(0)

        val prev = Thread.getDefaultUncaughtExceptionHandler()
        Thread.setDefaultUncaughtExceptionHandler { t, e ->
            line("未捕获异常 ${e.javaClass.name}: ${e.message}")
            for (el in e.stackTrace.take(25)) line("    at $el")
            e.cause?.let { c ->
                line("  caused by ${c.javaClass.name}: ${c.message}")
                for (el in c.stackTrace.take(15)) line("    at $el")
            }
            prev?.uncaughtException(t, e)
        }

        line("==== App 启动 ====")
        line("设备 ${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}, " +
            "Android ${android.os.Build.VERSION.RELEASE} (API ${android.os.Build.VERSION.SDK_INT}), " +
            "${android.os.Build.SUPPORTED_ABIS.firstOrNull()}")
        line("内存 maxHeap=${Runtime.getRuntime().maxMemory() / 1048576}MB " + memInfo(ctx))
    }

    /** 系统可用内存 —— 用来区分「native 崩了」和「被系统因内存杀掉了」 */
    fun memInfo(ctx: Context): String {
        return try {
            val am = ctx.getSystemService(Context.ACTIVITY_SERVICE) as android.app.ActivityManager
            val mi = android.app.ActivityManager.MemoryInfo()
            am.getMemoryInfo(mi)
            "系统可用 ${mi.availMem / 1048576}MB, 低内存线 ${mi.threshold / 1048576}MB, " +
                "appLimit=${am.memoryClass}MB, 低内存=${mi.lowMemory}"
        } catch (t: Throwable) {
            "内存信息读取失败"
        }
    }

    /**
     * 记一步。**立刻落盘**——native 崩了以后没机会再 flush。
     */
    fun step(msg: String) {
        line(msg)
    }

    private fun line(msg: String) {
        val s = "${ts.format(Date())}  $msg"
        Log.i("VoiceGw", s)
        synchronized(sb) {
            sb.append(s).append('\n')
            // 落盘：每步都写，宁可慢一点也要保证崩的时候有据可查
            runCatching { file?.appendText(s + "\n") }
        }
    }

    fun path(): String = file?.absolutePath ?: "(未初始化)"

    fun dump(): String = synchronized(sb) { sb.toString() }

    fun clear() {
        synchronized(sb) {
            sb.setLength(0)
            runCatching { file?.writeText("") }
        }
    }
}
