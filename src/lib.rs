//! TOML for Flareite: TOML documents parsed for Flareite's schemas, which
//! check them, and their keys and values completed and described from what
//! the schemas say, written as TOML writes them.

mod parse;
mod place;

use flareite_extension_api::events::Event;
use flareite_extension_api::language_features::{CompletionItem, CompletionRequest, HoverRequest};
use flareite_extension_api::types::Activation;
use flareite_extension_api::{Extension, register};
use flareite_extension_schemas::document::Document;
use flareite_extension_schemas::{Documents, Header, Language, Place, Property, Shape};
use serde_json::Value as Json;

/// TOML, as Flareite's schemas see it.
pub struct Toml;

impl Language for Toml {
  fn languages(&self) -> &[&str] {
    &["toml"]
  }

  fn parse(&self, text: &str) -> Document {
    parse::parse(text)
  }

  fn place(&self, text: &str, offset: usize) -> Place {
    place::toml_place(text, offset)
  }

  fn decode_key(&self, raw: &str) -> String {
    place::decode_key(raw)
  }

  fn key(&self, name: &str) -> String {
    key(name)
  }

  fn insert_property(
    &self,
    key: &str,
    property: &Property,
    header: Header,
    _text: &str,
    _start: usize,
  ) -> String {
    match (header, property.shape) {
      (Header::None, Shape::Value | Shape::Array) => format!("{key} = "),
      // A table's or an array of tables' key, for a header or a dotted key.
      _ => key.to_owned(),
    }
  }

  fn render(&self, value: &Json) -> String {
    render(value)
  }
}

/// A TOML basic string.
pub fn string(s: &str) -> String {
  let mut out = String::with_capacity(s.len() + 2);
  out.push('"');
  for c in s.chars() {
    match c {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\t' => out.push_str("\\t"),
      '\r' => out.push_str("\\r"),
      c if u32::from(c) < 0x20 || c == '\u{7f}' => {
        out.push_str(&format!("\\u{:04X}", u32::from(c)))
      }
      c => out.push(c),
    }
  }
  out.push('"');
  out
}

/// A TOML key: bare if it can be, else quoted.
pub fn key(key: &str) -> String {
  if !key.is_empty()
    && key
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
  {
    key.to_owned()
  } else {
    string(key)
  }
}

/// A value as TOML writes it, on one line.
pub fn render(value: &Json) -> String {
  match value {
    Json::Null => "null".to_owned(),
    Json::Bool(b) => b.to_string(),
    Json::Number(n) => n.to_string(),
    Json::String(s) => string(s),
    Json::Array(items) => format!(
      "[{}]",
      items.iter().map(render).collect::<Vec<_>>().join(", ")
    ),
    Json::Object(map) => {
      if map.is_empty() {
        return "{}".to_owned();
      }
      format!(
        "{{ {} }}",
        map
          .iter()
          .map(|(name, value)| format!("{} = {}", key(name), render(value)))
          .collect::<Vec<_>>()
          .join(", ")
      )
    }
  }
}

/// The extension: its open documents, checked against their schemas.
struct TomlExtension(Documents<Toml>);

impl Extension for TomlExtension {
  fn activate(_: Activation) -> Result<Self, String> {
    Ok(Self(Documents::new(Toml)))
  }

  fn handle_event(&mut self, event: Event) -> Result<(), String> {
    self.0.handle_event(event);
    Ok(())
  }

  fn provide_completions(
    &mut self,
    request: CompletionRequest,
  ) -> Result<Vec<CompletionItem>, String> {
    Ok(self.0.completions(request.document, request.position))
  }

  fn provide_hover(&mut self, request: HoverRequest) -> Result<Option<String>, String> {
    Ok(self.0.hover(request.document, request.position))
  }
}

register!(TomlExtension);

#[cfg(test)]
mod tests {
  use flareite_extension_schemas::testing::{self, Schema, Severity};
  use flareite_extension_schemas::{Completion, CompletionKind};
  use serde_json::json;

  use super::*;

  const TOSD: &str = r#"
[toml-schema]
version = "1.0.0"

[elements.name]
type = "string"
description = "The package's name."

[elements.edition]
type = "string"
allowedvalues = ["2021", "2024"]
optional = true

[elements.server]
type = "table"
optional = true
[elements.server.port]
type = "integer"
min = 1
"#;

  fn tosd() -> Schema {
    testing::toml_schema(TOSD)
  }

  /// `text` with `|` for the cursor.
  fn complete(text: &str, schema: &Schema) -> Vec<Completion> {
    let offset = text.find('|').expect("cursor");
    let text = text.replacen('|', "", 1);
    testing::complete(&Toml, &text, offset, &[schema])
  }

  #[test]
  fn documents_are_checked_where_problems_are() {
    let schema = tosd();
    let text = "edition = \"2018\"\nextra = 1\n[server]\nport = 0\n";
    let problems = testing::check(&Toml, text, &[&schema]);
    let found: Vec<(&str, Option<&str>)> = problems
      .iter()
      .map(|d| (&text[d.range.clone()], d.code.as_deref()))
      .collect();
    assert!(
      found.contains(&("\"2018\"", Some("allowedvalues"))),
      "{problems:?}"
    );
    assert!(
      found.contains(&("extra", Some("unknown-key"))),
      "{problems:?}"
    );
    assert!(found.contains(&("0", Some("min"))), "{problems:?}");
    // A missing key is marked at the document's first line.
    assert!(
      found.contains(&("edition = \"2018\"", Some("missing-required"))),
      "{problems:?}"
    );
    // Syntax errors come alone.
    let broken = testing::check(&Toml, "name = \n", &[&schema]);
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].code, None);
  }

  #[test]
  fn json_schemas_check_toml_documents() {
    let schema = testing::json_schema(json!({
      "type": "object",
      "properties": {
        "port": { "type": "integer" },
        "old": { "type": "boolean", "deprecated": true }
      },
      "additionalProperties": false
    }));
    let text = "port = \"x\"\nold = true\nzzz = 1\n";
    let problems = testing::check(&Toml, text, &[&schema]);
    let marked: Vec<&str> = problems.iter().map(|d| &text[d.range.clone()]).collect();
    assert_eq!(marked, ["\"x\"", "old", "zzz"], "{problems:?}");
    assert_eq!(problems[1].severity, Severity::Warning);
  }

  #[test]
  fn completes_keys_and_values() {
    let schema = tosd();
    let items = complete("name = \"x\"\n|", &schema);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["edition", "server"], "present keys are left out");
    assert_eq!(items[0].insert, "edition = ");
    assert_eq!(items[1].insert, "server");
    assert_eq!(items[1].kind, CompletionKind::Table);
    let headers = complete("[|", &schema);
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].label, "server");
    let values = complete("edition = |", &schema);
    let values: Vec<&str> = values.iter().map(|i| i.insert.as_str()).collect();
    assert_eq!(values, ["\"2021\"", "\"2024\""]);
    let ports = complete("[server]\n|", &schema);
    assert_eq!(ports[0].detail.as_deref(), Some("integer, required"));
    assert!(ports[0].sort.starts_with('0'), "required keys first");
  }

  #[test]
  fn hovers_describe_keys_and_values() {
    let schema = tosd();
    let text = "name = \"x\"\n[server]\nport = 8080\n";
    let (markdown, span) = testing::hover(&Toml, text, 2, &[&schema]).expect("hover");
    assert_eq!(&text[span], "name");
    assert!(markdown.starts_with("The package's name."), "{markdown}");
    assert!(markdown.ends_with("*From test*"), "{markdown}");
    let offset = text.find("8080").expect("port") + 1;
    let (markdown, _) = testing::hover(&Toml, text, offset, &[&schema]).expect("hover");
    assert!(markdown.contains("At least 1"), "{markdown}");
    assert!(testing::hover(&Toml, "zzz = 1", 1, &[&schema]).is_none());
  }

  #[test]
  fn values_are_written_as_toml() {
    assert_eq!(render(&json!("a\"b")), "\"a\\\"b\"");
    assert_eq!(
      render(&json!({"x y": [1, true]})),
      "{ \"x y\" = [1, true] }"
    );
    assert_eq!(key("tab_size"), "tab_size");
    assert_eq!(key("example.tidy"), "\"example.tidy\"");
  }
}
