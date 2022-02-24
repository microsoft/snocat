// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::{iter::FromIterator, str::FromStr};

#[derive(Clone, PartialEq, PartialOrd, Eq, Hash)]
#[repr(transparent)]
pub struct RouteAddress(String);

impl RouteAddress {
  pub fn iter_segments<'a>(&'a self) -> impl Iterator<Item = &'a str> {
    // If segment content contains a slash, it's its responsibility to
    // handle escaping internally to that value; we split on all slashes
    let s = self.0.as_str();
    if s.is_empty() {
      return Box::new(std::iter::empty()) as Box<_>;
    }
    // Now we know the string isn't empty- the first character must be a slash
    // Skip the initial slash or error if there wasn't one
    let s = s
      .strip_prefix('/')
      .expect("Route-addresses must internally still have a leading slash");
    // TODO: Escape sequence processing
    Box::new(s.split('/')) as Box<dyn Iterator<Item = &'a str> + Send + Sync>
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

  pub fn into_bytes(self) -> Vec<u8> {
    self.0.into_bytes()
  }
}

impl From<&RouteAddress> for String {
  fn from(a: &RouteAddress) -> Self {
    a.0.to_owned()
  }
}

impl From<RouteAddress> for String {
  fn from(a: RouteAddress) -> Self {
    a.0
  }
}

impl<'a, TIntoStr: Into<&'a str>> FromIterator<TIntoStr> for RouteAddress {
  fn from_iter<T: IntoIterator<Item = TIntoStr>>(iter: T) -> Self {
    let mut buffer = String::new();
    for item in iter {
      buffer.push('/');
      buffer.push_str(item.into());
    }
    RouteAddress(buffer)
  }
}

impl std::fmt::Debug for RouteAddress {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.0.as_str())
  }
}

impl std::fmt::Display for RouteAddress {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.0.as_str())
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
      return Ok(Self(String::with_capacity(0)));
    }
    // Now we know the string isn't empty- the first character must be a slash
    // Skip the initial slash or error if there wasn't one
    let s = s
      .strip_prefix('/')
      .ok_or(RouteAddressParseError::InvalidPrefix)?;
    // TODO: Escape sequence processing
    Ok(s.split('/').collect())
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
}
