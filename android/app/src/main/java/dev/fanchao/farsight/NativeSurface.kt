package dev.fanchao.farsight

import android.view.Surface

/**
 * Hands the session view's surface to the Rust side, which decodes video
 * straight into it. uniffi can't pass a [Surface], so this is plain JNI.
 */
object NativeSurface {
    init {
        System.loadLibrary("farsight_android")
    }

    /** The surface to draw on, or null once it is destroyed (this waits until it is let go of). */
    @JvmStatic external fun set(surface: Surface?)
}
