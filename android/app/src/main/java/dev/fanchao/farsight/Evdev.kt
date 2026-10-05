package dev.fanchao.farsight

import android.view.KeyEvent

/**
 * Linux input codes, which the server injects (design §4), and how
 * Android's keys and characters map to them.
 */
object Evdev {
    const val KEY_ESC = 1
    const val KEY_BACKSPACE = 14
    const val KEY_TAB = 15
    const val KEY_ENTER = 28
    const val KEY_LEFTCTRL = 29
    const val KEY_LEFTSHIFT = 42
    const val KEY_LEFTALT = 56
    const val KEY_SPACE = 57
    const val KEY_F1 = 59
    const val KEY_HOME = 102
    const val KEY_UP = 103
    const val KEY_PAGEUP = 104
    const val KEY_LEFT = 105
    const val KEY_RIGHT = 106
    const val KEY_END = 107
    const val KEY_DOWN = 108
    const val KEY_PAGEDOWN = 109
    const val KEY_INSERT = 110
    const val KEY_DELETE = 111
    const val KEY_LEFTMETA = 125

    const val BTN_LEFT = 0x110
    const val BTN_RIGHT = 0x111
    const val BTN_MIDDLE = 0x112
    const val BTN_SIDE = 0x113
    const val BTN_EXTRA = 0x114

    /** F1 to F12. */
    fun function(n: Int): Int = if (n <= 10) KEY_F1 + n - 1 else 87 + n - 11

    private val letters = intArrayOf(
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,
    )

    private val keys: Map<Int, Int> = buildMap {
        for (i in 0 until 26) put(KeyEvent.KEYCODE_A + i, letters[i])
        put(KeyEvent.KEYCODE_0, 11)
        for (i in 1..9) put(KeyEvent.KEYCODE_0 + i, 1 + i)
        for (i in 1..12) put(KeyEvent.KEYCODE_F1 + i - 1, function(i))
        put(KeyEvent.KEYCODE_ESCAPE, KEY_ESC)
        put(KeyEvent.KEYCODE_TAB, KEY_TAB)
        put(KeyEvent.KEYCODE_ENTER, KEY_ENTER)
        put(KeyEvent.KEYCODE_DEL, KEY_BACKSPACE)
        put(KeyEvent.KEYCODE_FORWARD_DEL, KEY_DELETE)
        put(KeyEvent.KEYCODE_SPACE, KEY_SPACE)
        put(KeyEvent.KEYCODE_MINUS, 12)
        put(KeyEvent.KEYCODE_EQUALS, 13)
        put(KeyEvent.KEYCODE_LEFT_BRACKET, 26)
        put(KeyEvent.KEYCODE_RIGHT_BRACKET, 27)
        put(KeyEvent.KEYCODE_BACKSLASH, 43)
        put(KeyEvent.KEYCODE_SEMICOLON, 39)
        put(KeyEvent.KEYCODE_APOSTROPHE, 40)
        put(KeyEvent.KEYCODE_GRAVE, 41)
        put(KeyEvent.KEYCODE_COMMA, 51)
        put(KeyEvent.KEYCODE_PERIOD, 52)
        put(KeyEvent.KEYCODE_SLASH, 53)
        put(KeyEvent.KEYCODE_SHIFT_LEFT, KEY_LEFTSHIFT)
        put(KeyEvent.KEYCODE_SHIFT_RIGHT, 54)
        put(KeyEvent.KEYCODE_CTRL_LEFT, KEY_LEFTCTRL)
        put(KeyEvent.KEYCODE_CTRL_RIGHT, 97)
        put(KeyEvent.KEYCODE_ALT_LEFT, KEY_LEFTALT)
        put(KeyEvent.KEYCODE_ALT_RIGHT, 100)
        put(KeyEvent.KEYCODE_META_LEFT, KEY_LEFTMETA)
        put(KeyEvent.KEYCODE_META_RIGHT, 126)
        put(KeyEvent.KEYCODE_CAPS_LOCK, 58)
        put(KeyEvent.KEYCODE_DPAD_UP, KEY_UP)
        put(KeyEvent.KEYCODE_DPAD_DOWN, KEY_DOWN)
        put(KeyEvent.KEYCODE_DPAD_LEFT, KEY_LEFT)
        put(KeyEvent.KEYCODE_DPAD_RIGHT, KEY_RIGHT)
        put(KeyEvent.KEYCODE_MOVE_HOME, KEY_HOME)
        put(KeyEvent.KEYCODE_MOVE_END, KEY_END)
        put(KeyEvent.KEYCODE_PAGE_UP, KEY_PAGEUP)
        put(KeyEvent.KEYCODE_PAGE_DOWN, KEY_PAGEDOWN)
        put(KeyEvent.KEYCODE_INSERT, KEY_INSERT)
        put(KeyEvent.KEYCODE_SYSRQ, 99)
        put(KeyEvent.KEYCODE_SCROLL_LOCK, 70)
        put(KeyEvent.KEYCODE_BREAK, 119)
        put(KeyEvent.KEYCODE_MENU, 127)
        put(KeyEvent.KEYCODE_NUM_LOCK, 69)
        val numpad = intArrayOf(82, 79, 80, 81, 75, 76, 77, 71, 72, 73)
        for (i in 0..9) put(KeyEvent.KEYCODE_NUMPAD_0 + i, numpad[i])
        put(KeyEvent.KEYCODE_NUMPAD_DIVIDE, 98)
        put(KeyEvent.KEYCODE_NUMPAD_MULTIPLY, 55)
        put(KeyEvent.KEYCODE_NUMPAD_SUBTRACT, 74)
        put(KeyEvent.KEYCODE_NUMPAD_ADD, 78)
        put(KeyEvent.KEYCODE_NUMPAD_DOT, 83)
        put(KeyEvent.KEYCODE_NUMPAD_ENTER, 96)
    }

    /**
     * The evdev code for a key event. A physical keyboard's scan code is
     * already one (Android passes the kernel's through); a virtual key
     * (the soft keyboard's) goes by its key code.
     */
    fun fromKeyEvent(event: KeyEvent): Int? {
        val physical = event.device?.isVirtual == false && event.scanCode > 0
        return if (physical) event.scanCode else keys[event.keyCode]
    }

    /** The evdev code for an Android key code, if there is one. */
    fun fromKeyCode(keyCode: Int): Int? = keys[keyCode]

    /** Shifted characters on a US keyboard, by their unshifted key. */
    private const val SHIFTED = "~!@#$%^&*()_+{}|:\"<>?"
    private const val UNSHIFTED = "`1234567890-=[]\\;',./"

    /**
     * How to type a character on a US keyboard, the session's keymap:
     * its key, and whether it needs shift. Null for what has no key.
     */
    fun forChar(c: Char): Pair<Int, Boolean>? = when (c) {
        in 'a'..'z' -> letters[c - 'a'] to false
        in 'A'..'Z' -> letters[c - 'A'] to true
        '0' -> 11 to false
        in '1'..'9' -> (c - '0' + 1) to false
        ' ' -> KEY_SPACE to false
        '\n' -> KEY_ENTER to false
        '\t' -> KEY_TAB to false
        else -> {
            val shifted = SHIFTED.indexOf(c)
            val plain = UNSHIFTED.indexOf(c)
            val base = if (shifted >= 0) UNSHIFTED[shifted] else if (plain >= 0) c else null
            base?.let { b ->
                val code = when (b) {
                    '`' -> 41
                    '0' -> 11
                    in '1'..'9' -> b - '0' + 1
                    '-' -> 12
                    '=' -> 13
                    '[' -> 26
                    ']' -> 27
                    '\\' -> 43
                    ';' -> 39
                    '\'' -> 40
                    ',' -> 51
                    '.' -> 52
                    '/' -> 53
                    else -> return null
                }
                code to (shifted >= 0)
            }
        }
    }
}
