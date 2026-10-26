plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.dosgo.voicegw"
    compileSdk = 37

    defaultConfig {
        applicationId = "com.dosgo.voicegw"
        minSdk = 26          // 前台服务从 Android 8.0 起是硬要求
        targetSdk = 34
        versionCode = 1
        versionName = "1.0"

        // sherpa-onnx 的 AAR 只带了 arm64-v8a 的 so，其他架构装不进去
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    // Kotlin 2.x 用 compilerOptions，旧的 kotlinOptions 已废弃

    // AAR 里的 .so 要能被解压安装
    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")

    // sherpa-onnx 官方 AAR（含 libonnxruntime.so + libsherpa-onnx-c-api.so）
    // 来源：https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.0/sherpa-onnx-2.0.0.aar
    implementation(files("libs/sherpa-onnx.aar"))

    // 手写 WebSocket 服务端太容易出错，用这个纯 Java 的库
    implementation("org.java-websocket:Java-WebSocket:1.5.6")

    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")

    // 单元测试：协议 / 唤醒词 / JSON / PCM 转换 / 下载逻辑，不需要真机
    testImplementation("junit:junit:4.13.2")
    // 下载测试要模拟 308 → 302 跳转和 Range 续传，自己写 HTTP 服务器不如用它
    testImplementation("com.squareup.okhttp3:mockwebserver:4.12.0")
}
