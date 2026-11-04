//! `{% python %}` blocks for MiniJinja, which has no API for custom tags.
//!
//! The tag is expanded in the template source before MiniJinja parses it:
//!
//! ```jinja
//! {% python target=150 %}          {% call py_block(target=150) %}{% raw %}
//! for r in regions:          ==>   for r in regions:
//!     print(r['name'])                 print(r['name'])
//! {% endpython %}                  {% endraw %}{% endcall %}
//! ```
//!
//! Wrapping the body in `{% raw %}` means `{{`, `{%` and `{#` inside the
//! Python are left alone. The expansion never adds or removes newlines, so
//! MiniJinja's error line numbers still match the original source.
//!
//! The scanner assumes MiniJinja's default delimiters; templates using a
//! custom `SyntaxConfig` are not supported.

use std::borrow::Cow;

use minijinja::{Error, ErrorKind, TemplateSource};

const OPEN: &str = "python";
const CLOSE: &str = "endpython";

/// Expands every `{% python %}...{% endpython %}` block in `source` into a
/// `py_block` call. Returns the source unchanged when it has none.
///
/// Use this for templates added with `add_template` or rendered with
/// `render_str`; [`python_tag_loader`] applies it to loaded templates.
pub fn expand_python_tags(source: &str) -> Result<Cow<'_, str>, Error> {
    let mut out = String::new();
    let mut copied = 0;
    let mut pos = 0;
    while let Some(found) = source[pos..].find('{') {
        let at = pos + found;
        let rest = &source[at..];
        pos = if rest.starts_with("{#") {
            find_after(source, at + 2, "#}")
        } else if rest.starts_with("{{") {
            close_of(source, at + 2, "}}").map_or(source.len(), |close| close + 2)
        } else if rest.starts_with("{%") {
            let Some(tag) = close_of(source, at + 2, "%}").map(|close| Tag::parse(source, at, close)) else {
                // Unterminated tag: leave it for MiniJinja to report.
                break;
            };
            match tag.name {
                "raw" => find_tag(source, tag.end, "endraw").map_or(source.len(), |end| end.end),
                OPEN => {
                    let close = find_tag(source, tag.end, CLOSE)
                        .ok_or_else(|| syntax_error(source, at, "unclosed {% python %} block"))?;
                    let body = &source[tag.end..close.start];
                    if find_tag(body, 0, "endraw").is_some() {
                        return Err(syntax_error(
                            source,
                            at,
                            "a {% python %} block cannot contain {% endraw %}",
                        ));
                    }
                    out.push_str(&source[copied..at]);
                    // Whitespace control on the outer edges carries over;
                    // markers on the inner edges are moot because
                    // `py_block` trims and dedents the body anyway.
                    out.push_str(&format!(
                        "{{%{} call py_block({}) %}}{{% raw %}}{body}{{% endraw %}}{{% endcall {}%}}",
                        tag.left, tag.args, close.right
                    ));
                    copied = close.end;
                    close.end
                }
                CLOSE => return Err(syntax_error(source, at, "{% endpython %} without {% python %}")),
                _ => tag.end,
            }
        } else {
            at + 1
        };
    }
    if copied == 0 {
        return Ok(Cow::Borrowed(source));
    }
    out.push_str(&source[copied..]);
    Ok(Cow::Owned(out))
}

/// Wraps a MiniJinja loader so every template it returns has its
/// `{% python %}` blocks expanded.
///
/// ```
/// # use minijinja::Environment;
/// let mut env = Environment::new();
/// env.set_loader(montyjinja::python_tag_loader(|name| {
///     Ok((name == "hi.txt").then(|| "{% python %}print('hi'){% endpython %}".to_owned()))
/// }));
/// ```
///
/// MiniJinja keeps a source's auto-reload check private, so sources from
/// `path_loader` lose it here; enable reloading by recreating the
/// environment instead.
pub fn python_tag_loader<F, R>(
    loader: F,
) -> impl Fn(&str) -> Result<Option<String>, Error> + Send + Sync + 'static
where
    F: Fn(&str) -> Result<Option<R>, Error> + Send + Sync + 'static,
    R: Into<TemplateSource>,
{
    move |name| {
        let Some(source) = loader(name)? else {
            return Ok(None);
        };
        let source: TemplateSource = source.into();
        let expanded = expand_python_tags(source.source()).map_err(|err| {
            Error::new(
                ErrorKind::SyntaxError,
                format!("{name}: {}", err.detail().unwrap_or_default()),
            )
        })?;
        Ok(Some(expanded.into_owned()))
    }
}

/// One `{% ... %}` block tag.
struct Tag<'a> {
    start: usize,
    /// Just past the closing `%}`.
    end: usize,
    /// Whitespace-control marker after `{%` (`-`, `+` or empty).
    left: &'a str,
    /// Whitespace-control marker before `%}`.
    right: &'a str,
    name: &'a str,
    args: &'a str,
}

impl<'a> Tag<'a> {
    /// Parses the tag from `start`, which points at `{%`, to `close`, which
    /// points at `%}`.
    fn parse(source: &'a str, start: usize, close: usize) -> Self {
        let mut inner = &source[start + 2..close];
        let left = marker(inner.as_bytes().first());
        inner = &inner[left.len()..];
        let right = marker(inner.as_bytes().last());
        inner = inner[..inner.len() - right.len()].trim();
        let name_len = inner
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(inner.len());
        Self {
            start,
            end: close + 2,
            left,
            right,
            name: &inner[..name_len],
            args: inner[name_len..].trim(),
        }
    }
}

fn marker(byte: Option<&u8>) -> &'static str {
    match byte {
        Some(b'-') => "-",
        Some(b'+') => "+",
        _ => "",
    }
}

/// Finds the first end tag named `name` at or after `from`.
///
/// Matches the text literally, without skipping quoted strings: the search
/// runs over raw content (Python code, `{% raw %}` bodies) where quotes have
/// no template meaning, and end tags take no arguments. This mirrors how
/// MiniJinja itself finds `{% endraw %}`.
fn find_tag<'a>(source: &'a str, from: usize, name: &str) -> Option<Tag<'a>> {
    let mut pos = from;
    while let Some(found) = source[pos..].find("{%") {
        let start = pos + found;
        let close = start + 2 + source[start + 2..].find("%}")?;
        let tag = Tag::parse(source, start, close);
        if tag.name == name {
            return Some(tag);
        }
        pos = start + 2;
    }
    None
}

/// Position of `closer` after `from`, skipping over quoted strings so that a
/// `"%}"` inside a tag argument does not end the tag early.
fn close_of(source: &str, from: usize, closer: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            _ if source[i..].starts_with(closer) => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_after(source: &str, from: usize, needle: &str) -> usize {
    source[from..]
        .find(needle)
        .map_or(source.len(), |i| from + i + needle.len())
}

fn syntax_error(source: &str, at: usize, msg: &str) -> Error {
    let line = source[..at].matches('\n').count() + 1;
    Error::new(ErrorKind::SyntaxError, format!("{msg} (line {line})"))
}

#[cfg(test)]
mod tests {
    use super::expand_python_tags;

    fn expand(source: &str) -> String {
        expand_python_tags(source).unwrap().into_owned()
    }

    #[test]
    fn expands_blocks_keeping_arguments_and_outer_whitespace_control() {
        assert_eq!(
            expand("a\n{%- python n=1, safe=true -%}\nprint(n)\n{%- endpython +%}\nb"),
            "a\n{%- call py_block(n=1, safe=true) %}{% raw %}\nprint(n)\n{% endraw %}{% endcall +%}\nb"
        );
    }

    #[test]
    fn leaves_everything_else_alone() {
        let source =
            "{# {% python %} #}{{ '{% python %}' }}{% raw %}{% python %}{% endraw %}{% if x %}y{% endif %}";
        assert!(matches!(
            expand_python_tags(source).unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn rejects_malformed_blocks() {
        for (source, msg) in [
            ("x\n{% python %}print(1)", "unclosed {% python %} block (line 2)"),
            ("{% endpython %}", "{% endpython %} without {% python %} (line 1)"),
            (
                "{% python %}{% endraw %}{% endpython %}",
                "cannot contain {% endraw %}",
            ),
        ] {
            let err = expand_python_tags(source).unwrap_err();
            assert!(err.to_string().contains(msg), "{source}: {err}");
        }
    }

    #[test]
    fn python_strings_do_not_hide_the_end_tag() {
        let out = expand("{% python %}print('{% \"'){% endpython %}tail");
        assert!(out.ends_with("{% endraw %}{% endcall %}tail"), "{out}");
    }

    #[test]
    fn preserves_line_count() {
        let source = "a\n{% python %}\nx = 1\nprint(x)\n{% endpython %}\nb\n";
        assert_eq!(expand(source).lines().count(), source.lines().count());
    }
}
