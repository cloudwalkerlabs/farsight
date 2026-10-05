import javax.inject.Inject

plugins {
    alias(libs.plugins.androidApplication)
    alias(libs.plugins.composeCompiler)
}

/** ABIs to build; `-Pfarsight.abis=arm64-v8a` builds just one. */
val rustAbis: List<String> =
    (findProperty("farsight.abis") as String? ?: "arm64-v8a,x86_64").split(',')

android {
    namespace = "dev.fanchao.farsight"
    compileSdk = 37

    defaultConfig {
        applicationId = "dev.fanchao.farsight"
        minSdk = 30
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"
        // Also keeps JNA's other ABIs out of the APK.
        ndk { abiFilters += rustAbis }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.activity.compose)
    implementation(libs.lifecycle.viewmodel.compose)
    implementation(libs.lifecycle.runtime.compose)
    // uniffi's Kotlin bindings call into Rust through JNA.
    implementation("${libs.jna.get()}@aar")
}

// --- Rust: farsight-android built with cargo-ndk, plus its uniffi bindings ---

val rustRoot: File = rootDir.parentFile

abstract class CargoNdkTask : DefaultTask() {
    @get:Inject abstract val exec: ExecOperations
    @get:Internal abstract val cargoRoot: DirectoryProperty
    @get:InputFiles abstract val sources: ConfigurableFileCollection
    @get:Input abstract val abis: ListProperty<String>
    @get:Input abstract val sdkDir: Property<String>
    @get:OutputDirectory abstract val outputDir: DirectoryProperty

    @TaskAction
    fun build() {
        val out = outputDir.get().asFile
        out.deleteRecursively()
        exec.exec {
            workingDir = cargoRoot.get().asFile
            environment("ANDROID_HOME", sdkDir.get())
            // Always optimised: debug builds are far too slow for video.
            commandLine(
                listOf("cargo", "ndk") +
                    abis.get().flatMap { listOf("-t", it) } +
                    listOf("-o", out.path, "build", "--release", "-p", "farsight-android"),
            )
        }
    }
}

abstract class UniffiBindgenTask : DefaultTask() {
    @get:Inject abstract val exec: ExecOperations
    @get:Internal abstract val cargoRoot: DirectoryProperty
    @get:InputDirectory abstract val libs: DirectoryProperty
    @get:Input abstract val abi: Property<String>
    @get:OutputDirectory abstract val outputDir: DirectoryProperty

    @TaskAction
    fun generate() {
        val out = outputDir.get().asFile
        out.deleteRecursively()
        val lib = libs.get().dir(abi.get()).file("libfarsight_android.so").asFile
        exec.exec {
            workingDir = cargoRoot.get().asFile
            commandLine(
                "cargo", "run", "-q", "-p", "farsight-uniffi-bindgen", "--",
                "generate", "--library", lib.path, "--language", "kotlin",
                "--no-format", "--out-dir", out.path,
            )
        }
    }
}

val cargoNdk = tasks.register<CargoNdkTask>("cargoNdk") {
    cargoRoot.set(rustRoot)
    sources.from(
        fileTree(rustRoot.resolve("crates")) { exclude("**/target/**") },
        rustRoot.resolve("Cargo.toml"),
        rustRoot.resolve("Cargo.lock"),
    )
    abis.set(rustAbis)
    sdkDir.set(androidComponents.sdkComponents.sdkDirectory.map { it.asFile.path })
    outputDir.set(layout.buildDirectory.dir("rust/jniLibs"))
}

val uniffiBindgen = tasks.register<UniffiBindgenTask>("uniffiBindgen") {
    cargoRoot.set(rustRoot)
    libs.set(cargoNdk.flatMap { it.outputDir })
    abi.set(rustAbis.first())
    outputDir.set(layout.buildDirectory.dir("generated/uniffi"))
}

androidComponents {
    onVariants { variant ->
        variant.sources.jniLibs?.addGeneratedSourceDirectory(cargoNdk, CargoNdkTask::outputDir)
        variant.sources.kotlin?.addGeneratedSourceDirectory(
            uniffiBindgen,
            UniffiBindgenTask::outputDir,
        )
    }
}
