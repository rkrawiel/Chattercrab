//! Selectable chat messages.
//!
//! iced's `text` widget cannot be highlighted. Each message is one paragraph — the
//! display name, then the body — laid out as one span per character so a click can
//! resolve to a character index. The chat log stays in the normal scrollable; this
//! widget does not take the mouse wheel. Web links in the body are coloured and
//! underlined; a plain click on one publishes its URL, dragging still selects.

use std::ops::Range;

use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::text as text_widget;
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Clipboard, Shell, clipboard, layout, mouse, renderer, widget};
use iced::alignment;
use iced::keyboard;
use iced::time::{Duration, Instant};
use iced::window;
use iced::{Color, Element, Event, Length, Pixels, Point, Rectangle, Size, Theme, Vector};

use super::links;

const NAME_SIZE: f32 = 12.0;
/// iced's double-click window. A clicked link opens only once it has passed, so a
/// double-click selects the link instead of opening it.
const DOUBLE_CLICK: Duration = Duration::from_millis(300);

/// A chat message the user can highlight and copy. A click on a link in `body`
/// publishes `on_link` with its URL.
pub(super) fn message<'a, Message: 'a>(
    id: u64,
    name: &'a str,
    body: &'a str,
    link_color: Color,
    on_link: fn(String) -> Message,
) -> Element<'a, Message> {
    SelectableMessage {
        id,
        name,
        body,
        link_color,
        on_link,
    }
    .into()
}

struct SelectableMessage<'a, Message> {
    id: u64,
    name: &'a str,
    body: &'a str,
    link_color: Color,
    on_link: fn(String) -> Message,
}

struct State<P> {
    built: bool,
    id: u64,
    name: String,
    body: String,
    /// `name`, a newline, then `body`. Character indexes in the selection refer to this.
    text: String,
    chars: usize,
    /// Links in `body` as character ranges in `text`, with their URL.
    links: Vec<(Range<usize>, String)>,
    link_color: Color,
    paragraph: P,
    anchor: usize,
    cursor: usize,
    focused: bool,
    /// A single click is being dragged into a selection. Double- and triple-clicks are not.
    single_drag: bool,
    modifiers: keyboard::Modifiers,
    last_click: Option<mouse::Click>,
    /// The link under a single click that has not become a selection, and when it
    /// may open.
    pressed_link: Option<(usize, Instant)>,
    /// A clicked link's URL and when it opens unless a double-click comes first.
    pending_link: Option<(String, Instant)>,
}

impl<'a, Message, R> widget::Widget<Message, Theme, R> for SelectableMessage<'a, Message>
where
    R: text::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<R::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            built: false,
            id: 0,
            name: String::new(),
            body: String::new(),
            text: String::new(),
            chars: 0,
            links: Vec::new(),
            link_color: Color::TRANSPARENT,
            paragraph: R::Paragraph::default(),
            anchor: 0,
            cursor: 0,
            focused: false,
            single_drag: false,
            modifiers: keyboard::Modifiers::empty(),
            last_click: None,
            pressed_link: None,
            pending_link: None,
        })
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Shrink,
        }
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        layout::sized(limits, Length::Fill, Length::Shrink, |limits| {
            let state = tree.state.downcast_mut::<State<R::Paragraph>>();
            sync(
                state,
                self,
                limits.max(),
                renderer.default_size(),
                renderer.default_font(),
            );
            state.paragraph.min_bounds()
        })
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut R,
        theme: &Theme,
        defaults: &renderer::Style,
        layout: layout::Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        if !layout.bounds().intersects(viewport) {
            return;
        }
        let state = tree.state.downcast_ref::<State<R::Paragraph>>();
        let translation = layout.position() - Point::ORIGIN;
        let (start, end) = selection_range(state.anchor, state.cursor);
        if state.focused && start < end {
            let highlight = theme.extended_palette().primary.weak.color;
            for index in start..end {
                for region in state.paragraph.span_bounds(index) {
                    let bounds = region + translation;
                    if bounds.intersects(viewport) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                ..renderer::Quad::default()
                            },
                            highlight,
                        );
                    }
                }
            }
        }
        // Underlined like iced's rich text: one pixel just under the baseline.
        let size = state.paragraph.size();
        let line_height = state.paragraph.line_height().to_absolute(size);
        let underline = Vector::new(0.0, size.0 + (line_height.0 - size.0) / 2.0 - size.0 * 0.08);
        for (range, _) in &state.links {
            for index in range.clone() {
                for region in state.paragraph.span_bounds(index) {
                    let bounds = Rectangle::new(
                        region.position() + translation + underline,
                        Size::new(region.width, 1.0),
                    );
                    if bounds.intersects(viewport) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                ..renderer::Quad::default()
                            },
                            state.link_color,
                        );
                    }
                }
            }
        }
        text_widget::draw(
            renderer,
            defaults,
            layout.bounds(),
            &state.paragraph,
            text_widget::Style { color: None },
            viewport,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State<R::Paragraph>>();
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let bounds = layout.bounds();
                let Some(local) = cursor.position_in(bounds) else {
                    if state.focused {
                        clear_selection(state);
                        shell.request_redraw();
                    }
                    return;
                };
                let click = mouse::Click::new(local, mouse::Button::Left, state.last_click);
                let extend = state.focused && state.modifiers.shift();
                state.last_click = Some(click);
                state.focused = true;
                match click.kind() {
                    mouse::click::Kind::Single => {
                        let index = cursor_index(&state.paragraph, state.chars, local);
                        if extend {
                            state.cursor = index;
                        } else {
                            state.anchor = index;
                            state.cursor = index;
                        }
                        state.single_drag = true;
                        state.pressed_link = if extend {
                            None
                        } else {
                            link_under(state, local)
                                .map(|link| (link, Instant::now() + DOUBLE_CLICK))
                        };
                    }
                    mouse::click::Kind::Double => {
                        let index = char_under(&state.paragraph, state.chars, local)
                            .unwrap_or_else(|| state.cursor.min(state.chars.saturating_sub(1)));
                        let (start, end) = word_range(&state.text, index);
                        state.anchor = start;
                        state.cursor = end;
                        state.single_drag = false;
                        state.pressed_link = None;
                        state.pending_link = None;
                    }
                    mouse::click::Kind::Triple => {
                        state.anchor = 0;
                        state.cursor = state.chars;
                        state.single_drag = false;
                        state.pressed_link = None;
                    }
                }
                shell.request_redraw();
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.single_drag => {
                let Some(local) = local_point(layout.bounds(), cursor) else {
                    return;
                };
                let index = cursor_index(&state.paragraph, state.chars, local);
                if index != state.cursor {
                    state.cursor = index;
                    shell.request_redraw();
                }
                if state.anchor != state.cursor {
                    state.pressed_link = None;
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.single_drag = false;
                let released_on = cursor
                    .position_in(layout.bounds())
                    .and_then(|local| link_under(state, local));
                if let Some((link, opens_at)) = state.pressed_link.take()
                    && state.anchor == state.cursor
                    && released_on == Some(link)
                    && let Some((_, url)) = state.links.get(link)
                {
                    state.pending_link = Some((url.clone(), opens_at));
                    shell.request_redraw_at(opens_at);
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => match state.pending_link.take() {
                Some((url, opens_at)) if *now >= opens_at => {
                    shell.publish((self.on_link)(url));
                }
                pending => {
                    if let Some((_, opens_at)) = &pending {
                        shell.request_redraw_at(*opens_at);
                    }
                    state.pending_link = pending;
                }
            },
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                ..
            }) if state.focused => {
                let Some(letter) = key.to_latin(*physical_key) else {
                    return;
                };
                if !modifiers.command() {
                    return;
                }
                match letter.to_ascii_lowercase() {
                    'c' => {
                        let selected = char_slice(&state.text, state.anchor, state.cursor);
                        if !selected.is_empty() {
                            clipboard.write(clipboard::Kind::Standard, selected);
                            shell.capture_event();
                        }
                    }
                    'a' => {
                        state.anchor = 0;
                        state.cursor = state.chars;
                        shell.request_redraw();
                        shell.capture_event();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &R,
    ) -> mouse::Interaction {
        let Some(local) = cursor.position_in(layout.bounds()) else {
            return mouse::Interaction::default();
        };
        let state = tree.state.downcast_ref::<State<R::Paragraph>>();
        let selecting = state.single_drag && state.anchor != state.cursor;
        if !selecting && link_under(state, local).is_some() {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::Text
        }
    }
}

impl<'a, Message: 'a> From<SelectableMessage<'a, Message>> for Element<'a, Message> {
    fn from(message: SelectableMessage<'a, Message>) -> Self {
        Self::new(message)
    }
}

fn sync<P, Font, Message>(
    state: &mut State<P>,
    message: &SelectableMessage<'_, Message>,
    bounds: Size,
    size: Pixels,
    font: Font,
) where
    P: text::Paragraph<Font = Font>,
    Font: Copy + PartialEq,
{
    let SelectableMessage {
        id,
        name,
        body,
        link_color,
        ..
    } = *message;
    let changed = !state.built || state.id != id || state.name != name || state.body != body;
    if changed {
        state.built = true;
        state.id = id;
        state.name.clear();
        state.name.push_str(name);
        state.body.clear();
        state.body.push_str(body);
        state.text = transcript(name, body);
        state.links = body_links(name, body);
        state.link_color = link_color;
        state.pending_link = None;
        clear_selection(state);
        rebuild(state, bounds, size, font);
        return;
    }
    if state.link_color != link_color {
        state.link_color = link_color;
        rebuild(state, bounds, size, font);
        return;
    }
    match state.paragraph.compare(text::Text {
        content: (),
        bounds,
        size,
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::WordOrGlyph,
    }) {
        text::Difference::None => {}
        text::Difference::Bounds => state.paragraph.resize(bounds),
        text::Difference::Shape => rebuild(state, bounds, size, font),
    }
}

fn rebuild<P, Font>(state: &mut State<P>, bounds: Size, size: Pixels, font: Font)
where
    P: text::Paragraph<Font = Font>,
    Font: Copy,
{
    let name_chars = state.name.chars().count();
    let spans: Vec<_> = state
        .text
        .chars()
        .enumerate()
        .map(|(index, ch)| {
            let span = text::Span::<(), Font>::new(ch.to_string());
            if index < name_chars {
                span.size(NAME_SIZE)
            } else if link_at(&state.links, index).is_some() {
                span.color(state.link_color)
            } else {
                span
            }
        })
        .collect();
    state.chars = spans.len();
    state.paragraph = P::with_spans(text::Text {
        content: spans.as_slice(),
        bounds,
        size,
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::WordOrGlyph,
    });
}

fn clear_selection<P>(state: &mut State<P>) {
    state.anchor = 0;
    state.cursor = 0;
    state.focused = false;
    state.single_drag = false;
    state.pressed_link = None;
}

fn selection_range(anchor: usize, cursor: usize) -> (usize, usize) {
    if anchor <= cursor {
        (anchor, cursor)
    } else {
        (cursor, anchor)
    }
}

/// Name and body as they are shown and copied, with the name on its own line.
fn transcript(name: &str, body: &str) -> String {
    let mut text = String::with_capacity(name.len() + 1 + body.len());
    text.push_str(name);
    text.push('\n');
    text.push_str(body);
    text
}

/// The links in `body` as character ranges in [`transcript`]`(name, body)`.
fn body_links(name: &str, body: &str) -> Vec<(Range<usize>, String)> {
    let offset = name.chars().count() + 1;
    links::find(body)
        .into_iter()
        .map(|range| {
            let start = offset + body[..range.start].chars().count();
            let url = &body[range];
            (start..start + url.chars().count(), url.to_owned())
        })
        .collect()
}

/// Which link, if any, holds the character at `index`.
fn link_at(links: &[(Range<usize>, String)], index: usize) -> Option<usize> {
    links.iter().position(|(range, _)| range.contains(&index))
}

/// Which link, if any, is under the widget-local `point`.
fn link_under<P: text::Paragraph>(state: &State<P>, point: Point) -> Option<usize> {
    char_under(&state.paragraph, state.chars, point).and_then(|index| link_at(&state.links, index))
}

fn char_slice(text: &str, anchor: usize, cursor: usize) -> String {
    let (start, end) = selection_range(anchor, cursor);
    text.chars().skip(start).take(end - start).collect()
}

/// The non-whitespace run around `char_index`, or that one character when it is whitespace.
fn word_range(text: &str, char_index: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    let index = char_index.min(chars.len() - 1);
    if chars[index].is_whitespace() {
        return (index, index + 1);
    }
    let mut start = index;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = index + 1;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    (start, end)
}

fn local_point(bounds: Rectangle, cursor: mouse::Cursor) -> Option<Point> {
    if let Some(point) = cursor.position_in(bounds) {
        return Some(point);
    }
    let point = cursor.position()?;
    if point.y < bounds.y {
        return Some(Point::new(0.0, -1.0));
    }
    if point.y > bounds.y + bounds.height {
        return Some(Point::new(0.0, bounds.height + 1.0));
    }
    Some(Point::new(
        (point.x - bounds.x).clamp(0.0, bounds.width.max(0.0)),
        point.y - bounds.y,
    ))
}

fn char_under<P: text::Paragraph>(paragraph: &P, chars: usize, point: Point) -> Option<usize> {
    paragraph.hit_span(point).filter(|index| *index < chars)
}

fn cursor_index<P: text::Paragraph>(paragraph: &P, chars: usize, point: Point) -> usize {
    if chars == 0 || point.y < 0.0 {
        return 0;
    }
    if point.y >= paragraph.min_height() {
        return chars;
    }
    if let Some(index) = paragraph.hit_span(point) {
        return gap_after(paragraph, chars, index, point);
    }
    let mut best: Option<(usize, f32)> = None;
    for index in 0..chars {
        for region in paragraph.span_bounds(index) {
            let dx = region.center().x - point.x;
            let dy = region.center().y - point.y;
            let distance = dx * dx + dy * dy;
            if best.is_none_or(|(_, best_distance)| distance < best_distance) {
                best = Some((index, distance));
            }
        }
    }
    best.map(|(index, _)| gap_after(paragraph, chars, index, point))
        .unwrap_or(chars)
}

fn gap_after<P: text::Paragraph>(paragraph: &P, chars: usize, index: usize, point: Point) -> usize {
    let regions = paragraph.span_bounds(index);
    let on_right = regions
        .iter()
        .min_by(|left, right| {
            (left.center().y - point.y)
                .abs()
                .total_cmp(&(right.center().y - point.y).abs())
        })
        .is_some_and(|region| point.x >= region.center().x);
    if on_right {
        (index + 1).min(chars)
    } else {
        index.min(chars)
    }
}

#[cfg(test)]
mod tests {
    use iced::advanced::clipboard::{self, Clipboard};
    use iced::advanced::renderer::Headless;
    use iced::keyboard::{self, key};
    use iced::time::{Duration, Instant};
    use iced::widget::{Column, column, scrollable, text_input};
    use iced::{Color, Element, Event, Font, Length, Pixels, Point, Size, mouse, window};
    use iced_runtime::user_interface::{self, UserInterface};

    use super::{body_links, char_slice, message, transcript, word_range};

    const WINDOW: Size = Size::new(800.0, 600.0);
    /// On the body line of the first message, below the 12 px name line.
    const BODY_Y: f32 = 31.0;
    const INPUT: Point = Point::new(400.0, 590.0);
    const LINK: &str = "https://a.example/x";

    #[derive(Debug, Clone, PartialEq)]
    enum Msg {
        Link(String),
        Input(String),
    }

    #[derive(Default)]
    struct Recorded(Vec<String>);

    impl Clipboard for Recorded {
        fn read(&self, _kind: clipboard::Kind) -> Option<String> {
            self.0.last().cloned()
        }

        fn write(&mut self, _kind: clipboard::Kind, contents: String) {
            self.0.push(contents);
        }
    }

    /// The chat panel's shape: messages in a scrollable above a text input, driven
    /// headlessly with a recording clipboard.
    struct Chat {
        body: &'static str,
        renderer: iced::Renderer,
        cache: user_interface::Cache,
        clipboard: Recorded,
        messages: Vec<Msg>,
    }

    impl Chat {
        fn new(body: &'static str) -> Self {
            let renderer = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(<iced::Renderer as Headless>::new(
                    Font::DEFAULT,
                    Pixels(16.0),
                    Some("tiny-skia"),
                ))
                .unwrap();
            Self {
                body,
                renderer,
                cache: user_interface::Cache::default(),
                clipboard: Recorded::default(),
                messages: Vec::new(),
            }
        }

        /// Feeds one event with the cursor at `at`. The view is rebuilt first, as the
        /// room view is on every backend event.
        fn send(&mut self, at: Point, event: Event) -> mouse::Interaction {
            let view: Element<'_, Msg> = column![
                scrollable(Column::new().push(message(
                    1,
                    "Ada",
                    self.body,
                    Color::from_rgb(0.0, 0.0, 1.0),
                    Msg::Link,
                )))
                .height(Length::Fill),
                text_input("", "").on_input(Msg::Input),
            ]
            .into();
            let mut ui = UserInterface::build(
                view,
                WINDOW,
                std::mem::take(&mut self.cache),
                &mut self.renderer,
            );
            let (state, _) = ui.update(
                &[event],
                mouse::Cursor::Available(at),
                &mut self.renderer,
                &mut self.clipboard,
                &mut self.messages,
            );
            self.cache = ui.into_cache();
            match state {
                user_interface::State::Updated {
                    mouse_interaction, ..
                } => mouse_interaction,
                user_interface::State::Outdated => mouse::Interaction::None,
            }
        }

        fn move_to(&mut self, at: Point) -> mouse::Interaction {
            self.send(at, Event::Mouse(mouse::Event::CursorMoved { position: at }))
        }

        fn click(&mut self, at: Point) {
            self.move_to(at);
            self.press(at);
            self.release(at);
        }

        fn press(&mut self, at: Point) {
            self.send(
                at,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            );
        }

        fn release(&mut self, at: Point) {
            self.send(
                at,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            );
        }

        fn redraw(&mut self, at: Point, now: Instant) {
            self.send(at, Event::Window(window::Event::RedrawRequested(now)));
        }

        /// Ctrl down, then C, as winit reports them on Windows.
        fn ctrl_c(&mut self, at: Point) {
            let ctrl = keyboard::Modifiers::CTRL;
            self.send(at, Event::Keyboard(keyboard::Event::ModifiersChanged(ctrl)));
            for (key, code, text) in [
                (
                    keyboard::Key::Named(key::Named::Control),
                    key::Code::ControlLeft,
                    None,
                ),
                (
                    keyboard::Key::Character("c".into()),
                    key::Code::KeyC,
                    Some("\u{3}".into()),
                ),
            ] {
                self.send(
                    at,
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        modified_key: key.clone(),
                        key,
                        physical_key: key::Physical::Code(code),
                        location: keyboard::Location::Standard,
                        modifiers: ctrl,
                        text,
                        repeat: false,
                    }),
                );
            }
        }

        /// The first point on the body line that shows the link pointer.
        fn find_link(&mut self) -> Point {
            (0..600)
                .step_by(2)
                .map(|x| Point::new(x as f32, BODY_Y))
                .find(|&at| self.move_to(at) == mouse::Interaction::Pointer)
                .map(|at| Point::new(at.x + 6.0, at.y))
                .expect("no link under the pointer")
        }
    }

    #[test]
    fn ctrl_c_copies_a_dragged_selection() {
        let mut chat = Chat::new("hello world");
        // The chat input had focus before, as it does after sending a message.
        chat.click(INPUT);
        let start = Point::new(1.0, BODY_Y);
        let end = Point::new(600.0, BODY_Y);
        assert_eq!(chat.move_to(start), mouse::Interaction::Text);
        chat.press(start);
        chat.move_to(end);
        chat.release(end);
        chat.ctrl_c(end);
        assert_eq!(chat.clipboard.0, ["hello world"]);
    }

    #[test]
    fn ctrl_c_copies_a_triple_clicked_message() {
        let mut chat = Chat::new("hello world");
        let at = Point::new(20.0, BODY_Y);
        for _ in 0..3 {
            chat.click(at);
        }
        chat.ctrl_c(INPUT);
        assert_eq!(chat.clipboard.0, ["Ada\nhello world"]);
    }

    #[test]
    fn a_click_on_a_link_publishes_it_once_a_double_click_is_ruled_out() {
        let mut chat = Chat::new("see https://a.example/x now");
        assert_eq!(
            chat.move_to(Point::new(1.0, BODY_Y)),
            mouse::Interaction::Text
        );
        let link = chat.find_link();
        chat.click(link);
        chat.redraw(link, Instant::now());
        assert!(chat.messages.is_empty());
        chat.redraw(link, Instant::now() + Duration::from_secs(1));
        assert_eq!(chat.messages, [Msg::Link(LINK.to_owned())]);
        assert!(chat.clipboard.0.is_empty());
    }

    #[test]
    fn a_double_click_selects_a_link_without_opening_it() {
        let mut chat = Chat::new("see https://a.example/x now");
        let link = chat.find_link();
        chat.click(link);
        chat.click(link);
        chat.redraw(link, Instant::now() + Duration::from_secs(1));
        assert!(chat.messages.is_empty());
        chat.ctrl_c(link);
        assert_eq!(chat.clipboard.0, [LINK]);
    }

    #[test]
    fn dragging_from_a_link_selects_without_opening_it() {
        let mut chat = Chat::new("see https://a.example/x now");
        let link = chat.find_link();
        let end = Point::new(600.0, BODY_Y);
        chat.press(link);
        chat.move_to(end);
        chat.release(end);
        chat.redraw(end, Instant::now() + Duration::from_secs(1));
        assert!(chat.messages.is_empty());
        chat.ctrl_c(end);
        let copied = chat.clipboard.0.concat();
        assert!(copied.ends_with("a.example/x now"), "{copied:?}");
    }

    #[test]
    fn body_links_index_the_transcript_by_character() {
        let links = body_links("Zoë", "à https://ü.example/ö.");
        let text = transcript("Zoë", "à https://ü.example/ö.");
        assert_eq!(links.len(), 1);
        let (range, url) = &links[0];
        assert_eq!(url, "https://ü.example/ö");
        assert_eq!(char_slice(&text, range.start, range.end), *url);
    }

    #[test]
    fn transcript_puts_the_name_on_its_own_line() {
        assert_eq!(transcript("Ada", "hello"), "Ada\nhello");
    }

    #[test]
    fn char_slice_uses_character_indexes_in_either_direction() {
        let text = transcript("Ada", "héllo");
        let end = text.chars().count();
        assert_eq!(char_slice(&text, 0, end), text);
        assert_eq!(char_slice(&text, 4, 7), "hél");
        assert_eq!(char_slice(&text, 7, 4), "hél");
        assert_eq!(char_slice(&text, 3, 3), "");
    }

    #[test]
    fn word_range_stops_at_whitespace_and_keeps_a_url_together() {
        let text = transcript("Ada", "see https://example.com/a now");
        let body = text.find("https").unwrap();
        let start = text[..body].chars().count();
        let url_chars = "https://example.com/a".chars().count();
        assert_eq!(word_range(&text, start + 4), (start, start + url_chars));
        let newline = "Ada".chars().count();
        assert_eq!(word_range(&text, newline), (newline, newline + 1));
        assert_eq!(word_range("", 0), (0, 0));
    }
}
