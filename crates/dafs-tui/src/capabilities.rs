//! Terminal capability detection: truecolor and Unicode/emoji support.
//!
//! No new dependency for this — it's the same handful of environment-variable
//! heuristics tools like starship and bat use, not something that needs a
//! crate. Detected once at startup: a terminal's capabilities don't change
//! mid-session.

#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub truecolor: bool,
    pub unicode: bool,
}

impl Capabilities {
    pub fn detect() -> Self {
        Self {
            truecolor: truecolor_from(std::env::var("COLORTERM").ok().as_deref()),
            unicode: unicode_from(
                std::env::var("TERM").ok().as_deref(),
                std::env::var("LC_ALL").ok().as_deref(),
                std::env::var("LC_CTYPE").ok().as_deref(),
                std::env::var("LANG").ok().as_deref(),
            ),
        }
    }
}

fn truecolor_from(colorterm: Option<&str>) -> bool {
    matches!(colorterm.map(str::to_ascii_lowercase).as_deref(), Some("truecolor") | Some("24bit"))
}

fn unicode_from(
    term: Option<&str>,
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    lang: Option<&str>,
) -> bool {
    // The Linux virtual console (kernel framebuffer text mode) reports a
    // UTF-8 locale but its built-in font has no emoji glyphs — treat it as
    // non-Unicode regardless of locale, the same special case bat/starship
    // make for `TERM=linux`.
    if term.is_some_and(|t| t.eq_ignore_ascii_case("linux")) {
        return false;
    }

    [lc_all, lc_ctype, lang].into_iter().flatten().any(|v| {
        let v = v.to_ascii_lowercase();
        v.contains("utf-8") || v.contains("utf8")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_recognizes_both_spellings_case_insensitively() {
        assert!(truecolor_from(Some("truecolor")));
        assert!(truecolor_from(Some("TrueColor")));
        assert!(truecolor_from(Some("24bit")));
        assert!(!truecolor_from(Some("256color")));
        assert!(!truecolor_from(None));
    }

    #[test]
    fn unicode_requires_a_utf8_locale() {
        assert!(unicode_from(Some("xterm-256color"), None, None, Some("en_US.UTF-8")));
        assert!(unicode_from(Some("xterm-256color"), Some("C.utf8"), None, None));
        assert!(!unicode_from(Some("xterm-256color"), None, None, Some("C")));
        assert!(!unicode_from(Some("xterm-256color"), None, None, None));
    }

    #[test]
    fn linux_console_is_never_unicode_even_with_a_utf8_locale() {
        assert!(!unicode_from(Some("linux"), None, None, Some("en_US.UTF-8")));
    }
}
