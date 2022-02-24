// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::{iter::FromIterator, str::FromStr};

#[derive(Clone, PartialEq, PartialOrd, Eq, Hash, Default)]
pub struct RouteAddress {
  segments: Vec<String>,
}

impl RouteAddress {
  pub fn iter_segments<'a>(&'a self) -> impl Iterator<Item = &'a str> {
    self.segments.iter().map(|s| s.as_str())
  }

  pub fn strip_segment_prefix<
    'a,
    Segments: IntoIterator<Item = Segment>,
    Segment: AsRef<str> + 'a,
  >(
    &'a self,
    expected_segments: Segments,
  ) -> Option<impl Iterator<Item = &'a str>> {
    let mut actual_segments = self.iter_segments();
    for expected in expected_segments.into_iter() {
      let actual = actual_segments.next();
      if actual != Some(expected.as_ref()) {
        return None;
      }
    }
    Some(actual_segments)
  }

  fn estimate_rendered_segment_upper_length_bound(segment: &str) -> usize {
    const LENGTH_OF_SLASH: usize = "\\".len();
    const LENGTH_OF_ESCAPE: usize = "/".len();
    // String byte length
    segment.len()
      // Each segment gets a slash
      + LENGTH_OF_SLASH
      // Each escaped character gets one escape's worth of byte-length
      + LENGTH_OF_ESCAPE * segment.chars().filter(|c| Self::needs_escaped(*c)).count()
  }

  fn needs_escaped(c: char) -> bool {
    std::matches!(c, '/' | '\\')
  }

  // TODO: Cache immutable view to improve perf (Requires manual trait impls to ignore cache for PartialEq, PartialOrd, Hash)
  fn rendered<'a>(&'a self) -> std::borrow::Cow<'a, str> {
    if self.segments.is_empty() {
      return std::borrow::Cow::Borrowed("");
    }
    let upper_length_bound = self
      .iter_segments()
      .map(Self::estimate_rendered_segment_upper_length_bound)
      .sum();
    let mut rendered = String::with_capacity(upper_length_bound);
    for segment in self.iter_segments() {
      rendered.push('/');
      for c in segment.chars() {
        if Self::needs_escaped(c) {
          rendered.push('\\');
        }
        rendered.push(c);
      }
    }
    debug_assert!(
      rendered.len() <= upper_length_bound,
      "Upper length bound {} must be accurate to ensure minimal resizing; actual length was {}",
      upper_length_bound,
      rendered.len(),
    );
    std::borrow::Cow::Owned(rendered)
  }

  pub fn into_bytes(self) -> Vec<u8> {
    Vec::from(self.rendered().as_ref().as_bytes())
  }
}

impl From<&RouteAddress> for String {
  fn from(a: &RouteAddress) -> Self {
    a.rendered().into_owned()
  }
}

impl From<RouteAddress> for String {
  fn from(a: RouteAddress) -> Self {
    // If/when this becomes cached, we can skip the clone and return the cached value if present
    a.rendered().into_owned()
  }
}

impl<'a, TIntoStr: Into<&'a str>> FromIterator<TIntoStr> for RouteAddress {
  fn from_iter<T: IntoIterator<Item = TIntoStr>>(iter: T) -> Self {
    RouteAddress {
      segments: iter.into_iter().map(|s| s.into().to_owned()).collect(),
    }
  }
}

impl std::fmt::Debug for RouteAddress {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.rendered().as_ref())
  }
}

impl std::fmt::Display for RouteAddress {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.rendered().as_ref())
  }
}

#[derive(Debug, thiserror::Error)]
pub enum RouteAddressParseError {
  #[error("Addresses must start with a '/' character or have no segments")]
  InvalidPrefix,
  #[error("Escape sequences must be valid- either \\\\ or \\/")]
  InvalidEscapeSequence,
}

impl FromStr for RouteAddress {
  type Err = RouteAddressParseError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    if s.is_empty() {
      return Ok(Default::default());
    }
    let mut segments = Vec::new();
    let mut cs = s.chars();
    // Ensure we start with a '/' before we move into the stateless machine
    if cs.next() != Some('/') {
      return Err(RouteAddressParseError::InvalidPrefix);
    }
    let mut current_segment = String::new();
    while let Some(c) = cs.next() {
      match c {
        // Segment markers finish the current segment and begin the next
        '/' => {
          let finished_segment = current_segment.clone();
          current_segment.clear();
          segments.push(finished_segment);
        }
        '\\' => match cs.next() {
          Some(c @ ('\\' | '/')) => current_segment.push(c),
          None | _ => return Err(RouteAddressParseError::InvalidEscapeSequence),
        },
        other => current_segment.push(other),
      }
    }
    // Reaching the end finishes the current segment
    // we always have one due to the early-out on s.is_empty()
    segments.push(current_segment);
    Ok(Self { segments })
  }
}

impl<'a> From<&'a RouteAddress> for Vec<&'a str> {
  fn from(val: &'a RouteAddress) -> Self {
    val.iter_segments().collect()
  }
}

#[cfg(test)]
mod tests {
  use std::assert_matches::assert_matches;

  use crate::common::protocol::{address::RouteAddressParseError, RouteAddress};

  const TRIVIAL_CASE: &str = "/hello/world";
  const TRIVIAL_CASE_SEGMENTS: &[&str] = &["hello", "world"];
  const ESCAPED_CASE: &str = "/\\\\foo\\/bar//\\\\/baz\\/";
  const ESCAPED_CASE_SEGMENTS: &[&str] = &["\\foo/bar", "", "\\", "baz/"];
  const MISSING_LEADING_SLASH: &str = "hello/world";

  #[test]
  fn from_segments_trivial() {
    let addr = TRIVIAL_CASE.parse::<RouteAddress>().unwrap();
    assert_eq!(
      &addr.iter_segments().collect::<Vec<_>>(),
      TRIVIAL_CASE_SEGMENTS
    );
  }

  #[test]
  fn display_round_trip_trivial() {
    let addr = TRIVIAL_CASE.parse::<RouteAddress>().unwrap();
    assert_eq!(addr.to_string(), TRIVIAL_CASE);
  }

  #[test]
  fn error_on_missing_leading_slash() {
    assert_matches!(
      MISSING_LEADING_SLASH.parse::<RouteAddress>().unwrap_err(),
      RouteAddressParseError::InvalidPrefix,
      "A missing leading slash must fail with an invalid prefix error"
    );
  }

  #[test]
  fn from_segments_zero_length_root() {
    let addr = "/".parse::<RouteAddress>().unwrap();
    assert_eq!(
      &addr.iter_segments().collect::<Vec<_>>(),
      &[""],
      "Zero-Length-Root addresses must contain empty-strings in the respective segments"
    );
  }

  #[test]
  fn display_round_trip_zero_length_root() {
    let addr = "/".parse::<RouteAddress>().unwrap();
    assert_eq!(
      addr.to_string(),
      "/",
      "Zero-Length-Root addresses must round-trip through display"
    );
  }

  #[test]
  fn from_segments_zero_length_root_multi() {
    let addr = "//".parse::<RouteAddress>().unwrap();
    assert_eq!(
      &addr.iter_segments().collect::<Vec<_>>(),
      &["", ""],
      "Zero-Length-Root addresses must contain empty-strings in the respective segments"
    );
  }

  #[test]
  fn display_round_trip_zero_length_root_multi() {
    let addr = "//".parse::<RouteAddress>().unwrap();
    assert_eq!(
      addr.to_string(),
      "//",
      "Zero-Length-Root addresses must round-trip through display"
    );
  }

  #[test]
  fn from_segments_empty() {
    let addr = "".parse::<RouteAddress>().unwrap();
    assert!(
      &addr.iter_segments().collect::<Vec<_>>().is_empty(),
      "Empty addresses must have no segments"
    );
  }

  #[test]
  fn display_round_trip_empty() {
    let addr = "".parse::<RouteAddress>().unwrap();
    assert_eq!(
      addr.to_string(),
      "",
      "Empty addresses must round-trip through display"
    );
  }

  #[test]
  fn from_segments_escaped() {
    let addr = ESCAPED_CASE.parse::<RouteAddress>().unwrap();
    assert_eq!(
      &addr.iter_segments().collect::<Vec<_>>(),
      ESCAPED_CASE_SEGMENTS,
      "Escaped addresses must contain the escaped characters at the appropriate locations raw when viewed as segments"
    );
  }

  #[test]
  fn display_round_trip_escaped() {
    let addr = ESCAPED_CASE.parse::<RouteAddress>().unwrap();
    assert_eq!(
      addr.to_string(),
      ESCAPED_CASE,
      "Escaped addresses must round-trip with the appropriate escapes in place"
    );
  }
}
