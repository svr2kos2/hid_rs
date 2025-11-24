plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.sayodevice.hid_rs"
    compileSdk = 34

    defaultConfig {
        minSdk = 24
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    
    sourceSets {
        getByName("main") {
            java.srcDirs("src/main/java")
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.12.0")
    implementation("org.jetbrains.kotlin:kotlin-stdlib:1.9.22")
}

tasks.register<Exec>("cargoBuild") {
    // This task builds the rust library for all android targets
    // You might need to install the targets first:
    // rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
    // And also install NDK and set up cargo-ndk if you want to use it, or configure .cargo/config.toml
    
    // For simplicity, we assume the environment is set up or we just try to build.
    // Note: Without proper linker configuration (e.g. via cargo-ndk), this might fail on some systems.
    // A robust solution would use the 'org.mozilla.rust-android-gradle.rust-android' plugin or similar.
    
    // Here is a basic implementation that tries to build for arm64-v8a (most common)
    // You can expand this list.
    val targets = mapOf(
        "aarch64-linux-android" to "arm64-v8a",
        "armv7-linux-androideabi" to "armeabi-v7a",
        "x86_64-linux-android" to "x86_64"
    )
    
    commandLine("cargo", "build", "--lib", "--release", "--target", "aarch64-linux-android") // Defaulting to one for now to avoid errors if others are missing
    // To build all:
    // commandLine("sh", "-c", "cargo build --lib --release --target aarch64-linux-android && cargo build --lib --release --target armv7-linux-androideabi ...")
    
    // We will use a doLast block to copy, but we need to actually run the build.
    // Since Exec task runs one command, we can use a script or multiple tasks.
    // Let's just define a task that calls cargo-ndk if available, or just cargo.
}

// Better approach: use a task that executes a shell script or multiple cargo commands.
tasks.register("buildRust") {
    doLast {
        exec {
            workingDir = file("..")
            commandLine("cargo", "ndk", 
                "-t", "aarch64-linux-android", 
                "-t", "armv7-linux-androideabi", 
                "-t", "x86_64-linux-android", 
                "-o", "android/src/main/jniLibs", 
                "build", "--release", "--lib")
        }
    }
}

tasks.named("preBuild") {
    dependsOn("buildRust")
}
