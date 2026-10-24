plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "io.anonchat.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.anonchat.app"
        minSdk = 26
        targetSdk = 35
        versionCode = 34
        versionName = "0.15.7"
        // ABI 由下面的 splits 控制，这里不能再写 abiFilters——
        // 两者同时设置 AGP 会直接报冲突。
    }

    /**
     * 正式发布用的签名密钥。**绝不写进仓库**（.gitignore 已挡 *.jks）。
     *
     * 从环境变量或 ~/.gradle/gradle.properties 读，没配置就回退到 debug 签名——
     * 本地开发和自测不受影响。为什么必须有这一步：
     * 1. debug keystore 是每台机器随机构建的，换机器编出来的包签名不同，
     *    用户覆盖安装会直接报 INSTALL_FAILED_UPDATE_INCOMPATIBLE；
     * 2. debug keystore 的密码是公开的（"android"），任何人都能签一个
     *    同包名的包冒充官方更新。
     *
     * 生成密钥（只做一次，密钥文件务必备份，丢了就再也发不了更新）：
     *   keytool -genkeypair -v -keystore veilwire.jks -alias veilwire \
     *     -keyalg RSA -keysize 4096 -validity 10000
     * 然后发布前设置：
     *   VEILWIRE_KEYSTORE=/绝对路径/veilwire.jks
     *   VEILWIRE_KEYSTORE_PASSWORD=... VEILWIRE_KEY_ALIAS=veilwire VEILWIRE_KEY_PASSWORD=...
     */
    val releaseKeystore: String? = System.getenv("VEILWIRE_KEYSTORE")
        ?: providers.gradleProperty("veilwire.keystore").orNull

    signingConfigs {
        if (!releaseKeystore.isNullOrBlank() && file(releaseKeystore).exists()) {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = System.getenv("VEILWIRE_KEYSTORE_PASSWORD")
                    ?: providers.gradleProperty("veilwire.keystorePassword").orNull
                keyAlias = System.getenv("VEILWIRE_KEY_ALIAS")
                    ?: providers.gradleProperty("veilwire.keyAlias").orNull
                keyPassword = System.getenv("VEILWIRE_KEY_PASSWORD")
                    ?: providers.gradleProperty("veilwire.keyPassword").orNull
            }
        }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
            // 方便直接装到真机验收
            isDebuggable = true
        }
        release {
            isMinifyEnabled = false
            // 配了正式密钥就用它，否则回退 debug（仅够自测，不能对外分发）
            signingConfig = signingConfigs.findByName("release")
                ?: signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
    }
    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }

    // 按 ABI 拆包。
    // WebRTC 的 libjingle 每个架构 11~12MB，打成一个包就是 44MB；
    // 拆开之后 arm64 单包约 25MB，覆盖几乎所有在用手机。
    splits {
        abi {
            isEnable = true
            reset()
            include("arm64-v8a", "x86_64")
            isUniversalApk = false
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation(platform("androidx.compose:compose-bom:2024.10.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")

    // 视频播放。
    // 不交给系统播放器：一跳出去用户就离开了聊天，回来还得重新找会话。
    // PlayerView 自带进度条/暂停/拖动，比自己用 VideoView + MediaController 拼的稳。
    implementation("androidx.media3:media3-exoplayer:1.4.1")
    implementation("androidx.media3:media3-ui:1.4.1")

    // WebRTC。
    // 不要用 org.webrtc:google-webrtc —— 官方那个 AAR 早已停止更新（最后一版 1.0.32006）。
    // webrtc-sdk 是社区维护的活跃分支，含新版 libwebrtc，Maven Central 可直达。
    implementation("io.github.webrtc-sdk:android:125.6422.07")

    // 二维码：zxing core 只做生成（无 UI 依赖）；
    // zxing-android-embedded 提供扫码 Activity（内置相机处理与对焦）。
    // 阿里云镜像可拉，两个库都极小（core ~600KB）。
    implementation("com.google.zxing:core:3.5.3")
    implementation("com.journeyapps:zxing-android-embedded:4.3.0@aar") {
        // 库里捆绑了老版本 zxing core，强制用我们声明的 3.5.3
        isTransitive = false
    }

    // 消息存储（MsgStore）是纯 JVM 逻辑，可以直接用单元测试盯住——
    // org.json 必须显式引入：android.jar 里的 org.json 在单测中只会抛 "not mocked"。
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}
