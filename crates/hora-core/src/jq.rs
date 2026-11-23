//! jq expressions for the `jq` body assertion: compiled once at load with
//! jaq, shared by every monitor that spells the same expression, and run
//! without environment, filesystem or module access.
//!
//! The expression is configuration, trusted like an exec monitor's command;
//! the body it runs on is not. What the body controls is bounded: its size
//! (`max_body_kb`), its nesting (`serde_json` stops at 128 levels, here and in
//! `fromjson`), and the outputs pulled (two at most).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use jaq_core::load::lex::{StrPart, Tok, Token};
use jaq_core::load::{Arena, File, Lexer, Loader};
use jaq_core::{Ctx, Cv, ValXs, Vars, data, native};
use jaq_json::Val;

use crate::config::ParseField;

type Data = data::JustLut<Val>;
type Filter = jaq_core::Filter<Data>;

// Monitors spelling the same expression share one compiled filter, across
// hot reloads too. The entries are weak, so a filter no monitor uses any more
// is freed; dead keys are pruned on a miss.
static PROGRAMS: LazyLock<Mutex<HashMap<String, Weak<Filter>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The definitions from jaq's standard library an expression may use: pure
/// functions on the input. Everything that reads the environment, the clock
/// or stdin, logs, halts or loads modules stays out.
const STD_DEFS: &[&str] = &[
    "null",
    "isboolean",
    "isnumber",
    "isstring",
    "isarray",
    "isobject",
    "nan",
    "infinite",
    "isnan",
    "isinfinite",
    "isfinite",
    "isnormal",
    "abs",
    "type",
    "values",
    "nulls",
    "booleans",
    "numbers",
    "finites",
    "normals",
    "strings",
    "arrays",
    "objects",
    "iterables",
    "scalars",
    "add",
    "min_by",
    "max_by",
    "min",
    "max",
    "unique_by",
    "unique",
    "pick",
    "keys",
    "flatten",
];

/// Native functions left out: `halt` would exit the process, and jaq's
/// `fromjson` (which `tonumber` calls) parses without a depth limit, so a
/// deeply nested string in the body would overflow the stack. It is
/// replaced by [`fromjson`].
const NATIVE_OUT: &[&str] = &["halt", "fromjson"];

/// `fromjson` with `serde_json`'s nesting limit.
fn fromjson(cv: Cv<'_, Data>) -> ValXs<'_, Val> {
    native::bome(match cv.1 {
        Val::TStr(text) => {
            serde_json::from_slice(&text).map_err(|_| jaq_core::Error::str("cannot parse as JSON"))
        }
        other => Err(jaq_core::Error::typ(other, "string")),
    })
}

/// The filters that go on for as long as they are told to, whatever the
/// body holds: with one of them an expression may never end, and nothing
/// stops an evaluation - it would hold its monitor's task, and a core, for
/// good. `recurse` alone and `..` walk the body and end with it.
const LOOPS: &[&str] = &["repeat", "until", "while", "range", "recurse"];

/// Why the tokens are refused, if they are: a function definition (a
/// recursive one can overflow the stack, which aborts the process rather
/// than failing the check), or a call to one of [`LOOPS`]. A call is the
/// word followed by its arguments: `{range: 1}` and `.range` name a field.
fn refused(tokens: &[Token<&str>]) -> Option<&'static str> {
    let mut next = tokens.iter().skip(1);
    tokens.iter().find_map(|Token(text, tok)| {
        let called = matches!(next.next(), Some(Token(_, Tok::Block(_))));
        match tok {
            Tok::Word if *text == "def" => Some("function definitions (def) are not available"),
            Tok::Word if called && LOOPS.contains(text) => Some(
                "repeat, until, while, range and recurse(f) are not available \
                 (an expression must end)",
            ),
            Tok::Block(inner) => refused(inner),
            Tok::Str(parts) => parts.iter().find_map(|part| match part {
                StrPart::Term(term) => refused(std::slice::from_ref(term)),
                StrPart::Str(_) | StrPart::Char(_) => None,
            }),
            _ => None,
        }
    })
}

/// A compiled jq filter (`jq`).
#[derive(Clone)]
pub struct JqProgram(Arc<Filter>);

impl ParseField for JqProgram {
    fn parse_field(raw: &str) -> Result<Self, String> {
        let mut programs = PROGRAMS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(program) = programs.get(raw).and_then(Weak::upgrade) {
            return Ok(Self(program));
        }
        programs.retain(|_, program| program.strong_count() > 0);
        if let Some(reason) = Lexer::new(raw)
            .lex()
            .ok()
            .and_then(|tokens| refused(&tokens))
        {
            return Err(reason.to_owned());
        }
        let defs = jaq_core::defs()
            .chain(jaq_std::defs().filter(|def| STD_DEFS.contains(&def.name)))
            .chain(jaq_json::defs());
        let funs = jaq_core::funs()
            .chain(jaq_std::base_funs())
            .chain(jaq_json::funs())
            .filter(|(name, ..)| !NATIVE_OUT.contains(name))
            .chain([native::run::<Data>((
                "fromjson",
                native::v(0),
                fromjson as jaq_core::RunPtr<Data>,
            ))]);
        let arena = Arena::default();
        let modules = Loader::new(defs)
            .load(
                &arena,
                File {
                    code: raw,
                    path: (),
                },
            )
            .map_err(|_| "invalid jq syntax (modules are not available)".to_owned())?;
        let filter = jaq_core::Compiler::default()
            .with_funs(funs)
            .compile(modules)
            .map_err(|_| "invalid jq expression or unavailable function".to_owned())?;
        let filter = Arc::new(filter);
        programs.insert(raw.to_owned(), Arc::downgrade(&filter));
        Ok(Self(filter))
    }
}

impl JqProgram {
    /// Whether both run the same compiled filter.
    #[cfg(test)]
    pub(crate) fn shares_filter_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Run the filter on a JSON body and return its one result as text: a
    /// string's content, a number or boolean as JSON writes it. Only the
    /// first two outputs are ever asked for, so a filter that streams many
    /// values is not collected.
    ///
    /// # Errors
    ///
    /// A reason that carries nothing from the body: the body is not JSON,
    /// the filter failed, or it produced no value, several values, or a
    /// `null`, array or object.
    pub fn evaluate(&self, body: &[u8]) -> Result<String, &'static str> {
        let input: Val = serde_json::from_slice(body).map_err(|_| "response is not valid JSON")?;
        let ctx = Ctx::<Data>::new(&self.0.lut, Vars::new([]));
        let mut outputs = self.0.id.run((ctx, input));
        let value = outputs
            .next()
            .ok_or("no result")?
            .map_err(|_| "evaluation failed")?;
        if outputs.next().is_some() {
            return Err("more than one result");
        }
        match value {
            Val::Bool(value) => Ok(value.to_string()),
            Val::Num(number) => Ok(number.to_string()),
            Val::TStr(text) | Val::BStr(text) => Ok(String::from_utf8_lossy(&text).into_owned()),
            Val::Null | Val::Arr(_) | Val::Obj(_) => {
                Err("result is not a string, number or boolean")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitors_spelling_the_same_expression_share_one_filter() {
        let first = JqProgram::parse_field(".shared == true").unwrap();
        let second = JqProgram::parse_field(".shared == true").unwrap();
        assert!(Arc::ptr_eq(&first.0, &second.0));
        let other = JqProgram::parse_field(".shared == false").unwrap();
        assert!(!Arc::ptr_eq(&first.0, &other.0));
    }

    #[test]
    fn results_read_as_text_and_anything_but_one_scalar_fails() {
        let run = |query: &str, body: &str| {
            JqProgram::parse_field(query)
                .unwrap()
                .evaluate(body.as_bytes())
        };
        assert_eq!(run(".a", r#"{"a":"up"}"#).as_deref(), Ok("up"));
        assert_eq!(run(".a", r#"{"a":2.5}"#).as_deref(), Ok("2.5"));
        assert_eq!(run(".a > 1", r#"{"a":2}"#).as_deref(), Ok("true"));
        for (query, body) in [
            (".missing", "{}"),
            (".[]", "[]"),
            (".[]", "[1,2]"),
            (".", "[1]"),
            (".", "{}"),
            ("tonumber", "\"bad\""),
            (".", "invalid"),
        ] {
            assert!(run(query, body).is_err(), "{query} on {body}");
        }
        // A long stream is cut after its second value.
        assert_eq!(run(".[]", "[1,2,3]"), Err("more than one result"));
        // What walks the body ends with it, and a field may bear any name.
        assert_eq!(run("[..] | length", "[[1],[2]]").as_deref(), Ok("5"));
        assert_eq!(run("[recurse] | length", "[[1]]").as_deref(), Ok("3"));
        assert_eq!(run(".range", r#"{"range":7}"#).as_deref(), Ok("7"));
        assert_eq!(
            run("{while: .until} | .while", r#"{"until":7}"#).as_deref(),
            Ok("7")
        );
        // A string nested deeper than the body may be fails, it does not
        // overflow the stack.
        let nested = format!(
            r#"{{"a":"{}1{}"}}"#,
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        assert!(run(".a | tonumber", &nested).is_err());
        assert!(run(".a | fromjson | length", &nested).is_err());
        assert_eq!(
            run(".a | fromjson | .b", r#"{"a":"{\"b\":3}"}"#).as_deref(),
            Ok("3")
        );
    }

    #[test]
    fn the_environment_files_and_modules_are_out_of_reach() {
        for query in [
            "env",
            "$ENV",
            "now",
            "input",
            "debug",
            "halt",
            "halt(1)",
            "halt_error",
            "def f: 1 + f; f",
            "[.[] | def f: .; f]",
            "\"\\(def f: .; f)\"",
            // Nothing that may never end: no evaluation is ever stopped.
            "until(false; .)",
            "last(repeat(1))",
            "repeat(1) | select(false)",
            "[range(1e12)] | length",
            "recurse(.) | select(false)",
            "[.[] | while(true; .)]",
            "\"\\(repeat(1))\"",
            "import \"secret\" as x; x::read",
            "include \"secret\"; .",
            "(",
        ] {
            assert!(JqProgram::parse_field(query).is_err(), "{query}");
        }
    }
}
