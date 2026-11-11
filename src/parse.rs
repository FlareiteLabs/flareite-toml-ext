//! TOML documents as trees of values with where each is, from the `toml`
//! crate's spans, so Flareite can mark a problem on the value or key it's
//! about. Recovers from syntax errors, keeping what it could read.

use flareite_extension_schemas::document::{Document, Node, Part, Problem, Scalar, Severity, Span};
use flareite_extension_schemas::widen;
use toml::de::{DeTable, DeValue};

/// Parses a TOML document.
pub fn parse(text: &str) -> Document {
  let (parsed, errors) = DeTable::parse_recoverable(text);
  let tree = Node::table(0..text.len(), entries(parsed.get_ref()));
  let problems = errors
    .into_iter()
    .map(|error| Problem {
      span: widen(text, error.span().unwrap_or(0..0)),
      severity: Severity::Error,
      message: sentence(error.message()),
    })
    .collect();
  Document {
    parts: vec![Part {
      tree,
      span: 0..text.len(),
    }],
    named_schema: named_schema(text, parsed.get_ref()),
    problems,
  }
}

/// A message as a sentence.
fn sentence(message: &str) -> String {
  let mut message = message.trim().to_owned();
  if let Some(first) = message.get(..1) {
    let upper = first.to_uppercase();
    message.replace_range(..1, &upper);
  }
  message
}

fn node(value: &DeValue<'_>, span: Span) -> Node {
  let scalar = |value| Node::scalar(span.clone(), value);
  match value {
    DeValue::Table(table) => Node::table(span, entries(table)),
    DeValue::Array(items) => Node::array(
      span,
      items
        .iter()
        .map(|item| node(item.get_ref(), item.span()))
        .collect(),
    ),
    DeValue::String(s) => scalar(Some(Scalar::String(s.to_string()))),
    DeValue::Integer(i) => scalar(
      i64::from_str_radix(i.as_str(), i.radix())
        .ok()
        .map(Scalar::Integer),
    ),
    DeValue::Float(f) => scalar(f.as_str().parse::<f64>().ok().map(Scalar::Float)),
    DeValue::Boolean(b) => scalar(Some(Scalar::Bool(*b))),
    DeValue::Datetime(d) => scalar(Some(Scalar::Datetime(d.to_string()))),
  }
}

fn entries(table: &DeTable<'_>) -> Vec<(String, Node)> {
  table
    .iter()
    .map(|(key, value)| {
      let child = node(value.get_ref(), value.span()).keyed(key.span());
      (key.get_ref().to_string(), child)
    })
    .collect()
}

/// The schema a TOML document names: a `#:schema <address>` comment before
/// its first key (as Taplo reads it), or a TOML Schema's `[toml-schema]
/// location`.
fn named_schema(text: &str, table: &DeTable<'_>) -> Option<String> {
  for line in text.lines() {
    let line = line.trim();
    if let Some(address) = line.strip_prefix("#:schema") {
      let address = address.trim();
      return (!address.is_empty()).then(|| address.to_owned());
    }
    if !(line.is_empty() || line.starts_with('#')) {
      break;
    }
  }
  let header = table.get("toml-schema")?;
  let DeValue::Table(header) = header.get_ref() else {
    return None;
  };
  match header.get("location")?.get_ref() {
    DeValue::String(location) => Some(location.to_string()),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use flareite_extension_schemas::document::Segment;

  use super::*;

  #[test]
  fn trees_know_where_keys_and_values_are() {
    let text = "a = 1\n[t]\nb = \"x\"\n[[arr]]\nc = true\n[[arr]]\nc = false\n";
    let document = parse(text);
    assert!(document.problems.is_empty());
    let tree = &document.parts[0].tree;
    let (b, taken) = tree.find(&[Segment::key("t"), Segment::key("b")]);
    assert_eq!(taken, 2);
    assert_eq!(&text[b.span.clone()], "\"x\"");
    assert_eq!(&text[b.key_span.clone().expect("key")], "b");
    assert_eq!(b.value(), Some(&Scalar::String("x".into())));
    let (c, _) = tree.find(&[Segment::key("arr"), Segment::Index(1), Segment::key("c")]);
    assert_eq!(&text[c.span.clone()], "false");
    let (t, _) = tree.find(&[Segment::key("t")]);
    assert_eq!(&text[t.key_span.clone().expect("key")], "t");
    // A missing key finds its table.
    let (_, taken) = tree.find(&[Segment::key("t"), Segment::key("zz")]);
    assert_eq!(taken, 1);
  }

  #[test]
  fn values_are_typed() {
    let text = "i = 0x1F\nn = -3_000\nf = 1.5e3\ng = inf\nd = 1979-05-27\nb = false\n";
    let tree = parse(text).parts.remove(0).tree;
    let value = |key: &str| tree.get(key).and_then(Node::value).cloned();
    assert_eq!(value("i"), Some(Scalar::Integer(31)));
    assert_eq!(value("n"), Some(Scalar::Integer(-3000)));
    assert_eq!(value("f"), Some(Scalar::Float(1500.0)));
    assert_eq!(value("g"), Some(Scalar::Float(f64::INFINITY)));
    assert_eq!(value("d"), Some(Scalar::Datetime("1979-05-27".into())));
    assert_eq!(value("b"), Some(Scalar::Bool(false)));
  }

  #[test]
  fn syntax_errors_are_placed_and_recovered_from() {
    let document = parse("a = \nb = 2\n");
    assert_eq!(document.problems.len(), 1, "{:?}", document.problems);
    assert!(document.has_errors());
    assert!(document.parts[0].tree.get("b").is_some(), "recovers");
  }

  #[test]
  fn documents_name_their_schemas() {
    let named = |text: &str| parse(text).named_schema;
    assert_eq!(
      named("# x\n#:schema ./a.tosd\nname = 1"),
      Some("./a.tosd".into())
    );
    assert_eq!(named("name = 1\n#:schema ./a.tosd"), None);
    assert_eq!(
      named("[toml-schema]\nlocation = \"c.tosd\"\n"),
      Some("c.tosd".into())
    );
  }
}
