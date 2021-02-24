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

pub struct AnyProto<T>(String, T);

impl<T> AnyProto<T> {
  pub fn new(type_url: &str, value: T) -> Self {
    Self(type_url.to_string(), value)
  }

  pub fn type_url(&self) -> &str {
    &self.0
  }

  pub fn value(&self) -> &T {
    &self.1
  }

  pub fn value_mut(&mut self) -> &mut T {
    &mut self.1
  }

  pub fn to_value(self) -> T {
    self.1
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
    if res.type_url != type_url {
      Ok(None)
    } else {
      T::decode(&*res.value)
        .map(|t| AnyProto::new(type_url, t))
        .map(Some)
    }
  }
}

impl<T: prost::Message> std::convert::TryInto<prost_types::Any> for AnyProto<T> {
  type Error = prost::EncodeError;

  fn try_into(self) -> Result<prost_types::Any, Self::Error> {
    let mut inner = Vec::new();
    T::encode(&self.1, &mut inner)?;
    Ok(prost_types::Any {
      type_url: self.0,
      value: inner,
    })
  }
}

impl<T: std::fmt::Debug> std::fmt::Debug for AnyProto<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "Any[{}](", &self.0)?;
    std::fmt::Debug::fmt(&self.1, f)?;
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
    T::encode_raw(&self.1, &mut inner);
    prost_types::Any {
      type_url: self.0.clone(),
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
    T::encode_raw(&self.1, &mut inner);
    let mut holder = prost_types::Any {
      type_url: self.0.clone(),
      value: inner,
    };
    holder.merge_field(tag, wire_type, buf, ctx)?;
    self.1 = T::decode(&*holder.value)?;
    Ok(())
  }

  fn encoded_len(&self) -> usize {
    let mut inner = Vec::new();
    T::encode_raw(&self.1, &mut inner);
    prost_types::Any {
      type_url: self.0.clone(),
      value: inner,
    }
    .encoded_len()
  }

  fn clear(&mut self) {
    let mut inner = Vec::new();
    T::encode_raw(&self.1, &mut inner);
    let mut holder = prost_types::Any {
      type_url: self.0.clone(),
      value: inner,
    };
    holder.clear();
    self.0 = holder.type_url;
    self.1 = T::decode(&*holder.value).expect("Cleared AnyProto values must decode successfully");
  }
}
