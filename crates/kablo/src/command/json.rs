//! The one thing `bateri message --json-field` reads of a JSON document: a string member of its
//! top-level object.
//!
//! The document is a hook's (Claude Code's `Stop` hook gives its `last_assistant_message` on
//! standard input), so it comes from a program and not from a person, and it is read as strictly as
//! RFC 8259 writes it: all of it is read, not up to the member that was asked for, and a document
//! that is cut short, has a comma too many or is followed by anything is no document at all. The
//! answer is `None`, never a panic and never a half of a text. No dependency: one field of one
//! shape does not make a JSON library worth the line in `Cargo.toml`.

/// How deep arrays and objects may lie in one another before the document is refused. The reader
/// calls itself once per level, so this is what keeps a document of a million `[` from taking the
/// stack with it.
const MAX_DEPTH: usize = 64;

/// The text of the member `field` of the object `input` is, or `None` when `input` is no JSON
/// object, is not all there, has no such member, or the member is not a string. A member of an
/// object inside it, or of an array, is not the member: only the top level is looked at. A name
/// that is written twice is the last one, as every JSON reader has it.
pub fn string_field(input: &[u8], field: &str) -> Option<String> {
    let mut reader = Reader {
        bytes: std::str::from_utf8(input).ok()?.as_bytes(),
        at: 0,
    };
    reader.blank();
    let found = reader.object(0, Some(field))?;
    reader.blank();
    (reader.at == reader.bytes.len()).then_some(())?;
    found
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn blank(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        if self.peek()? == byte {
            self.at += 1;
            Some(())
        } else {
            None
        }
    }

    fn literal(&mut self, word: &[u8]) -> Option<()> {
        let end = self.at.checked_add(word.len())?;
        if self.bytes.get(self.at..end)? == word {
            self.at = end;
            Some(())
        } else {
            None
        }
    }

    /// An object, from its `{`: the string member called `want` (when one is wanted) is returned,
    /// every other member is read and dropped. `Some(None)` is an object that has none, or one
    /// whose member is not a string.
    fn object(&mut self, depth: usize, want: Option<&str>) -> Option<Option<String>> {
        if depth >= MAX_DEPTH {
            return None;
        }
        self.eat(b'{')?;
        let mut found = None;
        self.blank();
        if self.peek()? == b'}' {
            self.at += 1;
            return Some(found);
        }
        loop {
            self.blank();
            let key = self.string()?;
            self.blank();
            self.eat(b':')?;
            self.blank();
            if want == Some(key.as_str()) {
                found = if self.peek()? == b'"' {
                    Some(self.string()?)
                } else {
                    self.skip(depth)?;
                    None
                };
            } else {
                self.skip(depth)?;
            }
            self.blank();
            match self.peek()? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    return Some(found);
                }
                _ => return None,
            }
        }
    }

    /// A value, read and dropped; `depth` is that of the object or array it is in.
    fn skip(&mut self, depth: usize) -> Option<()> {
        self.blank();
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' => {
                self.object(depth + 1, None)?;
            }
            b'[' => self.array(depth + 1)?,
            b't' => self.literal(b"true")?,
            b'f' => self.literal(b"false")?,
            b'n' => self.literal(b"null")?,
            _ => self.number()?,
        }
        Some(())
    }

    fn array(&mut self, depth: usize) -> Option<()> {
        if depth >= MAX_DEPTH {
            return None;
        }
        self.eat(b'[')?;
        self.blank();
        if self.peek()? == b']' {
            self.at += 1;
            return Some(());
        }
        loop {
            self.skip(depth)?;
            self.blank();
            match self.peek()? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// `-`, an integer with no leading zero, a fraction and an exponent, each when it is there.
    fn number(&mut self) -> Option<()> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek()? {
            b'0' => self.at += 1,
            b'1'..=b'9' => {
                self.digits();
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            (self.digits() > 0).then_some(())?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            (self.digits() > 0).then_some(())?;
        }
        Some(())
    }

    /// How many digits there were.
    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    /// A string, from its `"`, with its escapes read. The bytes between them are copied as they
    /// are: the document is UTF-8 as a whole and is cut only at an ASCII `"` or `\`.
    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = Vec::new();
        loop {
            let byte = self.peek()?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(self.escape()?.encode_utf8(&mut buf).as_bytes());
                }
                // A line break or another control character is written `\n`, `\u0007`.
                0..=0x1f => return None,
                other => out.push(other),
            }
        }
    }

    /// What follows a `\`. A surrogate with no partner (`\ud83d` alone, or `\ude00` first) is no
    /// character: it reads as U+FFFD, as a browser's reader gives it, and the text is kept.
    fn escape(&mut self) -> Option<char> {
        let byte = self.peek()?;
        self.at += 1;
        Some(match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let first = self.hex4()?;
                match first {
                    0xD800..=0xDBFF => {
                        let low = self.bytes.get(self.at..self.at + 2) == Some(b"\\u".as_slice());
                        let after_low = self.at;
                        if low {
                            self.at += 2;
                            let second = self.hex4()?;
                            if (0xDC00..=0xDFFF).contains(&second) {
                                char::from_u32(
                                    0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00),
                                )
                                .unwrap_or('\u{FFFD}')
                            } else {
                                // Not its partner: the escape is read again as its own.
                                self.at = after_low;
                                '\u{FFFD}'
                            }
                        } else {
                            '\u{FFFD}'
                        }
                    }
                    0xDC00..=0xDFFF => '\u{FFFD}',
                    other => char::from_u32(other)?,
                }
            }
            _ => return None,
        })
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.at..self.at + 4)?;
        let mut value = 0;
        for digit in digits {
            value = value * 16 + char::from(*digit).to_digit(16)?;
        }
        self.at += 4;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(input: &str) -> Option<String> {
        string_field(input.as_bytes(), "text")
    }

    #[test]
    fn a_string_member_of_the_top_level_is_read() {
        assert_eq!(field(r#"{"text":"hi"}"#).as_deref(), Some("hi"));
        assert_eq!(
            field(" \n{ \"a\" : 1 ,\t\"text\" :\r\"hi there\" , \"b\":[] }\n").as_deref(),
            Some("hi there")
        );
        assert_eq!(field(r#"{"text":""}"#).as_deref(), Some(""));
        assert_eq!(field(r#"{"text":"v"}"#).as_deref(), Some("v"));
        // The characters that are not escaped are the document's own.
        assert_eq!(
            field("{\"text\":\"é — 😀 \u{202e}\"}").as_deref(),
            Some("é — 😀 \u{202e}")
        );
    }

    #[test]
    fn a_stop_hooks_payload_gives_the_last_message() {
        let payload = r#"{"session_id":"abc","transcript_path":"/home/a/.claude/projects/x.jsonl","cwd":"/work","permission_mode":"default","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"Done.\n\nThree tests failed:\n- \"a\"\n- b"}"#;
        assert_eq!(
            string_field(payload.as_bytes(), "last_assistant_message").as_deref(),
            Some("Done.\n\nThree tests failed:\n- \"a\"\n- b")
        );
        assert_eq!(
            string_field(payload.as_bytes(), "hook_event_name").as_deref(),
            Some("Stop")
        );
        assert_eq!(string_field(payload.as_bytes(), "message"), None);
    }

    #[test]
    fn every_escape_is_read() {
        assert_eq!(
            field(r#"{"text":"\" \\ \/ \b \f \n \r \t é A €"}"#).as_deref(),
            Some("\" \\ / \u{8} \u{c} \n \r \t é A €")
        );
        // A pair of surrogates is one character.
        assert_eq!(field(r#"{"text":"a😀b"}"#).as_deref(), Some("a😀b"));
        assert_eq!(field(r#"{"text":"𝄞"}"#).as_deref(), Some("\u{1d11e}"));
    }

    #[test]
    fn a_surrogate_with_no_partner_is_kept_as_a_replacement_character() {
        for (input, text) in [
            (r#"{"text":"a\ud83db"}"#, "a\u{fffd}b"),
            (r#"{"text":"a\ude00b"}"#, "a\u{fffd}b"),
            (r#"{"text":"\ud83d"}"#, "\u{fffd}"),
            // A high surrogate and an escape that is not its partner: both are read.
            (r#"{"text":"\ud83dA"}"#, "\u{fffd}A"),
            (r#"{"text":"\ud83d😀"}"#, "\u{fffd}😀"),
            (r#"{"text":"\ud83d\n"}"#, "\u{fffd}\n"),
        ] {
            assert_eq!(field(input).as_deref(), Some(text), "{input}");
        }
    }

    #[test]
    fn a_member_inside_another_is_not_the_member() {
        assert_eq!(
            field(r#"{"a":{"text":"inner"},"text":"outer"}"#).as_deref(),
            Some("outer")
        );
        assert_eq!(
            field(r#"{"text":"outer","a":{"text":"inner"}}"#).as_deref(),
            Some("outer")
        );
        assert_eq!(field(r#"{"a":{"text":"inner"}}"#), None);
        assert_eq!(field(r#"{"a":[{"text":"inner"},"text"]}"#), None);
        assert_eq!(field(r#"{"a":"text"}"#), None);
        assert_eq!(field(r#"{"texts":"x","tex":"y"}"#), None);
        assert_eq!(field(r#"{"Text":"x"}"#), None);
        assert_eq!(field(r#"{}"#), None);
    }

    #[test]
    fn a_name_written_twice_is_the_last_one() {
        assert_eq!(field(r#"{"text":"a","text":"b"}"#).as_deref(), Some("b"));
        // And the last one decides even when it is no string.
        assert_eq!(field(r#"{"text":"a","text":null}"#), None);
        assert_eq!(field(r#"{"text":1,"text":"b"}"#).as_deref(), Some("b"));
    }

    #[test]
    fn a_member_that_is_no_string_gives_nothing() {
        for input in [
            r#"{"text":5}"#,
            r#"{"text":-1.5e+3}"#,
            r#"{"text":null}"#,
            r#"{"text":true}"#,
            r#"{"text":false}"#,
            r#"{"text":["x"]}"#,
            r#"{"text":{"x":"y"}}"#,
            r#"{"text":[]}"#,
            r#"{"text":{}}"#,
        ] {
            assert_eq!(field(input), None, "{input}");
        }
    }

    #[test]
    fn what_is_no_object_gives_nothing() {
        for input in [
            "",
            "   ",
            "text",
            r#""text""#,
            "[]",
            r#"["text"]"#,
            "null",
            "5",
            r#"{"text":"x"} {"text":"y"}"#,
            r#"{"text":"x"}x"#,
            r#"{"text":"x"},"#,
        ] {
            assert_eq!(field(input), None, "{input:?}");
        }
    }

    #[test]
    fn a_document_that_is_cut_short_gives_nothing() {
        let whole = r#"{"a":[1,{"b":"cé😀"},true,null,-0.5e2],"text":"hello \"you\"","z":{}}"#;
        assert_eq!(field(whole).as_deref(), Some("hello \"you\""));
        for end in 0..whole.len() {
            // Only at a character boundary: a document is text.
            let Some(part) = whole.get(..end) else {
                continue;
            };
            assert_eq!(field(part), None, "{part:?}");
        }
    }

    #[test]
    fn a_document_that_is_not_json_gives_nothing() {
        for input in [
            r#"{"text":"x",}"#,
            r#"{,"text":"x"}"#,
            r#"{"text" "x"}"#,
            r#"{"text":}"#,
            r#"{"text":"x" "b":1}"#,
            r#"{text:"x"}"#,
            r#"{'text':'x'}"#,
            r#"{"text":"\q"}"#,
            r#"{"text":"\u12"}"#,
            r#"{"text":"\u12g4"}"#,
            r#"{"text":"\"}"#,
            "{\"text\":\"a\nb\"}",
            "{\"text\":\"a\u{7}b\"}",
            r#"{"a":01,"text":"x"}"#,
            r#"{"a":-,"text":"x"}"#,
            r#"{"a":1.,"text":"x"}"#,
            r#"{"a":1e,"text":"x"}"#,
            r#"{"a":+1,"text":"x"}"#,
            r#"{"a":tru,"text":"x"}"#,
            r#"{"a":nul,"text":"x"}"#,
            r#"{"a":[1,],"text":"x"}"#,
            r#"{"a":[1 2],"text":"x"}"#,
            r#"{"a":[,],"text":"x"}"#,
            r#"{"a":NaN,"text":"x"}"#,
        ] {
            assert_eq!(field(input), None, "{input:?}");
        }
        // Bytes that are not UTF-8 are no text.
        assert_eq!(string_field(b"{\"text\":\"a\xffb\"}", "text"), None);
        assert_eq!(string_field(b"\xef\xbb\xbf{\"text\":\"x\"}", "text"), None);
    }

    #[test]
    fn a_document_nested_too_deep_gives_nothing_and_does_not_overflow_the_stack() {
        let deep = format!(
            "{{\"a\":{}1{},\"text\":\"x\"}}",
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        assert_eq!(field(&deep), None);
        let deep = format!(
            "{{\"a\":{}1{},\"text\":\"x\"}}",
            "{\"a\":".repeat(100_000),
            "}".repeat(100_000)
        );
        assert_eq!(field(&deep), None);
        let open = "[".repeat(1_000_000);
        assert_eq!(field(&open), None);
        // A few levels are fine.
        let shallow = format!(
            "{{\"a\":{}1{},\"text\":\"x\"}}",
            "[".repeat(30),
            "]".repeat(30)
        );
        assert_eq!(field(&shallow).as_deref(), Some("x"));
    }

    #[test]
    fn a_long_text_is_read_whole() {
        let long = "ab\\n".repeat(200_000);
        let input = format!("{{\"text\":\"{long}\"}}");
        assert_eq!(field(&input), Some("ab\n".repeat(200_000)));
    }
}
