package com.dosgo.voicegw

import android.content.Context
import java.io.File

/**
 * 音色目录 —— 预先把 187 个音色按基频分了男女，省得用户一个个试。
 *
 * 数据来自 `assets/female_voices.txt`，生成方式（在 PC 上跑的）：
 *   对每个 sid 合成同一句话 → 自相关估基频取中位数 → ≥165Hz 判为女声。
 * 结果：187 个音色里 **151 个女声**、36 个男声，按音高从低到高排好。
 *
 * 为什么不在手机上现算：合成 187 条要一分多钟，而且每次启动都算一遍没意义。
 */
object VoiceCatalog {

    data class Voice(val sid: Int, val f0: Double)

    @Volatile
    private var cache: List<Voice>? = null

    fun females(ctx: Context): List<Voice> {
        cache?.let { return it }
        val list = ArrayList<Voice>()
        try {
            ctx.assets.open("female_voices.txt").bufferedReader().useLines { lines ->
                for (line in lines) {
                    val t = line.trim()
                    if (t.isEmpty() || t.startsWith("#")) continue
                    val parts = t.split(',')
                    if (parts.size < 2) continue
                    val sid = parts[0].toIntOrNull() ?: continue
                    val f0 = parts[1].toDoubleOrNull() ?: continue
                    list.add(Voice(sid, f0))
                }
            }
        } catch (_: Throwable) {
            // 资源缺失不影响主流程，退化成没有快捷切换
        }
        cache = list
        return list
    }

    /** 当前 sid 在女声列表里的位置，找不到返回 -1 */
    fun indexOf(list: List<Voice>, sid: Int): Int = list.indexOfFirst { it.sid == sid }

    /** 循环取下一个/上一个女声 */
    fun step(list: List<Voice>, sid: Int, delta: Int): Voice? {
        if (list.isEmpty()) return null
        val i = indexOf(list, sid)
        val next = if (i < 0) 0 else ((i + delta) % list.size + list.size) % list.size
        return list[next]
    }
}
