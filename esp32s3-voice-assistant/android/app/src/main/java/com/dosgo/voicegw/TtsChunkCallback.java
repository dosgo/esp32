package com.dosgo.voicegw;

import kotlin.jvm.functions.Function1;

/**
 * sherpa-onnx 流式合成的回调 —— **必须用 Java 写，不能用 Kotlin lambda**。
 *
 * 原因：native 层（libsherpa-onnx-jni.so）是这样找回调的：
 *
 *     GetMethodID(cls, "invoke", "([F)Ljava/lang/Integer;")
 *
 * 也就是要求类上真的存在一个方法 `java.lang.Integer invoke(float[])`。
 * 而 Kotlin 的 lambda 编译成 `invoke(Object): Object`（bridge），
 * 签名对不上，native 拿不到 method id 后直接：
 *
 *     JNI DETECTED ERROR IN APPLICATION: JNI NewFloatArray called with pending exception
 *     java.lang.NoSuchMethodError: no non-static method
 *       "Lcom/dosgo/voicegw/Tts$$ExternalSyntheticLambda;.invoke([F)Ljava/lang/Integer;"
 *     → Abort message ... SIGABRT，整个进程被杀
 *
 * 用 Java 实现时，javac 会为泛型方法生成两个：
 *   · `Integer invoke(float[])`            ← native 要的正是这个
 *   · `Object invoke(Object)`（bridge）     ← 满足 Function1 接口
 * 所以这里绕一层。
 *
 * 返回值语义：**1 = 继续合成，0 = 立刻停止**（和 sherpa-onnx 官方示例一致）。
 */
public class TtsChunkCallback implements Function1<float[], Integer> {

    /** 真正的业务回调：返回 true 继续，false 停止 */
    public interface Sink {
        boolean onChunk(float[] samples);
    }

    private final Sink sink;

    public TtsChunkCallback(Sink sink) {
        this.sink = sink;
    }

    @Override
    public Integer invoke(float[] samples) {
        return sink.onChunk(samples) ? 1 : 0;
    }
}
