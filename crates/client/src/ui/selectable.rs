//! Selectable chat messages.
//!
//! iced's `text` widget cannot be highlighted. Each message is one paragraph — the
//! display name, then the body — laid out as one span per character so a click can
//! resolve to a character index. The chat log stays in the normal scrollable; this
//! widget does not take the mouse wheel.

use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::text as text_widget;
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Clipboard, Shell, clipboard, layout, mouse, renderer, widget};
use iced::alignment;
use iced::keyboard;
use iced::{Element, Event, Length, Pixels, Point, Rectangle, Size, Theme};

const NAME_SIZE: f32 = 12.0;

/// A chat message the user can highlight and copy.
pub(super) fn message<'a, Message: 'a>(
    id: u64,
    name: &'a str,
    body: &'a str,
) -> Element<'a, Message> {
    SelectableMessage { id, name, body }.into()
}

struct SelectableMessage<'a> {
    id: u64,
    name: &'a str,
    body: &'a str,
}

struct State<P> {
    built: bool,
    id: u64,
    name: String,
    body: String,
    /// `name`, a newline, then `body`. Character indexes in the selection refer to this.
    text: String,
    chars: usize,
    paragraph: P,
    anchor: usize,
    cursor: usize,
    focused: bool,
    /// A single click is being dragged into a selection. Double- and triple-clicks are not.
    single_drag: bool,
    modifiers: keyboard::Modifiers,
    last_click: Option<mouse::Click>,
}

impl<'a, Message, R> widget::Widget<Message, Theme, R> for SelectableMessage<'a>
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
            paragraph: R::Paragraph::default(),
            anchor: 0,
            cursor: 0,
            focused: false,
            single_drag: false,
            modifiers: keyboard::Modifiers::empty(),
            last_click: None,
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
                self.id,
                self.name,
                self.body,
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
        let (start, end) = selection_range(state.anchor, state.cursor);
        if state.focused && start < end {
            let translation = layout.position() - Point::ORIGIN;
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
                    }
                    mouse::click::Kind::Double => {
                        let index = char_under(&state.paragraph, state.chars, local)
                            .unwrap_or_else(|| state.cursor.min(state.chars.saturating_sub(1)));
                        let (start, end) = word_range(&state.text, index);
                        state.anchor = start;
                        state.cursor = end;
                        state.single_drag = false;
                    }
                    mouse::click::Kind::Triple => {
                        state.anchor = 0;
                        state.cursor = state.chars;
                        state.single_drag = false;
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
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.single_drag = false;
            }
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
        _tree: &Tree,
        layout: layout::Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &R,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::default()
        }
    }
}

impl<'a, Message: 'a> From<SelectableMessage<'a>> for Element<'a, Message> {
    fn from(message: SelectableMessage<'a>) -> Self {
        Self::new(message)
    }
}

fn sync<P, Font>(
    state: &mut State<P>,
    id: u64,
    name: &str,
    body: &str,
    bounds: Size,
    size: Pixels,
    font: Font,
) where
    P: text::Paragraph<Font = Font>,
    Font: Copy + PartialEq,
{
    let changed = !state.built || state.id != id || state.name != name || state.body != body;
    if changed {
        state.built = true;
        state.id = id;
        state.name.clear();
        state.name.push_str(name);
        state.body.clear();
        state.body.push_str(body);
        state.text = transcript(name, body);
        clear_selection(state);
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
    use super::{char_slice, transcript, word_range};

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
