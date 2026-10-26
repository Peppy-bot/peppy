//! The comments of a contract document, which the JSON5 parser drops: the
//! comment before the document, the comment of each member and the comment
//! of each field of its message formats.
//!
//! A comment belongs to the key or the array item that follows it, and a
//! comment that starts on the line where a value ends belongs to the key of
//! that value. The comment of a member is the comment before its item in
//! its list, then the comment before its `name`. The scanner reads the text
//! that the parser accepted, so it follows the JSON5 grammar only as far as
//! the structure: strings, comments, braces, brackets, keys and values.

use config::node::InterfaceKind;
use std::collections::BTreeMap;

/// One message side of a member, as its document names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageSide {
    /// `message_format` of a topic.
    Message,
    /// `request_message_format` of a service.
    Request,
    /// `response_message_format` of a service.
    Response,
    /// `goal_service.request_message_format` of an action.
    Goal,
    /// `feedback_topic.message_format` of an action.
    Feedback,
    /// `result_service.response_message_format` of an action.
    Result,
}

impl MessageSide {
    /// Every side, in the order of the variants.
    pub const ALL: [Self; 6] = [
        Self::Message,
        Self::Request,
        Self::Response,
        Self::Goal,
        Self::Feedback,
        Self::Result,
    ];

    /// The name of the side: `message`, `request`, `response`, `goal`,
    /// `feedback` or `result`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Request => "request",
            Self::Response => "response",
            Self::Goal => "goal",
            Self::Feedback => "feedback",
            Self::Result => "result",
        }
    }

    /// The side named `name` (see [`Self::as_str`]).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|side| side.as_str() == name)
    }

    /// The side of a member of `kind` whose format sits under `path`, the
    /// keys from the member's item to the format, or `None` for a path that
    /// names no format of that kind.
    fn of(kind: InterfaceKind, path: &[&str]) -> Option<Self> {
        match (kind, path) {
            (InterfaceKind::Topic, ["message_format"]) => Some(Self::Message),
            (InterfaceKind::Service, ["request_message_format"]) => Some(Self::Request),
            (InterfaceKind::Service, ["response_message_format"]) => Some(Self::Response),
            (InterfaceKind::Action, ["goal_service", "request_message_format"]) => Some(Self::Goal),
            (InterfaceKind::Action, ["feedback_topic", "message_format"]) => Some(Self::Feedback),
            (InterfaceKind::Action, ["result_service", "response_message_format"]) => {
                Some(Self::Result)
            }
            _ => None,
        }
    }
}

/// The comments of one contract document. A field is named by its path in
/// its message format: the key of each object it sits in, from the root of
/// the format, with the items of an array passed through, so the field
/// `feature` of an item of `features` of an item of `options` is
/// `["options", "features", "feature"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContractComments {
    document: Option<String>,
    members: BTreeMap<(InterfaceKind, String), String>,
    fields: BTreeMap<(InterfaceKind, String, MessageSide, Vec<String>), String>,
}

impl ContractComments {
    /// The comments of `text`, a contract document that the parser
    /// accepted. A text that the scanner cannot follow gives no comment.
    pub fn of_document(text: &str) -> Self {
        let Some(scanned) = Scanner::new(text).scan() else {
            return Self::default();
        };
        scanned.into_comments()
    }

    /// The comment before the document.
    pub fn document(&self) -> Option<&str> {
        self.document.as_deref()
    }

    /// The comment of the member `name` of `kind`.
    pub fn member(&self, kind: InterfaceKind, name: &str) -> Option<&str> {
        self.members
            .get(&(kind, name.to_owned()))
            .map(String::as_str)
    }

    /// The comment of the field at `path` of the message `side` of the
    /// member `name` of `kind`.
    pub fn field(
        &self,
        kind: InterfaceKind,
        name: &str,
        side: MessageSide,
        path: &[&str],
    ) -> Option<&str> {
        self.fields
            .get(&(
                kind,
                name.to_owned(),
                side,
                path.iter().map(|key| (*key).to_owned()).collect(),
            ))
            .map(String::as_str)
    }
}

/// One step of a path in the document: the key of an object, or the index
/// of an item of an array.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Key(String),
    Index(usize),
}

/// What the scanner found: the comment of each path, and the name of each
/// member item, by the path of its item.
#[derive(Default)]
struct Scanned {
    by_path: BTreeMap<Vec<Step>, Vec<String>>,
    names: BTreeMap<Vec<Step>, String>,
}

impl Scanned {
    fn into_comments(self) -> ContractComments {
        let mut comments = ContractComments {
            document: self.by_path.get(&Vec::new()).map(|lines| lines.join("\n")),
            ..ContractComments::default()
        };
        for (path, lines) in &self.by_path {
            let Some((kind, item, rest)) = member_of(path) else {
                continue;
            };
            let Some(name) = self.names.get(&item) else {
                continue;
            };
            let text = lines.join("\n");
            if rest.is_empty() || rest == [Step::Key("name".to_owned())] {
                let entry = comments.members.entry((kind, name.clone())).or_default();
                if !entry.is_empty() {
                    entry.push('\n');
                }
                entry.push_str(&text);
                continue;
            }
            if let Some((side, field)) = field_of(kind, rest) {
                comments
                    .fields
                    .insert((kind, name.clone(), side, field), text);
            }
        }
        comments
    }
}

/// The kind of the member whose item `path` is in, the path of that item,
/// and the steps from the item to `path`.
fn member_of(path: &[Step]) -> Option<(InterfaceKind, Vec<Step>, &[Step])> {
    let [Step::Key(interfaces), Step::Key(list), Step::Index(_), ..] = path else {
        return None;
    };
    if interfaces != "interfaces" {
        return None;
    }
    let kind = match list.as_str() {
        "topics" => InterfaceKind::Topic,
        "services" => InterfaceKind::Service,
        "actions" => InterfaceKind::Action,
        _ => return None,
    };
    Some((kind, path[..3].to_vec(), &path[3..]))
}

/// The side and the field path of `rest`, the steps from a member's item to
/// a key inside one of its message formats.
fn field_of(kind: InterfaceKind, rest: &[Step]) -> Option<(MessageSide, Vec<String>)> {
    let keys: Vec<&str> = rest
        .iter()
        .filter_map(|step| match step {
            Step::Key(key) => Some(key.as_str()),
            Step::Index(_) => None,
        })
        .collect();
    (1..keys.len()).find_map(|split| {
        let side = MessageSide::of(kind, &keys[..split])?;
        let field: Vec<String> = keys[split..]
            .iter()
            .filter(|key| **key != "$items")
            .map(|key| (*key).to_owned())
            .collect();
        (!field.is_empty() && !field.iter().any(|key| key.starts_with('$')))
            .then_some((side, field))
    })
}

/// A scanner of the structure of a JSON5 text.
struct Scanner<'a> {
    text: &'a [u8],
    at: usize,
    /// The comment lines read since the last key or item.
    pending: Vec<String>,
    /// The path of the last key whose value ended, and the line it ended
    /// on, so a comment on that line joins that key.
    last_value: Option<(Vec<Step>, usize)>,
    found: Scanned,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text: text.as_bytes(),
            at: 0,
            pending: Vec::new(),
            last_value: None,
            found: Scanned::default(),
        }
    }

    fn scan(mut self) -> Option<Scanned> {
        self.skip_space();
        let comment = std::mem::take(&mut self.pending);
        if !comment.is_empty() {
            self.found.by_path.insert(Vec::new(), comment);
        }
        self.value(&mut Vec::new())?;
        Some(self.found)
    }

    fn line_of(&self, at: usize) -> usize {
        self.text[..at].iter().filter(|b| **b == b'\n').count()
    }

    /// Skips white space and comments, keeping each comment in `pending`,
    /// or with the last value when it starts on the line that value ended.
    fn skip_space(&mut self) {
        loop {
            while self.at < self.text.len() && self.text[self.at].is_ascii_whitespace() {
                self.at += 1;
            }
            let rest = &self.text[self.at..];
            let (comment, end) = if rest.starts_with(b"//") {
                let end = rest
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(self.text.len(), |offset| self.at + offset);
                let line = String::from_utf8_lossy(&self.text[self.at + 2..end]);
                (vec![strip_one_space(&line)], end)
            } else if rest.starts_with(b"/*") {
                let end = rest
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map_or(self.text.len(), |offset| self.at + offset + 2);
                let body = String::from_utf8_lossy(&self.text[self.at + 2..end.saturating_sub(2)]);
                let lines = body
                    .lines()
                    .map(|line| {
                        let line = line.trim();
                        strip_one_space(line.strip_prefix('*').unwrap_or(line))
                    })
                    .filter(|line| !line.is_empty())
                    .collect();
                (lines, end)
            } else {
                return;
            };
            let start_line = self.line_of(self.at);
            match &self.last_value {
                Some((path, line)) if *line == start_line && self.pending.is_empty() => {
                    self.found
                        .by_path
                        .entry(path.clone())
                        .or_default()
                        .extend(comment);
                }
                _ => self.pending.extend(comment),
            }
            self.at = end;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    /// Reads one value at `path`.
    fn value(&mut self, path: &mut Vec<Step>) -> Option<()> {
        self.skip_space();
        match self.peek()? {
            b'{' => self.object(path),
            b'[' => self.array(path),
            b'"' | b'\'' => self.string().map(|_| ()),
            _ => {
                self.bare_word()?;
                Some(())
            }
        }
    }

    fn object(&mut self, path: &mut Vec<Step>) -> Option<()> {
        self.at += 1;
        loop {
            self.skip_space();
            if self.peek()? == b'}' {
                self.at += 1;
                return Some(());
            }
            let key = match self.peek()? {
                b'"' | b'\'' => self.string()?,
                _ => self.bare_word()?,
            };
            path.push(Step::Key(key));
            let comment = std::mem::take(&mut self.pending);
            if !comment.is_empty() {
                self.found
                    .by_path
                    .entry(path.clone())
                    .or_default()
                    .extend(comment);
            }
            self.skip_space();
            if self.peek()? != b':' {
                return None;
            }
            self.at += 1;
            self.skip_space();
            if path.len() == 4
                && matches!(path.as_slice(), [Step::Key(i), _, Step::Index(_), Step::Key(name)]
                    if i == "interfaces" && name == "name")
                && matches!(self.peek(), Some(b'"' | b'\''))
            {
                let save = self.at;
                let name = self.string()?;
                self.found.names.insert(path[..3].to_vec(), name);
                self.at = save;
            }
            self.value(path)?;
            self.last_value = Some((path.clone(), self.line_of(self.at)));
            path.pop();
            self.skip_space();
            if self.peek()? == b',' {
                self.at += 1;
            }
        }
    }

    fn array(&mut self, path: &mut Vec<Step>) -> Option<()> {
        self.at += 1;
        let mut index = 0;
        loop {
            self.skip_space();
            if self.peek()? == b']' {
                self.at += 1;
                return Some(());
            }
            path.push(Step::Index(index));
            let comment = std::mem::take(&mut self.pending);
            if !comment.is_empty() {
                self.found
                    .by_path
                    .entry(path.clone())
                    .or_default()
                    .extend(comment);
            }
            self.value(path)?;
            self.last_value = None;
            path.pop();
            index += 1;
            self.skip_space();
            if self.peek()? == b',' {
                self.at += 1;
            }
        }
    }

    /// Reads a quoted string and gives its text.
    fn string(&mut self) -> Option<String> {
        let quote = self.peek()?;
        self.at += 1;
        let start = self.at;
        while self.peek()? != quote {
            if self.peek()? == b'\\' {
                self.at += 1;
            }
            self.at += 1;
        }
        let text = String::from_utf8_lossy(&self.text[start..self.at]).into_owned();
        self.at += 1;
        Some(text)
    }

    /// Reads an unquoted key, a number or a literal.
    fn bare_word(&mut self) -> Option<String> {
        let start = self.at;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() || matches!(byte, b',' | b':' | b'}' | b']' | b'/') {
                break;
            }
            self.at += 1;
        }
        (self.at > start).then(|| String::from_utf8_lossy(&self.text[start..self.at]).into_owned())
    }
}

/// `line` without the one space that follows a comment marker.
fn strip_one_space(line: &str) -> String {
    line.strip_prefix(' ').unwrap_or(line).trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &str = r#"// The copies of a launch.
// Second line of the header.
{
  peppy_schema: "contract/v1",
  manifest: { name: "copies", tag: "v1" },
  interfaces: {
    topics: [
      // The state of the stack.
      { name: "state", message_format: { up: "bool" } },
    ],
    services: [
      {
        // Lists the copies.
        name: "list",
        response_message_format: {
          // Each copy.
          copies: {
            $type: "array",
            $items: {
              $type: "object",
              name: "string", // The copy's name.
              /* The machine
               * of the copy. */
              core_node: "string",
            },
          },
          max_copies: "u16",
        },
      },
    ],
    actions: [
      // Adds a copy.
      {
        name: "join",
        goal_service: { request_message_format: {
          // The option, "a // b" is not a comment.
          option: "string",
        } },
        feedback_topic: { message_format: { line: "string" } },
      },
    ],
  },
}
"#;

    #[test]
    fn the_scanner_gives_the_comment_of_the_document_each_member_and_each_field() {
        let comments = ContractComments::of_document(DOCUMENT);
        assert_eq!(
            comments.document(),
            Some("The copies of a launch.\nSecond line of the header.")
        );
        assert_eq!(
            comments.member(InterfaceKind::Topic, "state"),
            Some("The state of the stack.")
        );
        assert_eq!(
            comments.member(InterfaceKind::Service, "list"),
            Some("Lists the copies.")
        );
        assert_eq!(
            comments.member(InterfaceKind::Action, "join"),
            Some("Adds a copy.")
        );
        let field = |kind, member, side, path: &[&str]| {
            comments.field(kind, member, side, path).map(str::to_owned)
        };
        assert_eq!(
            field(
                InterfaceKind::Service,
                "list",
                MessageSide::Response,
                &["copies"]
            ),
            Some("Each copy.".to_owned())
        );
        assert_eq!(
            field(
                InterfaceKind::Service,
                "list",
                MessageSide::Response,
                &["copies", "name"]
            ),
            Some("The copy's name.".to_owned()),
            "a comment on the line where a value ends belongs to its key"
        );
        assert_eq!(
            field(
                InterfaceKind::Service,
                "list",
                MessageSide::Response,
                &["copies", "core_node"]
            ),
            Some("The machine\nof the copy.".to_owned())
        );
        assert_eq!(
            field(
                InterfaceKind::Action,
                "join",
                MessageSide::Goal,
                &["option"]
            ),
            Some("The option, \"a // b\" is not a comment.".to_owned())
        );
        assert_eq!(
            field(
                InterfaceKind::Service,
                "list",
                MessageSide::Response,
                &["max_copies"]
            ),
            None
        );
        assert_eq!(
            field(
                InterfaceKind::Action,
                "join",
                MessageSide::Feedback,
                &["line"]
            ),
            None
        );
    }

    #[test]
    fn a_text_with_no_comment_gives_none() {
        let comments = ContractComments::of_document(
            r#"{ peppy_schema: "contract/v1", manifest: { name: "a", tag: "v1" }, interfaces: {} }"#,
        );
        assert_eq!(comments, ContractComments::default());
    }
}
