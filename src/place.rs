//! Where the cursor is in a TOML document being typed, which may not
//! parse: typing a key (of which table, in a header or not) or a value (at
//! which path). The text is read from the start up to the cursor by a
//! small lexer that tolerates anything unfinished.

use flareite_extension_schemas::document::Segment;
use flareite_extension_schemas::{Header, Place};

/// Longest document read for context.
const MAX_SCAN: usize = 8 * 1024 * 1024;

/// The span of a key segment starting at `start`: a quoted string, or a
/// bare key.
fn key_end(text: &str, start: usize, json: bool) -> usize {
  let bytes = text.as_bytes();
  match bytes.get(start) {
    Some(&quote @ (b'"' | b'\'')) => {
      let mut i = start + 1;
      while i < bytes.len() && bytes[i] != b'\n' {
        if bytes[i] == b'\\' && quote == b'"' {
          i += 2;
          continue;
        }
        if bytes[i] == quote {
          return i + 1;
        }
        i += 1;
      }
      i.min(bytes.len())
    }
    _ if json => start,
    _ => {
      let mut i = start;
      while i < bytes.len() && is_bare(bytes[i]) {
        i += 1;
      }
      i
    }
  }
}

fn is_bare(b: u8) -> bool {
  b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Decodes a TOML key segment as written.
pub fn decode_key(raw: &str) -> String {
  if let Some(inner) = raw.strip_prefix('\'') {
    return inner.strip_suffix('\'').unwrap_or(inner).to_owned();
  }
  if raw.starts_with('"') {
    let complete = if raw.len() >= 2 && raw.ends_with('"') {
      raw.to_owned()
    } else {
      format!("{raw}\"")
    };
    if let Ok(toml::Value::String(key)) = format!("k = {complete}")
      .parse::<toml::Table>()
      .map(|table| table["k"].clone())
    {
      return key;
    }
    return raw.trim_matches('"').to_owned();
  }
  raw.to_owned()
}

/// What's being typed at `offset` in TOML `text`.
pub fn toml_place(text: &str, offset: usize) -> Place {
  let offset = offset.min(text.len()).min(MAX_SCAN);
  TomlLexer {
    text,
    bytes: text.as_bytes(),
    at: 0,
    cursor: offset,
    table: Vec::new(),
    arrays: Vec::new(),
  }
  .run()
}

struct TomlLexer<'a> {
  text: &'a str,
  bytes: &'a [u8],
  at: usize,
  cursor: usize,
  /// The table the last header opened.
  table: Vec<Segment>,
  /// Arrays of tables seen, by key path, and how many tables each has.
  arrays: Vec<(Vec<String>, usize)>,
}

/// What follows a value.
enum After {
  /// Another item or entry.
  More,
  /// The whole value, and its line.
  Ended,
  /// The cursor, where nothing can be completed.
  Cursor,
}

/// A value being read: an array's items, or an inline table's.
enum Frame {
  Array {
    index: usize,
  },
  Table {
    path: Vec<Segment>,
    keys: Vec<String>,
    value: bool,
  },
}

impl TomlLexer<'_> {
  fn peek(&self) -> Option<u8> {
    self.bytes.get(self.at).copied()
  }

  fn skip_spaces(&mut self) {
    while matches!(self.peek(), Some(b' ' | b'\t')) {
      self.at += 1;
    }
  }

  fn skip_line(&mut self) {
    while self.peek().is_some_and(|b| b != b'\n') {
      self.at += 1;
    }
  }

  /// The table path of a header's keys, with the index of each array of
  /// tables on the way.
  fn header_path(&self, keys: &[String], array: bool) -> Vec<Segment> {
    let mut path = Vec::new();
    for depth in 0..keys.len() {
      path.push(Segment::Key(keys[depth].clone()));
      let prefix = &keys[..=depth];
      let last = depth + 1 == keys.len();
      if last && array {
        let count = self
          .arrays
          .iter()
          .find(|(keys, _)| keys == prefix)
          .map_or(0, |(_, count)| *count);
        path.push(Segment::Index(count.saturating_sub(1)));
      } else if let Some((_, count)) = self.arrays.iter().find(|(keys, _)| keys == prefix) {
        path.push(Segment::Index(count.saturating_sub(1)));
      }
    }
    path
  }

  /// Reads a dotted key up to `stop` (`=`, `]`, ...). Returns its parts,
  /// or the place if the cursor is in it.
  fn dotted_key(&mut self, stops: &[u8]) -> Result<Vec<String>, (Vec<String>, usize, usize)> {
    let mut keys = Vec::new();
    loop {
      self.skip_spaces();
      let start = self.at;
      let end = key_end(self.text, start, false);
      if self.cursor >= start && self.cursor <= end.max(start) && self.cursor <= self.text.len() {
        // In this part, or right where it would start.
        let typed_end = if self.cursor == start && end == start {
          start
        } else {
          end
        };
        return Err((keys, start, typed_end));
      }
      if end == start {
        // Not a key character.
        return Ok(keys);
      }
      keys.push(decode_key(&self.text[start..end]));
      self.at = end;
      self.skip_spaces();
      match self.peek() {
        Some(b'.') => {
          self.at += 1;
          if self.cursor == self.at {
            return Err((keys, self.at, self.at));
          }
        }
        Some(b) if stops.contains(&b) => return Ok(keys),
        _ => return Ok(keys),
      }
    }
  }

  fn run(&mut self) -> Place {
    loop {
      // At the start of a line.
      self.skip_spaces();
      if self.at >= self.cursor {
        // An empty line, or before a key on it.
        let end = key_end(self.text, self.cursor, false);
        return Place::Key {
          table: self.table.clone(),
          start: self.cursor,
          end,
          header: Header::None,
        };
      }
      match self.peek() {
        None => return Place::Nothing,
        Some(b'\n' | b'\r') => {
          self.at += 1;
        }
        Some(b'#') => {
          self.skip_line();
          if self.at >= self.cursor {
            return Place::Nothing;
          }
        }
        Some(b'[') => {
          self.at += 1;
          let array = self.peek() == Some(b'[');
          if array {
            self.at += 1;
          }
          let header = if array {
            Header::ArrayOfTables
          } else {
            Header::Table
          };
          match self.dotted_key(b"]") {
            Err((keys, start, end)) => {
              return Place::Key {
                table: self.header_path(&keys, false),
                start,
                end,
                header,
              };
            }
            Ok(keys) => {
              if array {
                match self.arrays.iter_mut().find(|(known, _)| *known == keys) {
                  Some((_, count)) => *count += 1,
                  None => self.arrays.push((keys.clone(), 1)),
                }
                // Tables under an earlier table of the array are gone.
                self
                  .arrays
                  .retain(|(known, _)| !(known.len() > keys.len() && known.starts_with(&keys)));
              }
              self.table = self.header_path(&keys, array);
              self.skip_line();
              if self.at >= self.cursor {
                return Place::Nothing;
              }
            }
          }
        }
        Some(_) => {
          let keys = match self.dotted_key(b"=") {
            Err((keys, start, end)) => {
              let mut table = self.table.clone();
              table.extend(keys.into_iter().map(Segment::Key));
              return Place::Key {
                table,
                start,
                end,
                header: Header::None,
              };
            }
            Ok(keys) => keys,
          };
          self.skip_spaces();
          if self.peek() != Some(b'=') {
            // Not a key/value line; skip it.
            self.skip_line();
            if self.at >= self.cursor {
              return Place::Nothing;
            }
            continue;
          }
          self.at += 1;
          let mut path = self.table.clone();
          path.extend(keys.into_iter().map(Segment::Key));
          if let Some(place) = self.value(path) {
            return place;
          }
        }
      }
    }
  }

  /// Reads a value at `path` (after `=`). Returns the place if the cursor
  /// is in it.
  fn value(&mut self, path: Vec<Segment>) -> Option<Place> {
    let mut frames: Vec<Frame> = Vec::new();
    // The path of the value about to be read.
    let mut current = path;
    loop {
      // Skip spaces, and line breaks and comments inside arrays.
      loop {
        self.skip_spaces();
        match self.peek() {
          Some(b'#') if self.cursor > self.at => {
            self.skip_line();
            if self.at >= self.cursor {
              return Some(Place::Nothing);
            }
          }
          Some(b'\n' | b'\r')
            if matches!(frames.last(), Some(Frame::Array { .. })) && self.at < self.cursor =>
          {
            self.at += 1;
          }
          _ => break,
        }
      }
      let in_table_key = matches!(frames.last(), Some(Frame::Table { value: false, .. }));
      if self.at >= self.cursor {
        // Nothing typed yet here.
        return Some(if in_table_key {
          let Some(Frame::Table { path, .. }) = frames.last() else {
            return Some(Place::Nothing);
          };
          Place::Key {
            table: path.clone(),
            start: self.cursor,
            end: key_end(self.text, self.cursor, false),
            header: Header::None,
          }
        } else {
          Place::Value {
            path: current,
            start: self.cursor,
            end: self.value_end(self.cursor),
          }
        });
      }
      if in_table_key {
        match self.peek() {
          Some(b'}') => {
            self.at += 1;
            frames.pop();
            match self.after_value(&mut frames, &mut current) {
              After::More => {}
              After::Ended => return None,
              After::Cursor => return Some(Place::Nothing),
            }
            continue;
          }
          Some(b',') => {
            self.at += 1;
            continue;
          }
          _ => {}
        }
        let base = match frames.last() {
          Some(Frame::Table { path, .. }) => path.clone(),
          _ => Vec::new(),
        };
        match self.dotted_key(b"=") {
          Err((keys, start, end)) => {
            let mut table = base;
            table.extend(keys.into_iter().map(Segment::Key));
            return Some(Place::Key {
              table,
              start,
              end,
              header: Header::None,
            });
          }
          Ok(keys) => {
            self.skip_spaces();
            if self.peek() == Some(b'=') {
              self.at += 1;
            } else if keys.is_empty() {
              // Garbage: give up on this value.
              self.skip_line();
              return (self.at >= self.cursor).then_some(Place::Nothing);
            }
            if let Some(Frame::Table {
              keys: seen, value, ..
            }) = frames.last_mut()
            {
              seen.extend(keys.iter().cloned());
              *value = true;
            }
            current = base;
            current.extend(keys.into_iter().map(Segment::Key));
            continue;
          }
        }
      }
      // A value.
      let start = self.at;
      match self.peek() {
        Some(b'[') => {
          self.at += 1;
          frames.push(Frame::Array { index: 0 });
          current.push(Segment::Index(0));
          continue;
        }
        Some(b'{') => {
          self.at += 1;
          frames.push(Frame::Table {
            path: current.clone(),
            keys: Vec::new(),
            value: false,
          });
          continue;
        }
        Some(b']') if matches!(frames.last(), Some(Frame::Array { .. })) => {
          // An empty array, or a trailing comma.
          self.at += 1;
          frames.pop();
          current.pop();
          match self.after_value(&mut frames, &mut current) {
            After::More => {}
            After::Ended => return None,
            After::Cursor => return Some(Place::Nothing),
          }
          continue;
        }
        Some(b'"' | b'\'') => {
          let end = self.string_end(start);
          if self.cursor <= end && self.cursor > start {
            return Some(Place::Value {
              path: current,
              start,
              end,
            });
          }
          self.at = end;
        }
        Some(b'\n' | b'\r') | None => {
          return (self.at >= self.cursor).then_some(Place::Value {
            path: current,
            start: self.cursor,
            end: self.cursor,
          });
        }
        Some(_) => {
          // A number, boolean, or date: up to a delimiter.
          let end = self.value_end(start);
          if self.cursor <= end {
            return Some(Place::Value {
              path: current,
              start,
              end,
            });
          }
          self.at = end.max(start + 1);
          while !self.text.is_char_boundary(self.at) {
            self.at += 1;
          }
        }
      }
      match self.after_value(&mut frames, &mut current) {
        After::More => {}
        After::Ended => return None,
        After::Cursor => return Some(Place::Nothing),
      }
    }
  }

  /// After a value: reads a separator or a closing bracket.
  fn after_value(&mut self, frames: &mut Vec<Frame>, current: &mut Vec<Segment>) -> After {
    loop {
      if frames.is_empty() {
        // The rest of the line: a comment or nothing.
        self.skip_line();
        return if self.at >= self.cursor {
          After::Cursor
        } else {
          After::Ended
        };
      }
      self.skip_spaces();
      if matches!(frames.last(), Some(Frame::Array { .. })) {
        while matches!(self.peek(), Some(b'\n' | b'\r' | b' ' | b'\t' | b'#'))
          && self.at < self.cursor
        {
          if self.peek() == Some(b'#') {
            self.skip_line();
          } else {
            self.at += 1;
          }
        }
      }
      if self.at >= self.cursor {
        // Right after a value, where a separator goes.
        return After::Cursor;
      }
      match (self.peek(), frames.last_mut()) {
        (Some(b','), Some(Frame::Array { index, .. })) => {
          self.at += 1;
          *index += 1;
          let next = *index;
          if let Some(Segment::Index(last)) = current.last_mut() {
            *last = next;
          }
          return After::More;
        }
        (Some(b','), Some(Frame::Table { path, value, .. })) => {
          self.at += 1;
          *value = false;
          *current = path.clone();
          return After::More;
        }
        (Some(b']'), Some(Frame::Array { .. })) => {
          self.at += 1;
          frames.pop();
          current.pop();
        }
        (Some(b'}'), Some(Frame::Table { path, .. })) => {
          self.at += 1;
          *current = path.clone();
          frames.pop();
        }
        _ => {
          // Something unexpected: give up on this value.
          self.skip_line();
          return if self.at >= self.cursor {
            After::Cursor
          } else {
            After::Ended
          };
        }
      }
    }
  }

  /// Where a string starting at `start` ends (after its closing quote).
  fn string_end(&self, start: usize) -> usize {
    let bytes = self.bytes;
    let quote = bytes[start];
    let triple = bytes.get(start..start + 3) == Some(&[quote, quote, quote][..]);
    let mut i = start + if triple { 3 } else { 1 };
    while i < bytes.len() {
      let b = bytes[i];
      if b == b'\\' && quote == b'"' {
        i += 2;
        continue;
      }
      if b == b'\n' && !triple {
        return i;
      }
      if b == quote {
        if !triple {
          return i + 1;
        }
        if bytes.get(i..i + 3) == Some(&[quote, quote, quote][..]) {
          // Up to two more quotes may end the string's content.
          let mut end = i + 3;
          while end < bytes.len() && bytes[end] == quote && end < i + 5 {
            end += 1;
          }
          return end;
        }
      }
      i += 1;
    }
    bytes.len()
  }

  /// Where a bare value (number, boolean, date) starting at `start` ends.
  fn value_end(&self, start: usize) -> usize {
    let mut i = start;
    while i < self.bytes.len()
      && !matches!(
        self.bytes[i],
        b',' | b']' | b'}' | b'\n' | b'\r' | b'#' | b' ' | b'\t'
      )
    {
      i += 1;
    }
    // A date and time may have a space between them.
    i
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn at(text: &str) -> (String, usize) {
    let offset = text.find('|').expect("cursor");
    (text.replacen('|', "", 1), offset)
  }

  fn toml(text: &str) -> Place {
    let (text, offset) = at(text);
    toml_place(&text, offset)
  }

  fn keys(path: &[&str]) -> Vec<Segment> {
    path.iter().map(|key| Segment::key(*key)).collect()
  }

  #[test]
  fn toml_keys_in_tables_and_headers() {
    assert!(
      matches!(toml("|"), Place::Key { table, start: 0, header: Header::None, .. } if table.is_empty())
    );
    assert!(matches!(
      toml("[editor]\ntab|"),
      Place::Key { table, start: 9, end: 12, header: Header::None } if table == keys(&["editor"])
    ));
    assert!(matches!(
      toml("[editor]\nminimap.|"),
      Place::Key { table, header: Header::None, .. } if table == keys(&["editor", "minimap"])
    ));
    assert!(matches!(
      toml("[editor]\n[work|"),
      Place::Key { table, start: 10, header: Header::Table, .. } if table.is_empty()
    ));
    assert!(matches!(
      toml("[extensions.settings.\"example.|tidy\"]"),
      Place::Key { table, start: 21, end: 35, header: Header::Table } if table == keys(&["extensions", "settings"])
    ));
    assert!(matches!(
      toml("[[products]]\nname = 1\n[[products]]\n[products.size]\n|"),
      Place::Key { table, .. } if table == vec![Segment::key("products"), Segment::Index(1), Segment::key("size")]
    ));
    assert!(matches!(
      toml("[[pro|"),
      Place::Key {
        header: Header::ArrayOfTables,
        ..
      }
    ));
    assert_eq!(toml("# a comment |"), Place::Nothing);
    assert!(matches!(toml("a = 1 # |"), Place::Nothing));
  }

  #[test]
  fn toml_values() {
    assert!(matches!(
      toml("[editor]\ntab_size = |"),
      Place::Value { path, start: 20, .. } if path == keys(&["editor", "tab_size"])
    ));
    assert!(matches!(
      toml("theme = \"da|rk\""),
      Place::Value { path, start: 8, end: 14 } if path == keys(&["theme"])
    ));
    assert!(matches!(
      toml("flag = tr|"),
      Place::Value { path, start: 7, .. } if path == keys(&["flag"])
    ));
    assert!(matches!(
      toml("exclude = [\n  \".git\",\n  |\n]"),
      Place::Value { path, .. } if path == vec![Segment::key("exclude"), Segment::Index(1)]
    ));
    assert!(matches!(
      toml("server = { port = 1, ho|"),
      Place::Key { table, .. } if table == keys(&["server"])
    ));
    assert!(matches!(
      toml("server = { port = |"),
      Place::Value { path, .. } if path == keys(&["server", "port"])
    ));
    assert!(matches!(
      toml("s = \"\"\"\nmulti\n\"\"\"\nnext|"),
      Place::Key { table, .. } if table.is_empty()
    ));
    // A finished line moves on.
    assert!(matches!(
      toml("a = [1, 2]\n|"),
      Place::Key { table, .. } if table.is_empty()
    ));
  }
}
