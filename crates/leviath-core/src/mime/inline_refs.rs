//! `@path` references inside a paragraph of text.
//!
//! A user writes `edit sprite image @hero.png so the arm is longer` and
//! expects the file to travel with the sentence. This module finds those
//! tokens. It does not read files: the caller supplies a resolver that says
//! whether a token names something, so the CLI resolves against the current
//! directory, the daemon against the run's workdir, and a test against
//! nothing at all. The text keeps every token exactly as written, so the
//! model and the stand-in agree on the name; only `\@` is rewritten, to `@`.

use super::MimeType;

/// One `@path` token found in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineRef {
    /// The token as written, with its `@` and any `:type` suffix.
    pub token: String,
    /// The path part, without `@` or the suffix.
    pub path: String,
    /// A `:type/subtype` suffix, when one was written.
    pub mime_type: Option<MimeType>,
    /// Byte offset of the token's `@` in the cleaned text.
    pub start: usize,
}

/// The cleaned text and the references found in it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extracted {
    /// The text with `\@` unescaped and nothing else changed.
    pub text: String,
    /// The references the resolver accepted, in text order.
    pub refs: Vec<InlineRef>,
    /// Tokens that looked like a file path but the resolver refused, so a
    /// caller can warn about a typo without treating an email as a file.
    pub unresolved: Vec<String>,
}

/// Characters that end a token.
fn ends_token(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            ',' | ';' | ')' | ']' | '}' | '"' | '\'' | '<' | '>' | '|'
        )
}

/// Trailing punctuation that belongs to the sentence, not the path.
fn trim_trailing(path: &str) -> &str {
    path.trim_end_matches(['.', ':', '!', '?'])
}

/// Whether a token is worth warning about when it does not resolve: it has a
/// slash or a dot-extension, which an `@handle` or an email does not.
fn looks_like_path(path: &str) -> bool {
    !path.contains('@')
        && (path.contains('/')
            || path.contains('\\')
            || std::path::Path::new(path)
                .extension()
                .is_some_and(|e| !e.is_empty()))
}

/// Find every `@path` token in `text`, keeping those `resolve` accepts.
///
/// `resolve` receives the path as written and answers whether it names a
/// file the caller is willing to attach. A token may end in `:type/subtype`
/// to name the type; the suffix is split off before resolution.
pub fn extract(text: &str, resolve: &mut dyn FnMut(&str) -> bool) -> Extracted {
    let mut out = Extracted::default();
    let mut cleaned = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && chars.get(i + 1) == Some(&'@') {
            cleaned.push('@');
            i += 2;
            continue;
        }
        let at_boundary = i == 0
            || chars[i - 1].is_whitespace()
            || matches!(chars[i - 1], '(' | '[' | '"' | '\'' | '<');
        if c == '@'
            && at_boundary
            && chars
                .get(i + 1)
                .is_some_and(|n| !n.is_whitespace() && *n != '@')
        {
            let mut j = i + 1;
            while j < chars.len() && !ends_token(chars[j]) {
                j += 1;
            }
            let raw: String = chars[i + 1..j].iter().collect();
            let trimmed = trim_trailing(&raw);
            let (path, mime_type) = split_type(trimmed);
            if !path.is_empty() && !path.contains('@') && resolve(&path) {
                let token: String = chars[i..i + 1 + trimmed.chars().count()].iter().collect();
                out.refs.push(InlineRef {
                    token,
                    path,
                    mime_type,
                    start: cleaned.len(),
                });
            } else if looks_like_path(&path) {
                out.unresolved.push(path);
            }
            cleaned.extend(&chars[i..j]);
            i = j;
            continue;
        }
        cleaned.push(c);
        i += 1;
    }
    out.text = cleaned;
    out
}

/// `photo.png:image/png` into `("photo.png", Some(image/png))`. A colon that
/// is not followed by a mime type stays part of the path (Windows drives,
/// `file:` URLs, a stray colon).
fn split_type(raw: &str) -> (String, Option<MimeType>) {
    if let Some((path, suffix)) = raw.rsplit_once(':')
        && suffix.contains('/')
        && let Ok(t) = MimeType::parse(suffix)
    {
        return (path.to_string(), Some(t));
    }
    (raw.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(text: &str) -> Extracted {
        extract(text, &mut |_| true)
    }

    #[test]
    fn finds_tokens_at_start_middle_and_end() {
        let e = all("@a.png then @dir/b.wav and finally @c.mp4");
        let paths: Vec<_> = e.refs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["a.png", "dir/b.wav", "c.mp4"]);
        assert_eq!(e.text, "@a.png then @dir/b.wav and finally @c.mp4");
        assert_eq!(e.refs[0].start, 0);
        assert_eq!(e.refs[1].start, 12);
        assert_eq!(e.refs[1].token, "@dir/b.wav");
        assert!(e.unresolved.is_empty());
    }

    #[test]
    fn sentence_punctuation_is_not_part_of_the_path() {
        let e = all("look at @hero.png. Then (@b.png), \"@c.png\" and [@d.png]; ok?");
        let paths: Vec<_> = e.refs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["hero.png", "b.png", "c.png", "d.png"]);
        assert_eq!(e.refs[0].token, "@hero.png");
        assert!(e.text.contains("@hero.png. Then"));
    }

    #[test]
    fn type_suffix_is_split_off() {
        let e = all("use @scene.bin:model/gltf-binary here and @c:\\x\\y.obj");
        assert_eq!(e.refs[0].path, "scene.bin");
        assert_eq!(
            e.refs[0].mime_type.as_ref().unwrap().as_str(),
            "model/gltf-binary"
        );
        assert_eq!(e.refs[0].token, "@scene.bin:model/gltf-binary");
        assert_eq!(e.refs[1].path, "c:\\x\\y.obj");
        assert!(e.refs[1].mime_type.is_none());
        let e = all("@a.txt:notatype");
        assert_eq!(e.refs[0].path, "a.txt:notatype");
        let e = all("@a.txt:not/a/type");
        assert_eq!(e.refs[0].path, "a.txt:not/a/type");
        assert!(e.refs[0].mime_type.is_none());
    }

    #[test]
    fn escapes_and_non_tokens() {
        let e = all(r"mail me\@example.com, ping @@twice, a@b.c, \@literal and email me@x.io");
        assert!(e.refs.is_empty());
        assert_eq!(
            e.text,
            "mail me@example.com, ping @@twice, a@b.c, @literal and email me@x.io"
        );
        assert!(e.unresolved.is_empty());
        let e = all("@");
        assert!(e.refs.is_empty());
        assert_eq!(e.text, "@");
        let e = all("@ space");
        assert!(e.refs.is_empty());
        let e = all("end with \\");
        assert_eq!(e.text, "end with \\");
        let e = all("a \\x b @a@b @. c");
        assert!(e.refs.is_empty(), "{:?}", e.refs);
        assert!(e.unresolved.is_empty(), "{:?}", e.unresolved);
        assert_eq!(e.text, "a \\x b @a@b @. c");
    }

    #[test]
    fn resolver_decides_and_unresolved_paths_are_reported() {
        let e = extract("see @real.png and @missing.png and @channel", &mut |p| {
            p == "real.png"
        });
        assert_eq!(e.refs.len(), 1);
        assert_eq!(e.refs[0].path, "real.png");
        assert_eq!(e.unresolved, vec!["missing.png"]);
        assert_eq!(e.text, "see @real.png and @missing.png and @channel");
        let e = extract("@dir/thing and @a\\b", &mut |_| false);
        assert_eq!(e.unresolved, vec!["dir/thing", "a\\b"]);
        assert!(!looks_like_path("name."));
        assert!(!looks_like_path("a@b.c"));
    }

    #[test]
    fn unicode_text_keeps_offsets_in_bytes() {
        let e = all("héllo @ü.png done");
        assert_eq!(e.refs[0].start, "héllo ".len());
        assert_eq!(e.refs[0].path, "ü.png");
        assert_eq!(e.text, "héllo @ü.png done");
    }
}
