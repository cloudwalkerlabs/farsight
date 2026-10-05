package dev.fanchao.farsight

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ViewportTest {
    private fun viewport(remote: Pair<Float, Float>, view: Pair<Float, Float>) = Viewport().apply {
        setView(view.first, view.second)
        setRemote(remote.first, remote.second)
    }

    @Test
    fun fitCentresTheDesktop() {
        val v = viewport(1920f to 1080f, 960f to 1080f)
        assertEquals(0.5f, v.zoom, 1e-6f)
        assertEquals(0f, v.offsetX, 1e-3f)
        assertEquals((1080f - 540f) / 2, v.offsetY, 1e-3f)
        assertFalse(v.canPan())
    }

    @Test
    fun zoomKeepsTheFocusStill() {
        val v = viewport(1000f to 1000f, 1000f to 1000f)
        val before = v.toRemoteX(300f) to v.toRemoteY(400f)
        v.zoomBy(2f, 300f, 400f)
        assertEquals(2f, v.zoom, 1e-6f)
        assertEquals(before.first, v.toRemoteX(300f), 1e-3f)
        assertEquals(before.second, v.toRemoteY(400f), 1e-3f)
        assertTrue(v.canPan())
        // Panning stops at the desktop's edge.
        v.panBy(10_000f, 10_000f)
        assertEquals(0f, v.offsetX, 1e-3f)
        assertEquals(0f, v.offsetY, 1e-3f)
    }

    @Test
    fun theKeyboardShrinksWhatIsSeenNotTheDesktop() {
        val v = viewport(1080f to 2400f, 1080f to 2400f)
        assertEquals(1f, v.zoom, 1e-6f)
        v.setView(1080f, 2400f, visible = 1400f)
        assertEquals(1f, v.zoom, 1e-6f)
        // A cursor near the bottom is brought above the keyboard.
        v.follow(500f, 2300f, margin = 50f)
        assertTrue(v.toViewY(2300f) <= 1400f - 50f + 1e-3f)
    }

    @Test
    fun toggleSwitchesBetweenFitAndOneToOne() {
        val v = viewport(2000f to 1000f, 1000f to 1000f)
        assertEquals(0.5f, v.zoom, 1e-6f)
        v.toggle(500f, 500f)
        assertEquals(1f, v.zoom, 1e-6f)
        v.toggle(500f, 500f)
        assertEquals(0.5f, v.zoom, 1e-6f)
    }
}
