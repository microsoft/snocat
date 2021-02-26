// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use prost::{self, Message};

pub trait Messenger: prost::Message {
  fn encode_vec(&self) -> Result<Vec<u8>, prost::EncodeError>
  where
    Self: Sized;
  fn encode_length_delimited_vec(&self) -> Result<Vec<u8>, prost::EncodeError>
  where
    Self: Sized;
}

impl<T: prost::Message> Messenger for T {
  fn encode_vec(&self) -> Result<Vec<u8>, prost::EncodeError> {
    let mut buffer = Vec::new();
    <Self as prost::Message>::encode(&self, &mut buffer)?;
    Ok(buffer)
  }

  fn encode_length_delimited_vec(&self) -> Result<Vec<u8>, prost::EncodeError> {
    let mut buffer = Vec::new();
    <Self as prost::Message>::encode_length_delimited(&self, &mut buffer)?;
    Ok(buffer)
  }
}

pub struct AnyProto<T> {
  type_url: String,
  value: T,
}

impl<T> AnyProto<T> {
  pub fn new(type_url: &str, value: T) -> Self {
    Self {
      type_url: type_url.to_string(),
      value,
    }
  }

  pub fn type_url(&self) -> &str {
    &self.type_url
  }

  pub fn value(&self) -> &T {
    &self.value
  }

  pub fn value_mut(&mut self) -> &mut T {
    &mut self.value
  }

  pub fn to_value(self) -> T {
    self.value
  }
}

impl<T: prost::Message + Default> AnyProto<T> {
  pub fn decode_unverified<B: prost::bytes::Buf>(
    buf: B,
  ) -> Result<AnyProto<T>, prost::DecodeError> {
    let res = prost_types::Any::decode(buf)?;
    T::decode(&*res.value).map(|t| AnyProto::new(&res.type_url, t))
  }

  pub fn try_decode<B: prost::bytes::Buf>(
    type_url: &str,
    buf: B,
  ) -> Result<Option<AnyProto<T>>, prost::DecodeError> {
    let res = prost_types::Any::decode(buf)?;
    Self::try_from_any(type_url, &res)
  }

  pub fn try_from_any(
    type_url: &str,
    any: &prost_types::Any,
  ) -> Result<Option<AnyProto<T>>, prost::DecodeError> {
    if any.type_url != type_url {
      Ok(None)
    } else {
      T::decode(&*any.value)
        .map(|t| AnyProto::new(type_url, t))
        .map(Some)
    }
  }

  pub fn from_any_unverified(any: &prost_types::Any) -> Result<AnyProto<T>, prost::DecodeError> {
    T::decode(&*any.value).map(|t| AnyProto::new(&any.type_url, t))
  }
}

impl<T: prost::Message> std::convert::TryInto<prost_types::Any> for AnyProto<T> {
  type Error = prost::EncodeError;

  fn try_into(self) -> Result<prost_types::Any, Self::Error> {
    Ok(prost_types::Any {
      type_url: self.type_url,
      value: T::encode_vec(&self.value)?,
    })
  }
}

impl<T: std::fmt::Debug> std::fmt::Debug for AnyProto<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "Any[{}](", &self.type_url)?;
    std::fmt::Debug::fmt(&self.value, f)?;
    write!(f, ")")
  }
}

impl<T: prost::Message + std::fmt::Debug + Default> prost::Message for AnyProto<T> {
  fn encode_raw<B>(&self, buf: &mut B)
  where
    B: prost::bytes::BufMut,
    Self: Sized,
  {
    let mut inner = Vec::new();
    T::encode_raw(&self.value, &mut inner);
    prost_types::Any {
      type_url: self.type_url.clone(),
      value: inner,
    }
    .encode_raw(buf);
  }

  fn merge_field<B>(
    &mut self,
    tag: u32,
    wire_type: prost::encoding::WireType,
    buf: &mut B,
    ctx: prost::encoding::DecodeContext,
  ) -> Result<(), prost::DecodeError>
  where
    B: prost::bytes::Buf,
    Self: Sized,
  {
    let mut inner = Vec::new();
    T::encode_raw(&self.value, &mut inner);
    let mut holder = prost_types::Any {
      type_url: self.type_url.clone(),
      value: inner,
    };
    holder.merge_field(tag, wire_type, buf, ctx)?;
    self.value = T::decode(&*holder.value)?;
    Ok(())
  }

  fn encoded_len(&self) -> usize {
    let mut inner = Vec::new();
    T::encode_raw(&self.value, &mut inner);
    prost_types::Any {
      type_url: self.type_url.clone(),
      value: inner,
    }
    .encoded_len()
  }

  fn clear(&mut self) {
    let mut inner = Vec::new();
    T::encode_raw(&self.value, &mut inner);
    let mut holder = prost_types::Any {
      type_url: self.type_url.clone(),
      value: inner,
    };
    holder.clear();
    self.type_url = holder.type_url;
    self.value =
      T::decode(&*holder.value).expect("Cleared AnyProto values must decode successfully");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::util::messenger::*;
  use prost::Message;
  use prost_types::Any;
  use std::convert::TryInto;

  #[test]
  fn round_trip_string() {
    let expected_value = "hello world";
    let any: Any = AnyProto::new("string", String::from(expected_value))
      .try_into()
      .unwrap();
    let any_encoded = any.encode_vec().unwrap().into_boxed_slice();
    assert_eq!(
      AnyProto::<String>::try_from_any("string", &any)
        .unwrap()
        .unwrap()
        .value(),
      expected_value
    );
    assert_eq!(
      AnyProto::<String>::from_any_unverified(&any)
        .unwrap()
        .type_url(),
      "string"
    );
    assert_eq!(
      AnyProto::<String>::from_any_unverified(&any)
        .unwrap()
        .value(),
      expected_value
    );
    assert_eq!(
      AnyProto::<String>::try_decode("string", &*any_encoded)
        .unwrap()
        .unwrap()
        .value(),
      expected_value
    );
    assert_eq!(
      String::decode(&*Any::decode(&*any_encoded).unwrap().value).unwrap(),
      expected_value
    );
  }
}
