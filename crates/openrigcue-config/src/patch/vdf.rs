//! Parser for Valve KeyValues text (`libraryfolders.vdf`).

/// A KeyValues node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Text(String),
    Section(Vec<(String, Node)>),
}

impl Node {
    /// Child of a section by key.
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Section(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, node)| node),
            Node::Text(_) => None,
        }
    }

    /// Text of a leaf.
    pub fn text(&self) -> Option<&str> {
        match self {
            Node::Text(text) => Some(text),
            Node::Section(_) => None,
        }
    }
}

/// Parses a document into its top-level section; `None` on a syntax error.
pub fn parse(text: &str) -> Option<Node> {
    let mut tokens = tokenize(text)?.into_iter();
    let root = section(&mut tokens, false)?;
    Some(Node::Section(root))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Open,
    Close,
}

fn section(tokens: &mut std::vec::IntoIter<Token>, nested: bool) -> Option<Vec<(String, Node)>> {
    let mut entries: Vec<(String, Node)> = Vec::new();
    loop {
        let key = match tokens.next() {
            None => return (!nested).then_some(entries),
            Some(Token::Close) => return nested.then_some(entries),
            Some(Token::Open) => return None,
            Some(Token::Word(key)) => key,
        };
        let value = match tokens.next()? {
            Token::Word(text) => Node::Text(text),
            Token::Open => Node::Section(section(tokens, true)?),
            Token::Close => return None,
        };
        insert_merging(&mut entries, key, value);
    }
}

/// Duplicate keys: two sections merge, otherwise the later value replaces the earlier one in place.
fn insert_merging(entries: &mut Vec<(String, Node)>, key: String, value: Node) {
    let Some((_, existing)) = entries.iter_mut().find(|(name, _)| *name == key) else {
        entries.push((key, value));
        return;
    };
    match (existing, value) {
        (Node::Section(old), Node::Section(new)) => new.into_iter().for_each(|(k, v)| insert_merging(old, k, v)),
        (slot, value) => *slot = value,
    }
}

fn tokenize(text: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => tokens.push(Token::Open),
            '}' => tokens.push(Token::Close),
            '"' => tokens.push(Token::Word(quoted(&mut chars)?)),
            '/' if chars.peek() == Some(&'/') => {
                chars.by_ref().find(|c| *c == '\n');
            }
            '[' => {
                chars.by_ref().find(|c| *c == ']')?;
            }
            c if c.is_whitespace() => {}
            c => {
                let mut word = String::from(c);
                while let Some(next) = chars.next_if(|n| !n.is_whitespace() && !matches!(n, '{' | '}' | '"')) {
                    word.push(next);
                }
                tokens.push(Token::Word(word));
            }
        }
    }
    Some(tokens)
}

fn quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => out.push(match chars.next()? {
                'n' => '\n',
                't' => '\t',
                'v' => '\u{b}',
                'b' => '\u{8}',
                'r' => '\r',
                'f' => '\u{c}',
                'a' => '\u{7}',
                other => other,
            }),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBRARIES: &str = r#""libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"apps" { "228980" "1" }
	}
	// second drive
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}"#;

    /// Parsing libraryfolders.vdf: nested sections, escapes, comments.
    #[test]
    fn library_folders() {
        let root = parse(LIBRARIES).unwrap();
        let folders = root.get("libraryfolders").unwrap();
        assert_eq!(
            folders.get("0").and_then(|n| n.get("path")).and_then(Node::text),
            Some("/home/u/.local/share/Steam")
        );
        assert_eq!(folders.get("1").and_then(|n| n.get("path")).and_then(Node::text), Some("D:\\SteamLibrary"));
    }

    /// Duplicate-key merging and syntax errors.
    #[test]
    fn duplicates_and_errors() {
        let root = parse(r#""a" { "x" "1" } "a" { "y" "2" } "b" "1" "b" "2""#).unwrap();
        assert_eq!(root.get("a").and_then(|a| a.get("x")).and_then(Node::text), Some("1"));
        assert_eq!(root.get("a").and_then(|a| a.get("y")).and_then(Node::text), Some("2"));
        assert_eq!(root.get("b").and_then(Node::text), Some("2"));
        assert_eq!(parse(r#""a" { "x" "1""#), None);
        assert_eq!(parse(r#""a""#), None);
    }
}
