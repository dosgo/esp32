# sherpa-onnx 内部走 JNI，混淆会破坏类名/方法名
-keep class com.k2fsa.sherpa.onnx.** { *; }
-dontwarn com.k2fsa.sherpa.onnx.**
