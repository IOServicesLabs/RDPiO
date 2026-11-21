package io.ioservices.rdpio

/**
 * The keyboard shortcuts as the app tells them: the connect screen's list and
 * the reminder when a stream starts. One place, so the two can't drift apart
 * (the keys themselves are handled in StreamActivity.handleKey).
 */
object Hotkeys {
    /** The full list, for the connect screen. */
    const val ALL =
        "Keyboard: Ctrl+Shift+M locks the mouse (games), Ctrl+Shift+O game/office, Ctrl+Shift+S stats, " +
            "Ctrl+Shift+Tab switch session, Ctrl+Shift+Q disconnect, Shift+Esc the menu, Ctrl+Alt+End = Ctrl+Alt+Del."

    /** The short reminder shown over a starting stream. */
    const val REMINDER =
        "Ctrl+Shift+M locks the mouse · Ctrl+Shift+Tab switches session · Ctrl+Shift+Q disconnects · " +
            "the round button or Shift+Esc opens the menu (drag it anywhere)"
}
