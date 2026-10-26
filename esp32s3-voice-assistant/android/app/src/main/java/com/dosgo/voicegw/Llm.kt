package com.dosgo.voicegw

import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL

/**
 * 云端大模型调用（OpenAI 兼容接口，硅基流动 / DeepSeek 都能用）
 *
 * 用 HttpURLConnection 而不是 OkHttp：少一个依赖，请求频率也不高（每次对话一次）。
 */
object Llm {

    private const val TAG = "Llm"

    data class Config(
        val apiKey: String,
        val baseUrl: String,   // 例如 https://api.siliconflow.cn/v1
        val model: String,     // 例如 Qwen/Qwen2.5-7B-Instruct
    )

    /**
     * @param history 形如 [("user","你好"), ("assistant","你好呀")]
     */
    fun chat(cfg: Config, history: List<Pair<String, String>>): String {
        if (cfg.apiKey.isBlank()) return "还没有配置大模型的 API Key，去设置里填一下。"
        if (cfg.baseUrl.isBlank()) return "还没有配置大模型的接口地址。"

        return try {
            val body = JSONObject().apply {
                put("model", cfg.model)
                put("max_tokens", 200)
                put("temperature", 0.7)
                put("messages", JSONArray().apply {
                    put(JSONObject().put("role", "system").put("content", Protocol.SYSTEM_PROMPT))
                    for ((role, content) in history) {
                        put(JSONObject().put("role", role).put("content", content))
                    }
                })
            }

            val url = URL(cfg.baseUrl.trimEnd('/') + "/chat/completions")
            val conn = url.openConnection() as HttpURLConnection
            conn.requestMethod = "POST"
            conn.connectTimeout = 15000
            conn.readTimeout = 30000
            conn.doOutput = true
            conn.setRequestProperty("Content-Type", "application/json")
            conn.setRequestProperty("Authorization", "Bearer ${cfg.apiKey}")

            conn.outputStream.use { it.write(body.toString().toByteArray()) }

            val code = conn.responseCode
            val stream = if (code in 200..299) conn.inputStream else conn.errorStream
            val text = stream?.bufferedReader()?.readText() ?: ""
            conn.disconnect()

            if (code !in 200..299) {
                Log.e(TAG, "HTTP $code: ${text.take(200)}")
                return "抱歉，大模型接口返回了 $code。"
            }

            JSONObject(text)
                .getJSONArray("choices")
                .getJSONObject(0)
                .getJSONObject("message")
                .getString("content")
                .trim()
        } catch (e: Throwable) {
            Log.e(TAG, "调用失败", e)
            "抱歉，网络好像不太好。"
        }
    }
}
