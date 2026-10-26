package com.dosgo.voicegw

import android.app.Application

/**
 * 只为初始化崩溃日志。放在 Application 里是为了赶在 Activity 之前，
 * 这样连「启动就崩」也能留下痕迹。
 */
class App : Application() {
    override fun onCreate() {
        super.onCreate()
        CrashLog.init(this)
    }
}
