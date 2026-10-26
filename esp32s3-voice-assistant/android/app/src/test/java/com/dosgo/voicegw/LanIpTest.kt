package com.dosgo.voicegw

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * 选局域网地址的测试 —— 这段逻辑在真机上出过错，必须锁住。
 *
 * 真实事故：手机开着加速 VPN 时会出现 `tun0 = 172.19.0.1`，
 * 旧代码只判断 192.168/10/172 前缀，于是显示了一个隧道地址，
 * ESP32 怎么都连不上，而且从界面上完全看不出问题。
 */
class LanIpTest {

    @Test
    fun `只有 VPN 和蜂窝时返回 null`() {
        // 真机上实测到的就是这个组合
        val ifaces = listOf(
            "ccmni1" to "10.8.37.80",
            "tun0" to "172.19.0.1",
        )
        assertNull("隧道和蜂窝都不能给板子用", LanIp.pick(ifaces))
    }

    @Test
    fun `有 WiFi 时优先 WiFi`() {
        val ifaces = listOf(
            "ccmni1" to "10.8.37.80",
            "tun0" to "172.19.0.1",
            "wlan0" to "192.168.1.7",
        )
        assertEquals("192.168.1.7", LanIp.pick(ifaces))
    }

    @Test
    fun `WiFi 即使不是常见网段也要优先`() {
        // 有些路由器用 10.x 或 172.16.x，不能因为网段不同就放弃
        val ifaces = listOf(
            "eth0" to "10.0.0.5",
            "wlan0" to "172.20.10.3",
        )
        assertEquals("172.20.10.3", LanIp.pick(ifaces))
    }

    @Test
    fun `只开热点时选热点接口`() {
        val ifaces = listOf(
            "ccmni1" to "10.8.37.80",
            "ap0" to "192.168.43.1",
        )
        assertEquals("192.168.43.1", LanIp.pick(ifaces))
    }

    @Test
    fun `没有 WiFi 但有其他网卡时退而求其次`() {
        val ifaces = listOf(
            "tun0" to "172.19.0.1",
            "eth0" to "192.168.0.9",
        )
        assertEquals("192.168.0.9", LanIp.pick(ifaces))
    }

    @Test
    fun `各种排除项都不选`() {
        val names = listOf("tun0", "ppp0", "ccmni1", "rmnet_data0", "clat4", "dummy0", "ip6tnl0")
        for (n in names) {
            assertTrue("$n 应被排除", LanIp.isExcluded(n))
            assertNull("$n 不该被选中", LanIp.pick(listOf(n to "1.2.3.4")))
        }
    }

    @Test
    fun `WiFi 类接口名都认`() {
        for (n in listOf("wlan0", "wlan1", "ap0", "softap0", "swlan0", "wifi0")) {
            assertTrue("$n 应被优先", LanIp.isPreferred(n))
            assertEquals("9.9.9.9", LanIp.pick(listOf("tun0" to "1.1.1.1", n to "9.9.9.9")))
        }
    }

    @Test
    fun `名字不相干的接口不算 WiFi`() {
        assertFalse(LanIp.isPreferred("eth0"))
        assertFalse(LanIp.isPreferred("ccmni1"))
        assertFalse(LanIp.isExcluded("wlan0"))
    }

    @Test
    fun `空列表返回 null`() {
        assertNull(LanIp.pick(emptyList()))
    }
}
