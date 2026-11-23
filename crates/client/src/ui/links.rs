//! Web links in chat messages: finding them in text and opening them in the browser.

use std::io;
use std::ops::Range;
use std::process::{Command, Stdio};

const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Byte ranges of the `http://` and `https://` links in `text`.
///
/// A link starts at a scheme that does not follow a letter or digit and runs to the
/// next whitespace. Trailing `.,;:!?`, quotes, and a `)` or `]` without a matching
/// opener inside the link are dropped, so "see (https://a.example/x)." links only
/// `https://a.example/x`. A bare scheme is not a link.
pub(super) fn find(text: &str) -> Vec<Range<usize>> {
    let mut links = Vec::new();
    let mut from = 0;
    while let Some((start, scheme)) = next_scheme(text, from) {
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |len| start + len);
        let end = start + trimmed_len(&text[start..end]);
        if end > start + scheme.len() {
            links.push(start..end);
            from = end;
        } else {
            from = start + scheme.len();
        }
    }
    links
}

/// The first scheme at or after `from` that starts a word, with its byte offset.
fn next_scheme(text: &str, from: usize) -> Option<(usize, &'static str)> {
    let mut at = from;
    loop {
        let (offset, scheme) = SCHEMES
            .iter()
            .filter_map(|scheme| text[at..].find(scheme).map(|offset| (offset, *scheme)))
            .min_by_key(|(offset, _)| *offset)?;
        let start = at + offset;
        let starts_word = !text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        if starts_word {
            return Some((start, scheme));
        }
        at = start + scheme.len();
    }
}

/// Length of `link` without trailing punctuation that usually ends the sentence.
fn trimmed_len(link: &str) -> usize {
    let mut link = link;
    loop {
        let Some(last) = link.chars().next_back() else {
            return 0;
        };
        let drop = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' => true,
            ')' => link.matches('(').count() < link.matches(')').count(),
            ']' => link.matches('[').count() < link.matches(']').count(),
            _ => false,
        };
        if !drop {
            return link.len();
        }
        link = &link[..link.len() - last.len_utf8()];
    }
}

/// Opens `url` in the default browser without waiting for it. Only `http` and
/// `https` URLs are opened.
pub(super) fn open(url: &str) -> io::Result<()> {
    if !SCHEMES.iter().any(|scheme| url.starts_with(scheme))
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only http and https links are opened",
        ));
    }
    let mut child = browser(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reaped off the UI thread so no zombie is left behind.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(windows)]
fn browser(url: &str) -> Command {
    use std::os::windows::process::CommandExt;
    // rundll32 hands the rest of its command line to the shell's URL handler as is,
    // so the URL goes unquoted (it has no whitespace) and cmd.exe never sees `&`.
    let mut command = Command::new("rundll32");
    command.arg("url.dll,FileProtocolHandler").raw_arg(url);
    command
}

#[cfg(target_os = "macos")]
fn browser(url: &str) -> Command {
    let mut command = Command::new("open");
    command.arg(url);
    command
}

#[cfg(not(any(windows, target_os = "macos")))]
fn browser(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}

#[cfg(test)]
mod tests {
    use super::{find, open};

    fn links(text: &str) -> Vec<&str> {
        find(text).into_iter().map(|range| &text[range]).collect()
    }

    #[test]
    fn finds_links_up_to_whitespace() {
        assert_eq!(
            links("see https://a.example/x?q=1&b=2#top and http://b.example"),
            ["https://a.example/x?q=1&b=2#top", "http://b.example"]
        );
        assert_eq!(links("https://a.example"), ["https://a.example"]);
        assert_eq!(
            links("héllo https://ü.example/ö ok"),
            ["https://ü.example/ö"]
        );
        assert!(links("no links, ftp://a.example or www.example.com").is_empty());
    }

    #[test]
    fn drops_trailing_punctuation_and_unbalanced_brackets() {
        assert_eq!(links("go to https://a.example."), ["https://a.example"]);
        assert_eq!(links("https://a.example/x?!,;:"), ["https://a.example/x"]);
        assert_eq!(links("(see https://a.example/x)"), ["https://a.example/x"]);
        assert_eq!(links("[https://a.example/x]."), ["https://a.example/x"]);
        assert_eq!(links("\"https://a.example/x\""), ["https://a.example/x"]);
        assert_eq!(links("'https://a.example/x'"), ["https://a.example/x"]);
        assert_eq!(
            links("https://en.wikipedia.org/wiki/Rust_(programming_language)."),
            ["https://en.wikipedia.org/wiki/Rust_(programming_language)"]
        );
        assert_eq!(links("https://a.example/[1]"), ["https://a.example/[1]"]);
    }

    #[test]
    fn needs_a_word_start_and_something_after_the_scheme() {
        assert!(links("xhttps://a.example").is_empty());
        assert!(links("https:// http://.").is_empty());
        assert_eq!(links("(https://a.example)"), ["https://a.example"]);
        assert_eq!(links("xhttps://a https://b"), ["https://b"]);
        assert_eq!(
            links("https://a.example,https://b.example"),
            ["https://a.example,https://b.example"]
        );
    }

    #[test]
    fn open_refuses_other_schemes() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "C:\\Windows\\notepad.exe",
            "https://a.example/\nx",
            "https://a.example/ x",
        ] {
            assert!(open(url).is_err(), "{url}");
        }
    }
}
