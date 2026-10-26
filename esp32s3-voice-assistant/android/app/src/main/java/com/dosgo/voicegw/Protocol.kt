package com.dosgo.voicegw

/**
 * 协议常量 —— 必须和 ESP32 固件、以及 Python 版 gateway.py 完全一致。
 *
 * 客户端（ESP32）上行：16kHz / 16bit / 单声道 PCM，每帧 1920 字节（60ms）
 * 服务端（本 App）下行：同样的 PCM，每帧 ≤1920 字节
 * 控制消息走 JSON 文本帧。
 */
object Protocol {
    const val SAMPLE_RATE = 16000
    const val FRAME_MS = 60
    const val FRAME_BYTES = SAMPLE_RATE * 2 * FRAME_MS / 1000   // 1920

    // VAD 参数（和 Python 版保持一致）
    const val VAD_RMS_THRESHOLD = 500
    const val VAD_SILENCE_MS = 700
    const val VAD_MIN_SPEECH_MS = 300
    const val VAD_MAX_UTTERANCE_MS = 15000
    const val AWAKE_TIMEOUT_S = 90

    val WAKE_WORDS = listOf("你好小智", "小智小智", "嗨小智", "嘿小智", "小智")

    // 同音兜底：ASR 容易把「小智」听成「小芝 / 小志 / 晓智」
    // 手机端不引拼音库（体积/依赖），用一张常见同音字表覆盖
    val WAKE_HOMOPHONES = listOf(
        "你好小芝", "小芝小芝", "嗨小芝", "嘿小芝", "小芝",
        "小志小志", "晓智", "小智同学", "你好小志",
    )

    const val SYSTEM_PROMPT =
        "你是一个运行在 ESP32 小音箱里的语音助手。" +
        "回答要短：一般不超过两句、60 个字以内，因为你的回答会被转成语音播放。" +
        "不要用 Markdown、不要用列表、不要输出表情符号。" +
        "不确定的事情直接说不知道，不要编。"

    fun isWakeWord(text: String): Boolean {
        if (text.isEmpty()) return false
        if (WAKE_WORDS.any { text.contains(it) }) return true
        if (WAKE_HOMOPHONES.any { text.contains(it) }) return true
        return false
    }
}

/** 下行 JSON 消息 */
fun jsonMsg(type: String, vararg kv: Pair<String, String>): String {
    val sb = StringBuilder()
    sb.append("{\"type\":\"").append(type).append('"')
    for ((k, v) in kv) {
        sb.append(",\"").append(k).append("\":\"")
        sb.append(v.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", " "))
        sb.append('"')
    }
    sb.append('}')
    return sb.toString()
}

/**
 * 从 JSON 里取一个字段，够用就行，不引 JSON 库。
 *
 * 两种都要支持，因为固件发的 hello 是这样的（数字不带引号）：
 *   {"type":"hello","version":1,"format":"pcm","sample_rate":16000,...}
 * 带引号的走转义解析，裸数字/true/false 原样读出。
 */
fun jsonStr(json: String, key: String): String? {
    val pat = "\"$key\""
    var i = json.indexOf(pat)
    if (i < 0) return null
    i = json.indexOf(':', i + pat.length)
    if (i < 0) return null
    i++
    while (i < json.length && json[i].isWhitespace()) i++
    if (i >= json.length) return null

    // 数字 / true / false 这类字面量，读到分隔符为止
    if (json[i] != '"') {
        val start = i
        while (i < json.length && json[i] != ',' && json[i] != '}' && json[i] != ']') i++
        return json.substring(start, i).trim().ifEmpty { null }
    }

    i++
    val sb = StringBuilder()
    while (i < json.length) {
        val c = json[i]
        if (c == '\\' && i + 1 < json.length) {
            i++
            when (json[i]) {
                'n' -> sb.append('\n')
                'r' -> sb.append('\r')
                't' -> sb.append('\t')
                '"' -> sb.append('"')
                '\\' -> sb.append('\\')
                else -> sb.append(json[i])
            }
        } else if (c == '"') {
            return sb.toString()
        } else {
            sb.append(c)
        }
        i++
    }
    return null
}
