package dev.fanchao.farsight

import kotlin.math.max
import kotlin.math.min

/**
 * Where the remote desktop sits in the session view (design §7): remote
 * pixel (x, y) is drawn at view pixel (offsetX + x·zoom, offsetY + y·zoom).
 * Zooming and panning are the client's alone and never reach the server;
 * they just scale the last decoded frame on the GPU.
 *
 * "Fit" is computed against the whole view, and panning is limited to
 * the part the soft keyboard leaves visible: the keyboard shrinks what can
 * be seen, not the desktop.
 */
class Viewport {
    var remoteWidth = 0f
        private set
    var remoteHeight = 0f
        private set
    var viewWidth = 0f
        private set
    var viewHeight = 0f
        private set

    /** The view's height less the keyboard. */
    var visibleHeight = 0f
        private set

    var zoom = 1f
        private set
    var offsetX = 0f
        private set
    var offsetY = 0f
        private set

    /** Following "fit" as sizes change, until the user zooms. */
    var fitted = true
        private set

    fun fitZoom(): Float =
        if (remoteWidth <= 0f || remoteHeight <= 0f || viewWidth <= 0f) 1f
        else min(viewWidth / remoteWidth, viewHeight / remoteHeight)

    private fun minZoom() = min(fitZoom(), 1f)
    private fun maxZoom() = max(4f, fitZoom() * 4f)

    fun setRemote(width: Float, height: Float) {
        remoteWidth = width
        remoteHeight = height
        if (fitted) fit() else clamp()
    }

    fun setView(width: Float, height: Float, visible: Float = height) {
        viewWidth = width
        viewHeight = height
        visibleHeight = min(visible, height)
        if (fitted) fit() else clamp()
    }

    /** Shows the whole desktop. */
    fun fit() {
        zoom = fitZoom()
        fitted = true
        clamp()
    }

    /** One remote pixel to one screen pixel, around a point of the view. */
    fun oneToOne(focusX: Float, focusY: Float) {
        zoomBy(1f / zoom, focusX, focusY)
        fitted = zoom == fitZoom()
    }

    /** Between fit and 1:1, as a two-finger double tap does. */
    fun toggle(focusX: Float, focusY: Float) {
        if (fitted && fitZoom() != 1f) oneToOne(focusX, focusY) else fit()
    }

    /** Zooms by `factor`, keeping the remote point under the focus where it is. */
    fun zoomBy(factor: Float, focusX: Float, focusY: Float) {
        val rx = toRemoteX(focusX)
        val ry = toRemoteY(focusY)
        zoom = (zoom * factor).coerceIn(minZoom(), maxZoom())
        offsetX = focusX - rx * zoom
        offsetY = focusY - ry * zoom
        fitted = false
        clamp()
    }

    fun panBy(dx: Float, dy: Float) {
        offsetX += dx
        offsetY += dy
        clamp()
    }

    /** Whether there is anything to pan to. */
    fun canPan(): Boolean = remoteWidth * zoom > viewWidth + 0.5f || remoteHeight * zoom > visibleHeight + 0.5f

    /** Pans so the remote point is at least `margin` view pixels inside the visible part. */
    fun follow(remoteX: Float, remoteY: Float, margin: Float) {
        val x = toViewX(remoteX)
        val y = toViewY(remoteY)
        val mx = min(margin, viewWidth / 4)
        val my = min(margin, visibleHeight / 4)
        when {
            x < mx -> offsetX += mx - x
            x > viewWidth - mx -> offsetX -= x - (viewWidth - mx)
        }
        when {
            y < my -> offsetY += my - y
            y > visibleHeight - my -> offsetY -= y - (visibleHeight - my)
        }
        clamp()
    }

    fun toRemoteX(viewX: Float) = (viewX - offsetX) / zoom
    fun toRemoteY(viewY: Float) = (viewY - offsetY) / zoom
    fun toViewX(remoteX: Float) = offsetX + remoteX * zoom
    fun toViewY(remoteY: Float) = offsetY + remoteY * zoom

    /** Keeps the desktop on screen: centred when it fits, edge to edge when it doesn't. */
    private fun clamp() {
        val w = remoteWidth * zoom
        val h = remoteHeight * zoom
        offsetX = if (w <= viewWidth) (viewWidth - w) / 2 else offsetX.coerceIn(viewWidth - w, 0f)
        offsetY = if (h <= visibleHeight) (visibleHeight - h) / 2 else offsetY.coerceIn(visibleHeight - h, 0f)
    }
}
