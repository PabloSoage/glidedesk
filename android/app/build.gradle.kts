plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "io.github.pablosoage.glidedesk"
    compileSdk = 37
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "io.github.pablosoage.glidedesk"
        minSdk = 29
        targetSdk = 36
        versionCode = 2
        versionName = "0.2.0"
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    // Release signing from the environment (never in the repository):
    // GLIDEDESK_KEYSTORE (path), GLIDEDESK_KEYSTORE_PASSWORD, GLIDEDESK_KEY_ALIAS,
    // GLIDEDESK_KEY_PASSWORD. Without them, release builds use the debug key.
    val keystore = System.getenv("GLIDEDESK_KEYSTORE")
    signingConfigs {
        if (keystore != null) {
            create("release") {
                storeFile = file(keystore)
                storePassword = System.getenv("GLIDEDESK_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("GLIDEDESK_KEY_ALIAS")
                keyPassword = System.getenv("GLIDEDESK_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }
    buildFeatures {
        compose = true
        buildConfig = true
    }
    sourceSets {
        getByName("main") {
            jniLibs.directories.add("src/main/jniLibs")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_21)
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.material.icons.extended)
}

// --- Rust core (crates/android) through cargo-ndk, as in Rustify ---

val rustTargets = listOf("arm64-v8a", "x86_64")
val isWindows = System.getProperty("os.name").lowercase().contains("windows")
val cargoHome = System.getenv("CARGO_HOME") ?: (System.getProperty("user.home") + "/.cargo")
val cargoFile = file("$cargoHome/bin/" + if (isWindows) "cargo.exe" else "cargo")
val cargoCommand: String = if (cargoFile.exists()) cargoFile.absolutePath else "cargo"

rustTargets.forEach { target ->
    tasks.register<Exec>("buildRust_$target") {
        group = "rust"
        description = "Builds the glidedesk Android core for $target with cargo-ndk"
        workingDir = file("../..")
        val components = project.extensions.getByType<com.android.build.api.variant.ApplicationAndroidComponentsExtension>()
        environment("ANDROID_NDK_HOME", components.sdkComponents.ndkDirectory.get().asFile.absolutePath)
        // Without debug info the library is ~10x smaller (the APK went from 84 MB).
        environment("CARGO_PROFILE_RELEASE_DEBUG", "false")
        environment("CARGO_PROFILE_RELEASE_STRIP", "symbols")
        inputs.dir("../../crates/client/src")
        inputs.dir("../../crates/android/src")
        inputs.dir("../../crates/proto/src")
        outputs.dir("src/main/jniLibs/$target")
        commandLine(
            cargoCommand, "ndk", "-t", target, "-o", "android/app/src/main/jniLibs",
            "build", "--release", "-p", "glidedesk-android",
        )
    }
}

tasks.register("buildRustAll") {
    group = "rust"
    dependsOn(rustTargets.map { "buildRust_$it" })
}

tasks.configureEach {
    if (name.startsWith("merge") && name.endsWith("JniLibFolders")) {
        dependsOn("buildRustAll")
    }
}
