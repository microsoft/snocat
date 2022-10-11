// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::{fmt::Debug, sync::Arc};

use futures::{
  future::{BoxFuture, FutureExt},
  TryFutureExt,
};

use super::super::{registry::TunnelRegistry, TunnelName};

pub struct WriteThroughCache<Encode, Decode, Cache, Target> {
  encode: Arc<Encode>,
  decode: Arc<Decode>,
  a: Arc<Cache>,
  b: Arc<Target>,
}

#[derive(Debug, Clone, Hash)]
pub struct CacheIdentifier<CacheIdent, TargetIdent> {
  cache_ident: CacheIdent,
  target_ident: TargetIdent,
}

impl<CacheIdent, TargetIdent> CacheIdentifier<CacheIdent, TargetIdent> {
  fn new(cache_ident: CacheIdent, target_ident: TargetIdent) -> Self {
    Self {
      cache_ident,
      target_ident,
    }
  }
}

#[derive(thiserror::Error, Debug)]
pub enum WriteThroughCacheError<A, B> {
  #[error("Cache error: {0}")]
  CacheError(
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    A,
  ),
  #[error("Cached target error: {0}")]
  TargetError(
    #[source]
    #[cfg_attr(feature = "backtrace", backtrace)]
    B,
  ),
}

impl<Encode, Decode, Cache, Target> TunnelRegistry
  for WriteThroughCache<Encode, Decode, Cache, Target>
where
  Cache: TunnelRegistry + 'static,
  Cache::Record: Clone + Send + Sync + 'static,
  Target: TunnelRegistry + 'static,
  Target::Record: Clone + Send + Sync + 'static,
  Encode: Fn(&Cache::Record) -> Target::Record + Send + Sync + 'static,
  Decode: Fn(Target::Record) -> Cache::Record + Send + Sync + 'static,
{
  type Identifier = CacheIdentifier<Cache::Identifier, Target::Identifier>;

  type Record = Cache::Record;

  type Error = WriteThroughCacheError<Cache::Error, Target::Error>;

  fn lookup<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let tunnel_name = tunnel_name.clone();
    let a = self.a.clone();
    let b = self.b.clone();
    let decode = self.decode.clone();
    async move {
      if let Some(res) = a
        .lookup(&tunnel_name)
        .await
        .map_err(Self::Error::CacheError)?
      {
        Ok(Some(res))
      } else {
        if let Some(res) = b
          .lookup(&tunnel_name)
          .await
          .map_err(Self::Error::TargetError)?
        {
          let decoded = decode(res);
          // TODO: By not returning this registration, the identifier won't clear the cached version when the remote one is cleared...
          a.register(tunnel_name, &decoded)
            .await
            .map_err(Self::Error::CacheError)?;
          Ok(Some(decoded))
        } else {
          Ok(None)
        }
      }
    }
    .boxed()
  }

  fn register<'a>(
    &'a self,
    tunnel_name: TunnelName,
    record: &'a Self::Record,
  ) -> BoxFuture<'static, Result<Self::Identifier, Self::Error>> {
    let record: <Cache as TunnelRegistry>::Record = Clone::clone(&record);
    let tunnel_name = tunnel_name.clone();
    let a = self.a.clone();
    let b = self.b.clone();
    let encode = self.encode.clone();
    async move {
      let encoded = encode(&record);
      let (a_res, b_res) = futures::future::try_join(
        a.register(tunnel_name.clone(), &record)
          .map_err(Self::Error::CacheError),
        b.register(tunnel_name, &encoded)
          .map_err(Self::Error::TargetError),
      )
      .await?;
      Ok(Self::Identifier::new(a_res, b_res))
    }
    .boxed()
  }

  fn deregister<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let decode = self.decode.clone();
    let dereg_a = self
      .a
      .deregister(tunnel_name)
      .map_err(Self::Error::CacheError);
    let dereg_b = self
      .b
      .deregister(tunnel_name)
      .map_err(Self::Error::TargetError);
    async move {
      let (a_res, b_res) = futures::future::try_join(dereg_a, dereg_b).await?;
      // Return the record directly if cached in A, otherwise decode from B and return that if present
      Ok(a_res.or_else(|| b_res.map(decode.as_ref())))
    }
    .boxed()
  }

  fn deregister_identifier<'a>(
    &'a self,
    identifier: Self::Identifier,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let decode = self.decode.clone();
    let dereg_a = self
      .a
      .deregister_identifier(identifier.cache_ident)
      .map_err(Self::Error::CacheError);
    let dereg_b = self
      .b
      .deregister_identifier(identifier.target_ident)
      .map_err(Self::Error::TargetError);
    async move {
      let (a_res, b_res) = futures::future::try_join(dereg_a, dereg_b).await?;
      // Return the record directly if cached in A, otherwise decode from B and return that if present
      Ok(a_res.or_else(|| b_res.map(decode.as_ref())))
    }
    .boxed()
  }
}

impl<Encode, Decode, Cache, Target> WriteThroughCache<Encode, Decode, Cache, Target>
where
  Cache: TunnelRegistry + 'static,
  Target: TunnelRegistry + 'static,
  Encode: Fn(&Cache::Record) -> Target::Record + Send + Sync + 'static,
  Decode: Fn(Target::Record) -> Cache::Record + Send + Sync + 'static,
{
  pub fn new(cache: Arc<Cache>, target: Arc<Target>, encode: Encode, decode: Decode) -> Self {
    Self {
      a: cache,
      b: target,
      encode: Arc::new(encode),
      decode: Arc::new(decode),
    }
  }
}
