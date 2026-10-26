package com.dosgo.voicegw

/**
 * PCM 工具 —— 故意不碰任何 Android API，这样可以在普通 JVM 单元测试里跑。
 *
 * 全部按 16kHz / 16bit / 单声道 / 小端处理，和固件、Python 网关一致。
 */
object AudioPcm {

    /**
     * 任意采样率的 float 采样（[-1,1]）→ 16kHz s16le 字节。
     *
     * 用线性插值而不是直接丢点：22050 → 16000 是降采样，直接丢点会产生
     * 明显的混叠噪声（听感上是"沙沙"的高频毛刺）。
     */
    fun to16kPcm(src: FloatArray, srcRate: Int): ByteArray {
        val dstRate = Protocol.SAMPLE_RATE
        if (src.isEmpty() || srcRate <= 0) return ByteArray(0)

        val n = if (srcRate == dstRate) {
            src.size
        } else {
            // 用 Long 算，防止长语音（几万采样）时 Int 溢出
            (src.size.toLong() * dstRate / srcRate).toInt().coerceAtLeast(1)
        }

        val out = FloatArray(n)
        if (srcRate == dstRate) {
            System.arraycopy(src, 0, out, 0, n)
        } else {
            val step = srcRate.toDouble() / dstRate
            val last = src.size - 1
            for (i in 0 until n) {
                val pos = i * step
                val i0 = pos.toInt().coerceIn(0, last)
                val i1 = (i0 + 1).coerceAtMost(last)
                val frac = (pos - i0).toFloat()
                out[i] = src[i0] * (1 - frac) + src[i1] * frac
            }
        }

        val pcm = ByteArray(n * 2)
        for (i in 0 until n) {
            // 先夹到 [-1,1] 再乘，避免 float 溢出成 32768 回绕成 -32768（爆音）
            val v = (out[i].coerceIn(-1f, 1f) * 32767f).toInt()
            pcm[i * 2] = (v and 0xFF).toByte()
            pcm[i * 2 + 1] = ((v shr 8) and 0xFF).toByte()
        }
        return pcm
    }

    /** s16le 字节 → [-1,1] 的 float 采样，喂给 sherpa-onnx 用 */
    fun toFloatSamples(pcm: ByteArray): FloatArray {
        val n = pcm.size / 2
        val out = FloatArray(n)
        for (i in 0 until n) {
            val lo = pcm[i * 2].toInt() and 0xFF
            val hi = pcm[i * 2 + 1].toInt() and 0xFF
            out[i] = ((hi shl 8) or lo).toShort() / 32768.0f
        }
        return out
    }

    /** 一帧的 RMS，用于能量 VAD */
    fun rms(pcm: ByteArray): Int {
        val n = pcm.size / 2
        if (n == 0) return 0
        var sum = 0.0
        for (i in 0 until n) {
            val lo = pcm[i * 2].toInt() and 0xFF
            val hi = pcm[i * 2 + 1].toInt() and 0xFF
            val s = ((hi shl 8) or lo).toShort().toInt()
            // 转成 Long 再平方：32767^2 = 1.07e9，几个采样就溢出 Int
            sum += s.toLong() * s.toLong()
        }
        return Math.sqrt(sum / n).toInt()
    }
}
