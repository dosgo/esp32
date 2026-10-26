package com.dosgo.voicegw

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.util.Log
import java.io.File
import java.net.Inet4Address
import java.net.NetworkInterface
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.concurrent.Executors

/**
 * 前台服务 —— 整个网关的宿主。
 *
 * 为什么必须是前台服务：Android 8 之后后台服务会被系统随时冻结，
 * App 一退到后台 ESP32 就连不上了。前台服务带常驻通知，才不会被杀。
 */
class GatewayService : Service() {

    companion object {
        const val CHANNEL_ID = "voicegw"
        const val NOTIF_ID = 1001
        const val PREFS = "voicegw"

        @Volatile var running = false
            private set

        /** UI 通过它收日志 */
        @Volatile var logSink: ((String) -> Unit)? = null
        @Volatile var statusSink: ((String) -> Unit)? = null

        private val ts = SimpleDateFormat("HH:mm:ss", Locale.getDefault())

        fun log(msg: String) {
            Log.i("VoiceGw", msg)
            logSink?.invoke("${ts.format(Date())} $msg")
        }

        fun status(msg: String) {
            statusSink?.invoke(msg)
        }
    }

    private var server: VoiceWsServer? = null
    private var asr: Asr? = null
    private val boot = Executors.newSingleThreadExecutor { r ->
        Thread(r, "gw-boot").apply { isDaemon = true }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (running) return START_STICKY
        running = true

        val notif = buildNotification("正在加载模型…")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIF_ID, notif)
        }

        boot.execute {
            try {
                startUp()
            } catch (t: Throwable) {
                CrashLog.step("startUp 抛异常：${t.javaClass.name} ${t.message}")
                log("❌ 启动异常：${t.message}")
            }
        }
        return START_STICKY
    }

    private fun startUp() {
        CrashLog.step("网关启动")
        val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val port = prefs.getInt("port", 8765)
        val baseDir = getExternalFilesDir(null) ?: filesDir

        val asrDir = File(baseDir, "asr")

        log("模型目录：${baseDir.absolutePath}")
        CrashLog.step("模型目录 $baseDir port=$port")

        // 走 Models 单例：如果测试页已经加载过，这里直接复用，不会再吃 229MB
        status("加载识别模型…")
        asr = Models.asr(this).also {
            if (!it.isReady) {
                log("⚠️ ASR 模型未加载（${asrDir.absolutePath} 里缺 model.int8.onnx / tokens.txt）")
            } else {
                log("ASR 就绪")
            }
        }

        // TTS 不在这里加载：第一次要说话时才加载。
        // 好处是启动只吃 ASR 的 229MB；万一 TTS native 初始化有问题，
        // 也不会把整个网关带崩，日志还能明确停在「加载 TTS」。
        log("TTS 将在第一次合成时加载")

        CrashLog.step("准备启动 WebSocket 服务")
        status("启动 WebSocket 服务…")
        val ip = localIp()
        try {
            server = VoiceWsServer(
                port = port,
                asr = asr!!,
                ttsProvider = { Models.tts(applicationContext) },
                speakerIdProvider = {
                    prefs.getInt("ttsSpeaker", 0)
                },
                llmConfig = {
                    Llm.Config(
                        apiKey = prefs.getString("apiKey", "") ?: "",
                        baseUrl = prefs.getString("baseUrl", "https://api.siliconflow.cn/v1")
                            ?: "https://api.siliconflow.cn/v1",
                        model = prefs.getString("model", "Qwen/Qwen2.5-7B-Instruct")
                            ?: "Qwen/Qwen2.5-7B-Instruct",
                    )
                },
                logger = { log(it) },
            ).also { it.start() }

            val info = "ws://$ip:$port"
            CrashLog.step("WebSocket 已监听 $info")
            log("已启动 $info")
            status(info)
            updateNotification("运行中 $info")
        } catch (e: Throwable) {
            Log.e("GatewayService", "启动失败", e)
            CrashLog.step("启动失败：${e.javaClass.name} ${e.message}")
            log("❌ 启动失败：${e.message}")
            status("启动失败")
        }
    }

    /**
     * 挑一个「板子能连上」的地址。
     *
     * 之前只判断前缀是不是 192.168/10/172，结果手机上开着 VPN 加速时
     * 会挑到 tun0 的 172.19.0.1（隧道地址，局域网里根本不可达）。
     * 现在明确排除蜂窝、VPN 隧道、USB 网卡这类接口，优先 WiFi / 热点。
     */
    private fun localIp(): String {
        val candidates = mutableListOf<Pair<String, String>>()
        try {
            val ifaces = java.util.Collections.list(NetworkInterface.getNetworkInterfaces())
            for (iface in ifaces) {
                if (!iface.isUp || iface.isLoopback) continue
                val name = iface.name ?: continue
                for (a in java.util.Collections.list(iface.inetAddresses)) {
                    if (a is Inet4Address && !a.isLoopbackAddress) {
                        a.hostAddress?.let { candidates.add(name to it) }
                    }
                }
            }
        } catch (_: Throwable) {
        }

        val picked = LanIp.pick(candidates)
        if (picked != null) {
            val ifaceName = candidates.first { it.second == picked }.first
            log("监听地址来自网卡 $ifaceName = $picked")
            return picked
        }

        // 只剩蜂窝/VPN：板子连不上，如实说出来
        val all = candidates.joinToString(", ") { "${it.first}=${it.second}" }.ifEmpty { "无" }
        log("⚠️ 没有局域网地址（现有接口：$all）。手机没连 WiFi 也没开热点，ESP32 连不上")
        return "手机IP（先连 WiFi 或开热点）"
    }

    // ---------------------------------------------------------------- 通知

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                CHANNEL_ID, "语音网关", NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "保持语音网关在后台运行"
                setShowBadge(false)
            }
            (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
                .createNotificationChannel(ch)
        }
    }

    private fun buildNotification(text: String): Notification {
        val pi = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("语音网关")
            .setContentText(text)
            .setSmallIcon(R.drawable.ic_launcher_fg)
            .setContentIntent(pi)
            .setOngoing(true)
            .build()
    }

    private fun updateNotification(text: String) {
        (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager)
            .notify(NOTIF_ID, buildNotification(text))
    }

    // ---------------------------------------------------------------- 退出

    override fun onDestroy() {
        running = false
        server?.shutdownAll()
        server = null
        // 只释放自己这一份引用；模型本体由 Models 管理，测试页/其他页面可能还在用
        asr = null
        boot.shutdownNow()
        super.onDestroy()
    }
}
