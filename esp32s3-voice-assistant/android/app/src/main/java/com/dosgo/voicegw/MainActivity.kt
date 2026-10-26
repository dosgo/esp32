package com.dosgo.voicegw

import android.Manifest
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.widget.Button
import android.widget.EditText
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import java.io.File
import java.util.concurrent.Executors

class MainActivity : AppCompatActivity() {

    private lateinit var tvStatus: TextView
    private lateinit var tvLog: TextView
    private lateinit var tvModelPath: TextView
    private lateinit var svLog: ScrollView
    private lateinit var btnToggle: Button
    private lateinit var etKey: EditText
    private lateinit var etBase: EditText
    private lateinit var etModel: EditText
    private lateinit var btnDownload: Button
    private lateinit var btnDlCancel: Button
    private lateinit var pbDownload: ProgressBar
    private lateinit var tvDlProgress: TextView

    private val logBuf = StringBuilder()

    /** 下载跑在独立线程，别跟网关的启动线程抢 */
    private val dlWorker = Executors.newSingleThreadExecutor { r ->
        Thread(r, "model-dl").apply { isDaemon = true }
    }

    @Volatile
    private var downloading = false

    @Volatile
    private var dlCancelled = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        tvStatus = findViewById(R.id.tvStatus)
        tvLog = findViewById(R.id.tvLog)
        tvModelPath = findViewById(R.id.tvModelPath)
        svLog = findViewById(R.id.svLog)
        btnToggle = findViewById(R.id.btnToggle)
        etKey = findViewById(R.id.etKey)
        etBase = findViewById(R.id.etBase)
        etModel = findViewById(R.id.etModel)
        btnDownload = findViewById(R.id.btnDownload)
        btnDlCancel = findViewById(R.id.btnDlCancel)
        pbDownload = findViewById(R.id.pbDownload)
        tvDlProgress = findViewById(R.id.tvDlProgress)

        findViewById<Button>(R.id.btnSave).setOnClickListener { saveConfig() }
        findViewById<Button>(R.id.btnTest).setOnClickListener {
            startActivity(Intent(this, TestActivity::class.java))
        }
        findViewById<Button>(R.id.btnLog).setOnClickListener { showRunLog() }
        findViewById<Button>(R.id.btnBattery).setOnClickListener { requestIgnoreBattery() }
        btnDownload.setOnClickListener { startDownload() }
        btnDlCancel.setOnClickListener {
            dlCancelled = true
            appendLog("正在取消下载…（已下部分保留，下次续传）")
        }
        btnToggle.setOnClickListener { toggle() }

        loadConfig()
        showModelPath()
        askNotificationPermission()

        // 服务 → UI 的回调
        GatewayService.logSink = { line ->
            runOnUiThread { appendLog(line) }
        }
        GatewayService.statusSink = { s ->
            runOnUiThread { tvStatus.text = s }
        }

        refreshButton()
        renderDownloadState()
    }

    override fun onDestroy() {
        dlCancelled = true
        dlWorker.shutdownNow()
        GatewayService.logSink = null
        GatewayService.statusSink = null
        super.onDestroy()
    }

    // ---------------------------------------------------------------- 启停

    private fun toggle() {
        if (GatewayService.running) {
            stopService(Intent(this, GatewayService::class.java))
            tvStatus.text = "已停止"
            appendLog("— 已停止 —")
        } else {
            if (!modelsPresent()) {
                Toast.makeText(this, "模型还没放好，见下方路径", Toast.LENGTH_LONG).show()
            }
            startForegroundService(Intent(this, GatewayService::class.java))
        }
        btnToggle.postDelayed({ refreshButton() }, 300)
    }

    private fun refreshButton() {
        btnToggle.text = if (GatewayService.running) "停止网关" else "启动网关"
        if (GatewayService.running && tvStatus.text == "未启动") {
            tvStatus.text = "运行中"
        }
    }

    private fun modelsPresent(): Boolean {
        val asrOk = Models.checkAsrFiles(this).filter { it.required }.all { it.ok }
        val ttsOk = Models.checkTtsFiles(this).filter { it.required }.all { it.ok }
        return asrOk && ttsOk
    }

    private fun showModelPath() {
        val base = Models.baseDir(this)
        tvModelPath.text = "模型目录：${base.absolutePath}\n" +
            "  asr/  需要 model.int8.onnx + tokens.txt\n" +
            "  tts/  需要 model.onnx + tokens.txt + lexicon.txt"
    }

    // ---------------------------------------------------------------- 运行日志

    /**
     * 把 run.log 直接显示出来。
     *
     * 崩溃排查的关键：模型加载走 C++，native 崩了系统不给任何提示，
     * 只有这份「下一步之前先写盘」的日志能看出崩在哪一步。
     */
    private fun showRunLog() {
        val cur = CrashLog.dump().ifBlank { "（本次运行还没有日志）" }
        val prevFile = File(Models.baseDir(this), "run.prev.log")
        val prev = if (prevFile.exists() && prevFile.length() > 0) {
            "\n\n===== 上次运行（末尾）=====\n" + prevFile.readText().takeLast(1500)
        } else {
            ""
        }
        val all = cur + prev

        AlertDialog.Builder(this)
            .setTitle("运行日志")
            .setMessage(
                all.takeLast(4000) + "\n\n文件：${CrashLog.path()}\n" +
                    "（连电脑可在 Android/data/com.dosgo.voicegw/files/ 里直接拷 run.log）"
            )
            .setPositiveButton("复制") { _, _ ->
                val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                cm.setPrimaryClip(ClipData.newPlainText("run.log", all))
                Toast.makeText(this, "已复制 ${all.length} 字符", Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton("关闭", null)
            .show()
    }

    // ---------------------------------------------------------------- 模型下载

    /** 主界面那行提示：缺几个文件、共多大；齐了就直说齐了 */
    private fun renderDownloadState() {
        if (downloading) return
        val specs = ModelDownloader.specs(this)
        val missing = specs.filter { !it.dest.exists() || it.dest.length() == 0L }
        pbDownload.visibility = android.view.View.GONE

        if (missing.isEmpty()) {
            tvDlProgress.text = "模型已就位"
            btnDownload.text = "重新下载缺失文件"
        } else {
            val mb = missing.sumOf { it.expectMb }
            tvDlProgress.text = "还缺 ${missing.size} 个文件，约 ${"%.0f".format(mb)}MB"
            btnDownload.text = if (mb > 0) "下载模型（约 ${"%.0f".format(mb)}MB）" else "下载缺失文件"
        }
    }

    private fun startDownload() {
        if (downloading) return
        val missing = ModelDownloader.specs(this).filter { !it.dest.exists() || it.dest.length() == 0L }
        if (missing.isEmpty()) {
            Toast.makeText(this, "模型已经齐了，不用下", Toast.LENGTH_SHORT).show()
            return
        }

        downloading = true
        dlCancelled = false
        btnDownload.isEnabled = false
        btnDlCancel.isEnabled = true
        pbDownload.visibility = android.view.View.VISIBLE
        pbDownload.progress = 0
        tvDlProgress.text = "准备下载…"

        dlWorker.execute {
            val ok = try {
                ModelDownloader.downloadAll(
                    this,
                    customMirror = null,
                    cancelled = { dlCancelled },
                    onProgress = { p -> runOnUiThread { showProgress(p) } },
                    onLog = { line -> runOnUiThread { appendLog(line) } },
                )
            } catch (t: Throwable) {
                runOnUiThread { appendLog("下载异常：${t.javaClass.simpleName} ${t.message}") }
                false
            }

            downloading = false
            runOnUiThread {
                btnDownload.isEnabled = true
                btnDlCancel.isEnabled = false
                pbDownload.visibility = android.view.View.GONE
                renderDownloadState()
                showModelPath()
            }

            if (ok) {
                Models.invalidate()
                runOnUiThread {
                    Toast.makeText(this, "模型下载完成", Toast.LENGTH_SHORT).show()
                    appendLog("模型已就位，可以点「启动网关」或去「本地测试」验证")
                }
            } else {
                runOnUiThread {
                    appendLog("下载未完成。想看详细原因和被卡的地址，去「本地测试」页点「下载缺失的模型」")
                }
            }
        }
    }

    private fun showProgress(p: ModelDownloader.Progress) {
        pbDownload.visibility = android.view.View.VISIBLE
        if (p.totalBytes > 0) {
            pbDownload.progress = p.percent
            val speed = p.speedBps / 1048576.0
            val left = if (p.speedBps > 0) (p.totalBytes - p.doneBytes) / p.speedBps else -1
            val eta = if (left >= 0) "  剩余 ${left / 60}:${"%02d".format(left % 60)}" else ""
            tvDlProgress.text = "【${p.index}/${p.count}】${p.label}  " +
                "%.1f/%.1fMB  %.2fMB/s%s".format(
                    p.doneBytes / 1048576.0, p.totalBytes / 1048576.0, speed, eta,
                )
        } else {
            pbDownload.progress = 0
            tvDlProgress.text = "【${p.index}/${p.count}】${p.label}  " +
                "%.1fMB".format(p.doneBytes / 1048576.0)
        }
    }

    // ---------------------------------------------------------------- 配置

    private fun loadConfig() {
        val p = getSharedPreferences(GatewayService.PREFS, Context.MODE_PRIVATE)
        etKey.setText(p.getString("apiKey", "") ?: "")
        etBase.setText(p.getString("baseUrl", "https://api.siliconflow.cn/v1"))
        etModel.setText(p.getString("model", "Qwen/Qwen2.5-7B-Instruct"))
        p.edit().putInt("port", p.getInt("port", 8765)).apply()
    }

    private fun saveConfig() {
        getSharedPreferences(GatewayService.PREFS, Context.MODE_PRIVATE).edit()
            .putString("apiKey", etKey.text.toString().trim())
            .putString("baseUrl", etBase.text.toString().trim())
            .putString("model", etModel.text.toString().trim())
            .apply()
        Toast.makeText(this, "已保存，重启网关后生效", Toast.LENGTH_SHORT).show()
        appendLog("配置已保存")
    }

    // ---------------------------------------------------------------- 权限

    private fun askNotificationPermission() {
        if (Build.VERSION.SDK_INT >= 33) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED
            ) {
                ActivityCompat.requestPermissions(
                    this, arrayOf(Manifest.permission.POST_NOTIFICATIONS), 100,
                )
            }
        }
    }

    private fun requestIgnoreBattery() {
        try {
            val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
            if (pm.isIgnoringBatteryOptimizations(packageName)) {
                Toast.makeText(this, "已经在白名单里了", Toast.LENGTH_SHORT).show()
                return
            }
            startActivity(
                Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
                    .setData(Uri.parse("package:$packageName"))
            )
        } catch (e: Throwable) {
            // 有些 ROM 不给直接用这个 action，退回到列表页让用户自己找
            try {
                startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
            } catch (_: Throwable) {
                Toast.makeText(this, "请到系统设置里手动关闭电池优化", Toast.LENGTH_LONG).show()
            }
        }
    }

    // ---------------------------------------------------------------- 日志

    private fun appendLog(line: String) {
        logBuf.append(line).append('\n')
        // 只留最近 300 行，防止越滚越慢
        val lines = logBuf.lines()
        if (lines.size > 300) {
            logBuf.setLength(0)
            logBuf.append(lines.takeLast(300).joinToString("\n"))
        }
        tvLog.text = logBuf.toString()
        svLog.post { svLog.fullScroll(ScrollView.FOCUS_DOWN) }
    }
}
