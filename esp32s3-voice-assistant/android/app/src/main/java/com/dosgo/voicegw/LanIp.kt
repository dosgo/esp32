package com.dosgo.voicegw

/**
 * 挑一个「ESP32 能连上」的本机地址。
 *
 * 为什么单独抽出来：这段逻辑之前在真机上挑错过——手机开着加速 VPN 时，
 * `tun0` 有个 172.19.0.1，而旧代码只判断前缀是不是 192.168/10/172，
 * 于是把一个隧道地址当成局域网地址显示出来，板子怎么都连不上。
 *
 * 现在规则很明确：先排除不可能被板子访问的接口，再优先 WiFi / 热点。
 * 纯函数，配单元测试（见 `LanIpTest`）。
 */
object LanIp {

    /** 板子永远连不上的接口：蜂窝、VPN 隧道、USB 网络、clat 翻译层 */
    private val EXCLUDED = listOf("tun", "ppp", "ccmni", "rmnet", "clat", "dummy", "v4-", "ip6tnl")

    /** 可能是 Wi-Fi 或热点的接口名（各厂商叫法不同） */
    private val PREFERRED = listOf("wlan", "ap", "softap", "swlan", "wifi")

    fun isExcluded(name: String): Boolean = EXCLUDED.any { name.startsWith(it) }

    fun isPreferred(name: String): Boolean = PREFERRED.any { name.startsWith(it) }

    /**
     * @param candidates 形如 [("wlan0", "192.168.1.7"), ("tun0", "172.19.0.1")]
     * @return 可用的局域网地址；只剩蜂窝/VPN 时返回 null
     */
    fun pick(candidates: List<Pair<String, String>>): String? {
        val usable = candidates.filterNot { isExcluded(it.first) }
        val preferred = usable.firstOrNull { isPreferred(it.first) }
        return preferred?.second ?: usable.firstOrNull()?.second
    }
}
