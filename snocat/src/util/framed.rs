// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum NextExpected {
  LengthSpecifier,
  Content { length: usize },
}

#[derive(thiserror::Error, Debug)]
pub enum ReadError {
  #[error("Frame length exceeded expectation of {expected} bytes with {received}")]
  MaxLengthExceeded { expected: usize, received: usize },
  #[error("Unexpected end of frame; expected {expected:?}")]
  UnexpectedEnd {
    expected: NextExpected,
    error: ::std::io::Error,
  },
}

#[derive(thiserror::Error, Debug)]
pub enum JsonReadError {
  #[error("Failure reading JSON from frame: {0}")]
  Read(#[from] ReadError),
  #[error("Failure deserializing JSON from frame: {0}")]
  Deserialization(#[from] ::serde_json::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum WriteError {
  #[error("Frame write failure: {0:?}")]
  UnexpectedEnd(#[from] ::std::io::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum JsonWriteError {
  #[error("Failure writing JSON into frame: {0}")]
  Write(#[from] WriteError),
  #[error("Failure serializing JSON for frame: {0}")]
  Serialization(#[from] ::serde_json::Error),
  /// Since the output is generated automatically, we return before
  /// risking corruption of the stream, skipping any write actions.
  ///
  /// Will never occur when a maximum length of `None` is provided.
  #[error("Frame length exceeded expectation of {expected} bytes with {produced}")]
  MaxLengthExceeded { expected: usize, produced: usize },
}

pub async fn read_frame<T: tokio::io::AsyncRead + Unpin>(
  mut s: T,
  max_length: Option<usize>,
) -> Result<Vec<u8>, ReadError> {
  use tokio::io::AsyncReadExt;
  let length = s
    .read_u32()
    .await
    .map_err(|error| ReadError::UnexpectedEnd {
      expected: NextExpected::LengthSpecifier,
      error,
    })? as usize;
  if let Some(max_length) = max_length {
    if length > max_length {
      return Err(ReadError::MaxLengthExceeded {
        expected: max_length,
        received: length,
      });
    }
  }
  let mut buffer = Vec::with_capacity(length);
  buffer.resize_with(length, Default::default);
  s.read_exact(buffer.as_mut_slice())
    .await
    .map_err(|error| ReadError::UnexpectedEnd {
      expected: NextExpected::Content { length },
      error,
    })?;
  Ok(buffer)
}

pub async fn write_frame<T: tokio::io::AsyncWrite + Unpin>(
  mut s: T,
  buffer: &[u8],
) -> Result<(), WriteError> {
  use tokio::io::AsyncWriteExt;
  s.write_u32(buffer.len() as u32).await?;
  Ok(s.write_all(&buffer).await?)
}

pub async fn read_framed_json<
  TStream: tokio::io::AsyncRead + Unpin,
  TOutput: serde::de::DeserializeOwned,
>(
  s: TStream,
  max_length: Option<usize>,
) -> Result<TOutput, JsonReadError> {
  let buffer = read_frame(s, max_length).await?;
  let x = serde_json::from_slice::<TOutput>(&buffer)?;
  Ok(x)
}

pub async fn write_framed_json<TStream: tokio::io::AsyncWrite + Unpin, TInput: serde::Serialize>(
  s: TStream,
  value: TInput,
  max_length: Option<usize>,
) -> Result<(), JsonWriteError> {
  const U32_SIZE: usize = std::mem::size_of::<u32>();
  let buffer = serde_json::to_vec(&value)?.into_boxed_slice(); // Drop the ability to resize the buffer
  if let Some(max_length) = max_length {
    if buffer.len() + U32_SIZE > max_length {
      return Err(JsonWriteError::MaxLengthExceeded {
        expected: max_length,
        produced: buffer.len() + U32_SIZE,
      });
    }
  }
  Ok(write_frame(s, &buffer).await?)
}

#[cfg(test)]
mod tests {
  use std::assert_matches::assert_matches;

  use crate::util::framed::{read_frame, read_framed_json, write_frame, write_framed_json};

  use super::JsonWriteError;

  #[tokio::test]
  async fn stream_framed_roundtrip() {
    use super::{read_frame, write_frame};
    use ::std::io::Seek;
    use ::tokio::io::{AsyncReadExt, AsyncWriteExt};
    const TEST_BLOB_LENGTH: usize = 1234;
    let mut buffer: Vec<u8> = Vec::with_capacity(TEST_BLOB_LENGTH + std::mem::size_of::<u32>());
    {
      let mut cursor = std::io::Cursor::new(&mut buffer);
      // Test data is a simple array of 0 through (but not including) its capacity
      let test_data = {
        let mut test_data = Vec::with_capacity(TEST_BLOB_LENGTH);
        test_data.extend(
          (0u32..(test_data.capacity() as u32))
            .map(|x| std::ops::Rem::rem(x, std::u8::MAX as u32) as u8),
        );
        test_data
      };
      write_frame(&mut cursor, &test_data)
        .await
        .expect("Writing frame to stream must succeed");
      cursor.set_position(0);
      let deserialized = read_frame(&mut cursor, None)
        .await
        .expect("Reading frame from stream must succeed");
      // Input and output data should be the same
      assert_eq!(test_data, deserialized);
      // After the length of a u32, the stream should be equal to the content
      assert_eq!(&buffer[std::mem::size_of::<u32>()..], &test_data[..]);
    }
    // Stream must receive content of equal length to a u32 plus that of the content
    assert_eq!(buffer.len(), TEST_BLOB_LENGTH + std::mem::size_of::<u32>());
    // Verify function on zero-length frames
    buffer.clear();
    {
      let mut cursor = std::io::Cursor::new(&mut buffer);
      // Test data is an empty array
      let test_data = Vec::new();
      write_frame(&mut cursor, &test_data).await.unwrap();
      cursor.set_position(0);
      let result = read_frame(&mut cursor, None).await.unwrap();
      assert_eq!(&test_data, &result);
    }
    assert_eq!(buffer.len(), std::mem::size_of::<u32>());
  }

  #[tokio::test]
  async fn exceeding_maximum_length_is_no_op() {
    let mut buffer: Vec<u8> = Vec::with_capacity(0);
    {
      // a single-character string in JSON UTF-8 is 3 bytes long due to quotes
      assert_matches!(
        write_framed_json(&mut buffer, "a", Some(std::mem::size_of::<u32>() + 2)).await,
        Err(JsonWriteError::MaxLengthExceeded { .. })
      );
    }
    assert_eq!(
      buffer.len(),
      0,
      "Buffer must not have been written to during a max length error"
    );
  }

  #[tokio::test]
  async fn stream_json_serialization_roundtrip() {
    let buffer: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(buffer);
    let original = (6f32, String::from("a"), 2u8, 12f64);
    write_framed_json(&mut cursor, &original, None)
      .await
      .expect("Writing to stream must succeed");
    cursor.set_position(0);
    let deserialized = read_framed_json(&mut cursor, None)
      .await
      .expect("Reading header from stream must succeed");
    assert_eq!(original, deserialized);
  }
}
