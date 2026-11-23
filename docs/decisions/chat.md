# Chat decisions (ADR-610 series)

Same format as `docs/DECISIONS.md`: context, decision, consequences.

### ADR-610: Ctrl+C in the chat log

* Context: a user reported that Ctrl+C does not copy text selected in the chat log
  (`crates/client/src/ui/selectable.rs`, one paragraph per message, copy in the
  widget's `KeyPressed` branch through the widget `Clipboard`).
* Investigation: no defect was found, so the root cause is not known. Each suspected
  cause was checked on Windows 11 and ruled out:
  * Widget state across view rebuilds: the room view is rebuilt on every backend
    event. The real `App::view` (room with chat history, members panel, bottom bar)
    was driven headlessly through `iced_runtime::UserInterface` with a tiny-skia
    renderer and a recording clipboard, rebuilding the view before every event. The
    message state (tag, `sync`, selection) survives; drag and triple-click selections
    followed by Ctrl+C put the text on the clipboard, also when the chat input had
    focus first.
  * Events reaching the widget: `Column`/`Row` forward every event to every child,
    `Scrollable` forwards keyboard events to its content, and a press outside a
    message (levitated or translated cursor) clears only that message's selection.
  * Key and modifiers from winit: a debug build hosting the same widget in a
    scrollable, with a 10 Hz tick, received `KeyPressed { key: Character("c"),
    modifiers: CTRL }` after `ModifiersChanged(CTRL)` and `KeyPressed(Control)` from
    real Windows key messages. The full app built without a backend received the
    same events.
  * The OS clipboard: in that build the widget's write went through iced_winit and
    `window_clipboard`/`clipboard-win` and `Get-Clipboard` returned the selection.
    The widget clipboard writes synchronously; no `iced::clipboard::write` task is
    needed.
* Decision: keep the copy logic. Add headless regression tests in `selectable.rs`
  that run the chat panel's widget shape (messages in a scrollable above a text
  input) through `UserInterface`, rebuilding the view before every event, and assert
  that Ctrl+C after a drag or a triple-click writes exactly the selected text.
* Consequences: the copy path is now under test. If the report recurs, check that the
  build includes selectable text (commit 74d91a1 or later), whether Ctrl+C works in
  the chat input (same clipboard path), and whether a keyboard remapper, remote
  desktop or clipboard tool sits in between. Mouse-driven checks were not repeated
  in the full client because posted mouse messages race `WM_MOUSELEAVE` when the
  real pointer is outside the window.

### ADR-611: Link detection in chat messages

* Context: users paste web links into chat and want to open them.
* Decision: `ui::links::find` returns the byte ranges of links in a message body
  (never the display name). A link starts at `http://` or `https://` (lowercase)
  that is not preceded by a letter or digit, and runs to the next whitespace.
  Trailing `.`, `,`, `;`, `:`, `!`, `?`, `"` and `'` are dropped, as are a trailing
  `)` or `]` with no matching opener inside the link, repeatedly, so
  `(see https://a.example/x).` links `https://a.example/x` while
  `https://en.wikipedia.org/wiki/Rust_(programming_language)` keeps its `)`. A bare
  scheme is not a link. No `www.` or scheme-less detection.
* Consequences: link characters are drawn in the theme's primary colour (baked into
  the per-character spans; the view passes the colour because `layout` has no
  theme) and underlined with one-pixel quads the way iced's rich text does. Links
  keep working with selection: the existing word rule already selects a whole URL
  on double-click.

### ADR-612: Opening links

* Context: a click on a link should open it in the default browser without
  breaking selection, and the widget should not run side effects.
* Decision:
  * The selectable widget takes `on_link: fn(String) -> Message` and publishes
    `Message::OpenLink(url)`. The hover cursor over a link is `Pointer`.
  * A link opens after a single press and release on the same link with no
    selection dragged in between. Because a double-click starts with that same
    click, the open waits for iced's 300 ms double-click window (a redraw is
    requested for that time); a double-click within it selects the URL instead and
    nothing opens. Dragging from a link selects text as before.
  * `App::update` calls `ui::links::open`, which refuses anything that is not
    `http://` or `https://` or that contains whitespace or control characters, then
    spawns without a shell: `rundll32 url.dll,FileProtocolHandler <url>` on Windows
    (the URL is passed raw, so `&` and quotes are never interpreted by cmd.exe),
    `open` on macOS, `xdg-open` elsewhere. The child is waited on from a short-lived
    thread so no zombie is left; `update` never blocks. A spawn failure becomes the
    status-line notice. No new crate.
* Consequences: a link opens about 300 ms after the click. The opener only knows
  that the helper started; a browser that fails later is not reported.
