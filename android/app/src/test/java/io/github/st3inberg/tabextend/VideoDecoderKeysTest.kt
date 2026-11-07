package io.github.st3inberg.tabextend

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class VideoDecoderKeysTest {
    @Test
    fun qualcommGetsPictureOrderAndLowLatency() {
        assertEquals(
            listOf("vendor.qti-ext-dec-picture-order.enable", "vendor.qti-ext-dec-low-latency.enable"),
            configureKeys("c2.qti.avc.decoder"),
        )
        assertEquals(configureKeys("c2.qti.avc.decoder"), configureKeys("OMX.qcom.video.decoder.avc"))
    }

    @Test
    fun exynosGetsItsOwnSwitch() {
        assertEquals(listOf("vendor.rtc-ext-dec-low-latency.enable"), configureKeys("c2.exynos.h264.decoder"))
    }

    @Test
    fun otherDecodersGetNothing() {
        assertTrue(configureKeys("c2.mtk.avc.decoder").isEmpty())
        assertTrue(configureKeys("c2.android.avc.decoder").isEmpty())
    }
}
