import java.util.Properties

plugins {
    id("com.android.application")
}

// Version: the workspace version from the root Cargo.toml (override: -PbunkoVersion=…).
val cargoToml = rootProject.file("../../Cargo.toml")
val bunkoVersion: String = (findProperty("bunkoVersion") as String?)
    ?: Regex("""(?m)^version\s*=\s*"([^"]+)"""").find(cargoToml.readText())?.groupValues?.get(1)
    ?: error("no version in $cargoToml")

/**
 * A monotonic versionCode from a semver: MMmmpp + a pre-release slot
 * (alpha.N → N, beta.N → 30+N, rc.N → 60+N, release → 99), so 0.7.0-alpha.1 = 70001,
 * 0.7.0 = 70099, 0.7.1-rc.2 = 70162.
 */
fun versionCodeOf(v: String): Int {
    val m = Regex("""^(\d+)\.(\d+)\.(\d+)(?:-([a-z]+)\.?(\d+)?)?""").find(v) ?: error("bad version $v")
    val (major, minor, patch) = m.destructured.let { Triple(it.component1().toInt(), it.component2().toInt(), it.component3().toInt()) }
    val n = m.groupValues[5].ifEmpty { "0" }.toInt().coerceIn(0, 29)
    val pre = when (m.groupValues[4]) {
        "" -> 99
        "alpha" -> n
        "beta" -> 30 + n
        "rc" -> 60 + n
        else -> 0
    }
    return ((major * 100 + minor) * 100 + patch) * 100 + pre
}

// Release signing: keystore.properties next to this file, or BUNKO_ANDROID_KEYSTORE* env
// (CI). Without either, the release APK is signed with the debug key so it installs
// for testing — see docs/rust-port/MOBILE.md before publishing one.
val ks = Properties().apply {
    rootProject.file("keystore.properties").takeIf { it.exists() }?.inputStream()?.use { load(it) }
}
fun signingValue(prop: String, env: String): String? = ks.getProperty(prop) ?: System.getenv(env)?.takeIf { it.isNotEmpty() }
val releaseStore = signingValue("storeFile", "BUNKO_ANDROID_KEYSTORE")

android {
    namespace = "app.mokuro.bunko"
    compileSdk = 36
    // Kept in step with packaging/android/build.sh and the CI job.
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "app.mokuro.bunko"
        minSdk = 26
        targetSdk = 36
        versionCode = versionCodeOf(bunkoVersion)
        versionName = bunkoVersion
    }

    signingConfigs {
        if (releaseStore != null) {
            create("release") {
                storeFile = file(releaseStore)
                storePassword = signingValue("storePassword", "BUNKO_ANDROID_KEYSTORE_PASSWORD")
                keyAlias = signingValue("keyAlias", "BUNKO_ANDROID_KEY_ALIAS")
                keyPassword = signingValue("keyPassword", "BUNKO_ANDROID_KEY_PASSWORD")
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
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        // The .so files are already stripped by cargo (profile.release strip = "symbols").
        jniLibs.keepDebugSymbols += "**/libbunko_android.so"
        // Store native libraries uncompressed and page-aligned (16 KB pages, Android 15+).
        jniLibs.useLegacyPackaging = false
    }

    lint {
        // The release build is driven by build.sh/CI; a lint failure must not block it.
        abortOnError = false
        checkReleaseBuilds = false
    }
}

// The native library is built by build.sh (cargo ndk); fail with a hint, not a crash at
// runtime, when it is missing.
val checkRustLibs = tasks.register("checkRustLibs") {
    val dir = layout.projectDirectory.dir("src/main/jniLibs")
    doLast {
        val libs = dir.asFile.walkTopDown().filter { it.name == "libbunko_android.so" }.toList()
        if (libs.isEmpty()) {
            throw GradleException("No libbunko_android.so under ${dir.asFile}: run packaging/android/build.sh")
        }
    }
}
tasks.named("preBuild") { dependsOn(checkRustLibs) }
