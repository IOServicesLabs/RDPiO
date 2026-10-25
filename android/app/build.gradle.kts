import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "io.ioservices.rdpio"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.ioservices.rdpio"
        // Android 11: MediaCodec's low-latency decoding, Surface.setFrameRate.
        minSdk = 30
        targetSdk = 36
        versionCode = 300
        versionName = "0.3.0"
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        release {
            // R8: the Kotlin standard library and the app shrunk to what's used,
            // inlined and optimised (smaller dex, less for ART to load and JIT).
            // Native's methods keep their names (the core's JNI symbols are
            // Java_io_ioservices_rdpio_Native_*): proguard-rules.pro. Resources
            // aren't shrunk (few, and looked up by id only).
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // Sideloaded for now: signed with the debug key.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    compilerOptions { jvmTarget.set(JvmTarget.JVM_17) }
}

dependencies {
    // Applies src/main/baseline-prof.txt on a sideloaded install too (Play
    // would otherwise be what does): the stream's code compiled ahead of time
    // in the background after the first launch, not JIT-compiled during the
    // first minutes of a stream.
    implementation("androidx.profileinstaller:profileinstaller:1.4.1")
}

// The native core (librdpio.so, Rust) is built by build-rust.sh before the app.
val bash: String = providers.gradleProperty("rdpio.bash").orNull
    ?: listOf("C:/Program Files/Git/bin/bash.exe", "/usr/bin/bash", "/bin/bash").firstOrNull { file(it).exists() }
    ?: "bash"

val buildRust = tasks.register<Exec>("buildRust") {
    description = "Builds the Rust core into src/main/jniLibs"
    workingDir = rootDir
    environment("ANDROID_HOME", android.sdkDirectory.absolutePath)
    commandLine(bash, "build-rust.sh", "release")
}

tasks.named("preBuild") { dependsOn(buildRust) }
