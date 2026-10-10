//! Valve's text KeyValues format (`libraryfolders.vdf`, `appmanifest_*.acf`).

#[derive(Clone, Debug, PartialEq)]
pub enum Vdf {
    Str(String),
    Obj(Vec<(String, Vdf)>),
}

impl Vdf {
    /// A child by key, case-insensitively (Steam is not consistent).
    pub fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Vdf::Obj(v) => v
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            Vdf::Str(_) => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Vdf::Str(s) => Some(s),
            Vdf::Obj(_) => None,
        }
    }

    pub fn pairs(&self) -> &[(String, Vdf)] {
        match self {
            Vdf::Obj(v) => v,
            Vdf::Str(_) => &[],
        }
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Str(String),
    Open,
    Close,
}

fn tokens(text: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut it = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(&c) = it.peek() {
        match c {
            c if c.is_whitespace() => {
                it.next();
            }
            '/' => {
                it.next();
                if it.peek() == Some(&'/') {
                    for c in it.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                } else {
                    return None;
                }
            }
            '{' => {
                it.next();
                out.push(Tok::Open);
            }
            '}' => {
                it.next();
                out.push(Tok::Close);
            }
            '"' => {
                it.next();
                let mut s = String::new();
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => s.push(match it.next()? {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        }),
                        c => s.push(c),
                    }
                }
                out.push(Tok::Str(s));
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = it.peek() {
                    if c.is_whitespace() || matches!(c, '"' | '{' | '}') {
                        break;
                    }
                    s.push(c);
                    it.next();
                }
                out.push(Tok::Str(s));
            }
        }
    }
    Some(out)
}

fn object(toks: &[Tok], i: &mut usize, depth: u32, top: bool) -> Option<Vec<(String, Vdf)>> {
    if depth > 32 {
        return None;
    }
    let mut pairs = Vec::new();
    loop {
        match toks.get(*i) {
            None => return top.then_some(pairs),
            Some(Tok::Close) => {
                *i += 1;
                return (!top).then_some(pairs);
            }
            Some(Tok::Open) => return None,
            Some(Tok::Str(k)) => {
                *i += 1;
                match toks.get(*i)? {
                    Tok::Str(v) => pairs.push((k.clone(), Vdf::Str(v.clone()))),
                    Tok::Open => {
                        *i += 1;
                        pairs.push((k.clone(), Vdf::Obj(object(toks, i, depth + 1, false)?)));
                        continue;
                    }
                    Tok::Close => return None,
                }
                *i += 1;
            }
        }
    }
}

/// Parses a whole file into one root object (its top-level pairs). `None`
/// on malformed input.
pub fn parse(text: &str) -> Option<Vdf> {
    let toks = tokens(text)?;
    let mut i = 0;
    object(&toks, &mut i, 0, true).map(Vdf::Obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBS: &str = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"apps"
		{
			"228980"		"123"
		}
	}
	"1"
	{
		"path"		"D:\\Games\\Steam Library"
	}
}
"#;

    #[test]
    fn nested_keys_and_escaped_backslashes() {
        let v = parse(LIBS).unwrap();
        let libs = v.get("LibraryFolders").unwrap();
        assert_eq!(libs.pairs().len(), 2);
        assert_eq!(
            libs.get("0").unwrap().get("path").unwrap().as_str(),
            Some(r"C:\Program Files (x86)\Steam")
        );
        assert_eq!(
            libs.get("1").unwrap().get("PATH").unwrap().as_str(),
            Some(r"D:\Games\Steam Library")
        );
        assert_eq!(
            libs.get("0")
                .unwrap()
                .get("apps")
                .unwrap()
                .get("228980")
                .unwrap()
                .as_str(),
            Some("123")
        );
        assert_eq!(
            libs.get("0").unwrap().get("label").unwrap().as_str(),
            Some("")
        );
    }

    #[test]
    fn quotes_comments_bom_and_bare_words() {
        let v = parse("\u{feff}// hi\n\"a\" { \"k\" \"say \\\"x\\\"\\n\" bare 5 } \"z\" \"1\"")
            .unwrap();
        assert_eq!(
            v.get("a").unwrap().get("k").unwrap().as_str(),
            Some("say \"x\"\n")
        );
        assert_eq!(v.get("a").unwrap().get("bare").unwrap().as_str(), Some("5"));
        assert_eq!(v.get("z").unwrap().as_str(), Some("1"));
        assert_eq!(parse("").unwrap().pairs().len(), 0);
    }

    #[test]
    fn malformed_input_is_none_not_a_panic() {
        for bad in [
            "\"a\" {",
            "\"a\" { \"b\" }",
            "}",
            "{",
            "\"a\"",
            "\"unterminated",
            "\"a\" \"b\\",
            "/ x",
            "\"a\" { \"b\" { } } }",
        ] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
        let deep = "\"a\" {".repeat(100) + &"}".repeat(100);
        assert!(parse(&deep).is_none());
    }
}
