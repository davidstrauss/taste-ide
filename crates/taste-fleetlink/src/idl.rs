//! Just enough varlink IDL to keep the interface file honest.
//!
//! The `.varlink` file is the interface's source of truth — it is what
//! `GetInterfaceDescription` hands a client, and what a GJS or Python
//! client will read to learn the shape of a row. A checked-in description
//! that has quietly drifted from the structs the service actually
//! serialises is worse than none, so this module parses it and the tests
//! assert the two agree, field for field.
//!
//! Deliberately a *subset*: named types, methods with named parameters,
//! comments, and nothing else. Varlink also has enums, inline anonymous
//! structs, maps, foreign type references and `error` declarations; this
//! interface uses none of them, and a parser that accepted more would be
//! code with no caller. Anything outside the subset is a parse error, on
//! purpose — that is what makes it a check.

use std::collections::BTreeMap;

/// A parsed interface description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    /// Declaration order preserved for rendering; lookup is by name.
    pub types: Vec<Struct>,
    pub methods: Vec<Method>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Struct {
    pub name: String,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub name: String,
    pub parameters: Vec<Field>,
    pub results: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    /// The type as written: `int`, `?string`, `[]Row`, `Spend`.
    pub type_name: String,
}

impl Interface {
    pub fn type_named(&self, name: &str) -> Option<&Struct> {
        self.types.iter().find(|item| item.name == name)
    }

    pub fn method_named(&self, name: &str) -> Option<&Method> {
        self.methods.iter().find(|item| item.name == name)
    }

    /// Every method's fully qualified name, as a call would spell it.
    pub fn qualified_methods(&self) -> Vec<String> {
        self.methods
            .iter()
            .map(|method| format!("{}.{}", self.name, method.name))
            .collect()
    }
}

impl Struct {
    pub fn field_names(&self) -> Vec<&str> {
        self.fields.iter().map(|f| f.name.as_str()).collect()
    }
}

/// Render an interface back to IDL. Normalized (no comments, one field per
/// line) — this exists so a parse can be checked by round-tripping, not to
/// regenerate the checked-in file.
pub fn render(interface: &Interface) -> String {
    fn fields(list: &[Field]) -> String {
        if list.is_empty() {
            return "()".to_string();
        }
        let body: Vec<String> = list
            .iter()
            .map(|field| format!("  {}: {}", field.name, field.type_name))
            .collect();
        format!("(\n{}\n)", body.join(",\n"))
    }
    let mut out = format!("interface {}\n", interface.name);
    for item in &interface.types {
        out.push_str(&format!("\ntype {} {}\n", item.name, fields(&item.fields)));
    }
    for method in &interface.methods {
        out.push_str(&format!(
            "\nmethod {}{} -> {}\n",
            method.name,
            fields(&method.parameters),
            fields(&method.results)
        ));
    }
    out
}

/// Parse an interface description.
pub fn parse(source: &str) -> Result<Interface, String> {
    let tokens = tokenize(source);
    let mut cursor = Cursor {
        tokens: &tokens,
        at: 0,
    };
    cursor.expect_word("interface")?;
    let name = cursor.identifier()?;
    if !name.contains('.') {
        return Err(format!(
            "interface name {name:?} is not a reverse-domain name"
        ));
    }
    let mut types = Vec::new();
    let mut methods = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    while let Some(keyword) = cursor.peek().map(str::to_string) {
        match keyword.as_str() {
            "type" => {
                cursor.at += 1;
                let name = cursor.identifier()?;
                let fields = cursor.fields()?;
                if seen.insert(format!("type {name}"), ()).is_some() {
                    return Err(format!("type {name} is declared twice"));
                }
                types.push(Struct { name, fields });
            }
            "method" => {
                cursor.at += 1;
                let name = cursor.identifier()?;
                let parameters = cursor.fields()?;
                cursor.expect_word("->")?;
                let results = cursor.fields()?;
                if seen.insert(format!("method {name}"), ()).is_some() {
                    return Err(format!("method {name} is declared twice"));
                }
                methods.push(Method {
                    name,
                    parameters,
                    results,
                });
            }
            other => return Err(format!("expected `type` or `method`, found {other:?}")),
        }
    }
    let interface = Interface {
        name,
        types,
        methods,
    };
    // Every type a field mentions must exist: a reference to a type nobody
    // declared is exactly the drift this module is here to catch.
    let declared: Vec<&str> = interface.types.iter().map(|t| t.name.as_str()).collect();
    for field in interface
        .types
        .iter()
        .flat_map(|item| item.fields.iter())
        .chain(
            interface
                .methods
                .iter()
                .flat_map(|method| method.parameters.iter().chain(method.results.iter())),
        )
    {
        let bare = field
            .type_name
            .trim_start_matches('?')
            .trim_start_matches("[]");
        if !matches!(bare, "int" | "float" | "bool" | "string" | "object")
            && !declared.contains(&bare)
        {
            return Err(format!(
                "field {} has undeclared type {:?}",
                field.name, field.type_name
            ));
        }
    }
    Ok(interface)
}

struct Cursor<'a> {
    tokens: &'a [String],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.at).map(String::as_str)
    }

    fn bump(&mut self) -> Result<&'a str, String> {
        let token = self
            .tokens
            .get(self.at)
            .ok_or_else(|| "unexpected end of interface".to_string())?;
        self.at += 1;
        Ok(token.as_str())
    }

    fn expect_word(&mut self, word: &str) -> Result<(), String> {
        let token = self.bump()?;
        if token == word {
            Ok(())
        } else {
            Err(format!("expected {word:?}, found {token:?}"))
        }
    }

    fn identifier(&mut self) -> Result<String, String> {
        let token = self.bump()?.to_string();
        let ok = token
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        if ok {
            Ok(token)
        } else {
            Err(format!("{token:?} is not an identifier"))
        }
    }

    /// `( name: type, … )`, possibly empty.
    fn fields(&mut self) -> Result<Vec<Field>, String> {
        self.expect_word("(")?;
        let mut fields = Vec::new();
        if self.peek() == Some(")") {
            self.at += 1;
            return Ok(fields);
        }
        loop {
            let name = self.identifier()?;
            self.expect_word(":")?;
            let type_name = self.bump()?.to_string();
            fields.push(Field { name, type_name });
            match self.bump()? {
                "," => continue,
                ")" => break,
                other => return Err(format!("expected `,` or `)`, found {other:?}")),
            }
        }
        Ok(fields)
    }
}

/// Words, and the four structural tokens. `#` runs to end of line.
fn tokenize(source: &str) -> Vec<String> {
    // Type names carry their modifiers inline (`?[]Row`), so brackets and
    // the option mark are word characters, not punctuation.
    fn is_word(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '?' | '[' | ']')
    }
    let mut tokens = Vec::new();
    for line in source.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut chars = line.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
            } else if is_word(c) {
                let mut word = String::new();
                while chars.peek().copied().is_some_and(is_word) {
                    word.push(chars.next().unwrap());
                }
                tokens.push(word);
            } else if c == '-' {
                chars.next();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    tokens.push("->".to_string());
                } else {
                    tokens.push("-".to_string());
                }
            } else {
                chars.next();
                tokens.push(c.to_string());
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# A comment, and a # inside one.
interface org.example.Thing

type Pair (
  left: string,   # trailing comment
  right: ?int
)

method Get() -> (pairs: []Pair)
method Put(pair: Pair, force: bool) -> ()
"#;

    #[test]
    fn the_subset_parses_and_round_trips() {
        let parsed = parse(SAMPLE).unwrap();
        assert_eq!(parsed.name, "org.example.Thing");
        assert_eq!(
            parsed.type_named("Pair").unwrap().field_names(),
            ["left", "right"]
        );
        assert_eq!(
            parsed.type_named("Pair").unwrap().fields[1].type_name,
            "?int"
        );
        assert!(parsed.method_named("Get").unwrap().parameters.is_empty());
        assert_eq!(
            parsed.method_named("Get").unwrap().results[0].type_name,
            "[]Pair"
        );
        assert!(parsed.method_named("Put").unwrap().results.is_empty());
        assert_eq!(
            parsed.qualified_methods(),
            ["org.example.Thing.Get", "org.example.Thing.Put"]
        );
        // Rendering drops comments and formatting; parsing it back must
        // yield the same interface, or the parser is losing something.
        assert_eq!(parse(&render(&parsed)).unwrap(), parsed);
    }

    #[test]
    fn drift_and_malformed_input_are_errors_not_shrugs() {
        assert!(parse("interface nodots\n").is_err());
        assert!(parse("type Pair (a: int)\n").is_err(), "no interface line");
        assert!(
            parse("interface a.b\ntype P (x: Nope)\n").is_err(),
            "a reference to an undeclared type is the drift we are hunting"
        );
        assert!(
            parse("interface a.b\ntype P (x: int)\ntype P (y: int)\n").is_err(),
            "a duplicate declaration hides one of them"
        );
        assert!(parse("interface a.b\ntype P (x int)\n").is_err());
        assert!(
            parse("interface a.b\nenum E (a, b)\n").is_err(),
            "outside the subset"
        );
    }
}
