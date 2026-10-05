package dev.fanchao.farsight

import android.media.MediaCodecInfo
import android.media.MediaCodecList
import uniffi.farsight_android.VideoCodec
import uniffi.farsight_android.VideoDecoder

/**
 * The video decoders worth offering the server: for each codec, the best
 * this device has, hardware first and low latency first (design §3).
 * 4:2:0 at 8 bits only: that is what phones decode in hardware.
 */
object Decoders {
    private val codecs = listOf(
        VideoCodec.H264 to "video/avc",
        VideoCodec.HEVC to "video/hevc",
        VideoCodec.AV1 to "video/av01",
    )

    fun available(): List<VideoDecoder> {
        val infos = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter { !it.isEncoder }
        return codecs.mapNotNull { (codec, mime) ->
            infos.filter { info -> info.supportedTypes.any { it.equals(mime, ignoreCase = true) } }
                .maxByOrNull { info ->
                    val caps = info.getCapabilitiesForType(mime)
                    val lowLatency = caps.isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
                    (if (info.isHardwareAccelerated) 2 else 0) + (if (lowLatency) 1 else 0)
                }
                ?.let { info ->
                    val video = info.getCapabilitiesForType(mime).videoCapabilities ?: return@mapNotNull null
                    VideoDecoder(
                        codec = codec,
                        name = info.name,
                        maxWidth = video.supportedWidths.upper.toUInt(),
                        maxHeight = video.supportedHeights.upper.toUInt(),
                        hardware = info.isHardwareAccelerated,
                    )
                }
        }
    }
}
