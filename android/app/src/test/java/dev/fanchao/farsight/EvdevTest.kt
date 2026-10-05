package dev.fanchao.farsight

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class EvdevTest {
    @Test
    fun charactersTypeOnAUsKeyboard() {
        assertEquals(30 to false, Evdev.forChar('a'))
        assertEquals(30 to true, Evdev.forChar('A'))
        assertEquals(11 to false, Evdev.forChar('0'))
        assertEquals(2 to true, Evdev.forChar('!'))
        assertEquals(53 to true, Evdev.forChar('?'))
        assertEquals(40 to true, Evdev.forChar('"'))
        assertEquals(Evdev.KEY_ENTER to false, Evdev.forChar('\n'))
        assertNull(Evdev.forChar('é'))
    }

    @Test
    fun functionKeys() {
        assertEquals(59, Evdev.function(1))
        assertEquals(68, Evdev.function(10))
        assertEquals(87, Evdev.function(11))
        assertEquals(88, Evdev.function(12))
    }
}
