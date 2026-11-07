package io.github.st3inberg.tabextend

import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import android.view.View

/**
 * The laptop's pointer, drawn over the video. GNOME sends it as a position
 * instead of drawing it into a virtual monitor's picture. The video fills
 * this view exactly (the stream is sized to the screen), so a fraction of
 * the stream is the same fraction of the view.
 *
 * Only the position comes through, not the pointer's shape, so this is
 * always an arrow.
 */
class CursorOverlay(context: Context) : View(context) {
    @Volatile private var visibleOnStream = false
    @Volatile private var fx = 0f
    @Volatile private var fy = 0f

    private val size = 22f * resources.displayMetrics.density
    private val arrow = Path().apply {
        // Tip at (0, 0), in units of `size`.
        moveTo(0f, 0f)
        lineTo(0f, 1f)
        lineTo(0.27f, 0.76f)
        lineTo(0.45f, 1.12f)
        lineTo(0.6f, 1.05f)
        lineTo(0.42f, 0.7f)
        lineTo(0.75f, 0.7f)
        close()
    }
    private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.WHITE
        style = Paint.Style.FILL
    }
    private val outline = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        color = Color.BLACK
        style = Paint.Style.STROKE
        strokeJoin = Paint.Join.ROUND
    }

    init {
        // Never take touches: they belong to the video surface underneath.
        isClickable = false
        isFocusable = false
    }

    /** Safe to call from any thread. */
    fun update(visible: Boolean, x: Float, y: Float) {
        visibleOnStream = visible
        fx = x.coerceIn(0f, 1f)
        fy = y.coerceIn(0f, 1f)
        postInvalidateOnAnimation()
    }

    fun hide() = update(false, 0f, 0f)

    override fun onDraw(canvas: Canvas) {
        if (!visibleOnStream) return
        canvas.save()
        canvas.translate(fx * width, fy * height)
        canvas.scale(size, size)
        outline.strokeWidth = 1.5f / size * resources.displayMetrics.density
        canvas.drawPath(arrow, fill)
        canvas.drawPath(arrow, outline)
        canvas.restore()
    }
}
