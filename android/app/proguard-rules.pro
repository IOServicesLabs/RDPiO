# R8 rules beyond proguard-android-optimize.txt.

# The Rust core (librdpio.so) binds Native's methods by their JNI symbol names
# (Java_io_ioservices_rdpio_Native_<method>), so the class and every native
# method keep their names, and their parameter/return types too (Surface,
# ByteBuffer: framework classes, never renamed anyway). The core calls nothing
# in Java or Kotlin by name (no FindClass / CallMethod: it only returns
# strings and arrays), so nothing else needs keeping for it.
-keep class io.ioservices.rdpio.Native {
    native <methods>;
}

# Nothing in the app is reflective: activities, the service and the views are
# reached from the manifest (aapt2 writes their keep rules) or directly.

# Readable stack traces from the field: keep line numbers, rename the file.
-keepattributes SourceFile,LineNumberTable
-renamesourcefileattribute SourceFile
