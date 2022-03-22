// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0

use std::{
  fmt::Debug,
  marker::PhantomData,
  sync::{Arc, Weak},
  time::Duration,
};

use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt};
use tokio_util::sync::CancellationToken;

use fred::pool::DynamicRedisPool;
use fred::prelude::*;

use super::super::{registry::TunnelRegistry, TunnelName};
use crate::util::{cancellation::CancellationListener, dropkick::Dropkick};

/// Configuration for background work for a Redis registry
///
/// Use `..Default::default()` to initialize, as this is
/// non-exhaustive for when new parameters are added.
#[non_exhaustive]
pub struct RedisRegistryConfig {
  /// Time before expiring tunnel name to redis-unique-key mappings
  ///
  /// If the target key expires before the name-to-id reference,
  /// it behaves as if it is null anyway.
  ///
  /// Only durations higher than seconds count
  tunnel_id_ref_lifetime: std::time::Duration,
  /// Time before expiring tunnel entries at redis-unique-key locations
  ///
  /// Only durations higher than seconds count
  tunnel_entry_lifetime: std::time::Duration,
  /// How often the background thread will attempt to refresh expiry of its items
  ///
  /// If a refresh is attempted and the target is missing, and no current target
  /// exists at the name mapping, it will attempt reregistration.
  renewal_rate: std::time::Duration,
}

impl Default for RedisRegistryConfig {
  fn default() -> Self {
    Self {
      tunnel_id_ref_lifetime: Duration::from_secs(600),
      tunnel_entry_lifetime: Duration::from_secs(60),
      renewal_rate: Duration::from_secs(20),
    }
  }
}

pub type Registration = Dropkick<CancellationToken>;
pub type RegistrationMap = DashMap<String, Weak<Registration>>;

/// Single-occupancy overriding registry based on Redis
///
/// Registration conflicts are handled by replacement of name ownership (Last wins)
///
/// Dropping the last identifier for a key deregisters it from auto-renewal, but does not perform explicit IO.
pub struct RedisRegistry<R> {
  config: Arc<RedisRegistryConfig>,
  pool: Arc<DynamicRedisPool>,
  active_registration_map: Arc<RegistrationMap>,
  phantom_item: PhantomData<R>,
  // Cancels all renewal jobs if the registry itself is dropped; is parent to all renewal task tokens
  core_canceller: Arc<Dropkick<CancellationToken>>,
}

impl<R> RedisRegistry<R>
where
  R: 'static,
{
  #[must_use]
  pub fn new(config: RedisRegistryConfig, pool: DynamicRedisPool) -> Self {
    Self {
      config: Arc::new(config),
      pool: Arc::new(pool),
      phantom_item: PhantomData,
      active_registration_map: Arc::new(RegistrationMap::default()),
      core_canceller: Arc::new(Dropkick::new(CancellationToken::new())),
    }
  }

  fn tunnel_key_for(tunnel_name: &TunnelName) -> String {
    format!("/tunnel/id/{}", tunnel_name.raw())
  }

  fn tunnel_rid_key(rid: &str) -> String {
    format!("/tunnel/rid/{}", rid)
  }

  fn register_for_renewals(
    registration_map: Arc<RegistrationMap>,
    pool: Arc<DynamicRedisPool>,
    canceller: &CancellationToken,
    config: &RedisRegistryConfig,
    tunnel_name: &TunnelName,
    rid: &str,
    entry_encoded: Vec<u8>,
  ) -> Arc<Registration> {
    let canceller = canceller.child_token();

    // TODO: tracing on error-exit of renewal tasks
    tokio::task::spawn(Self::run_renewal(
      pool,
      canceller.clone().into(),
      tunnel_name.clone(),
      rid.to_owned(),
      entry_encoded,
      config.tunnel_id_ref_lifetime,
      config.tunnel_entry_lifetime,
      config.renewal_rate,
    ));

    let registration = Arc::new(Dropkick::new(canceller));
    // Add our registration to the map to allow us to cancel entries which are no longer valid
    registration_map.insert(rid.to_string(), Arc::downgrade(&registration));
    registration
  }

  async fn run_renewal(
    pool: Arc<DynamicRedisPool>,
    canceller: CancellationListener,
    tunnel_name: TunnelName,
    rid: String,
    entry_encoded: Vec<u8>,
    tunnel_id_ref_lifetime: std::time::Duration,
    tunnel_entry_lifetime: std::time::Duration,
    renewal_rate: std::time::Duration,
  ) -> Result<(), anyhow::Error> {
    let tunnel_key = Self::tunnel_rid_key(&rid);
    let tunnel_ref_key = Self::tunnel_key_for(&tunnel_name);
    let tunnel_entry_lifetime_secs = tunnel_entry_lifetime
      .as_secs()
      .try_into()
      .unwrap_or(i64::MAX);
    // Ensure we don't smash the redis server with an absurdly-small delay; minimum is 1 second
    let renewal_rate = renewal_rate.max(std::time::Duration::from_secs(1));
    // Run the loop until asked to exit
    // We'll be cancelled if either the core canceller is disposed *or* the entry itself is deleted
    while !canceller.is_cancelled() {
      tokio::time::sleep(renewal_rate).await;
      let conn = pool.next_connect(true).await;
      let _ = conn
        .expire::<bool, _>(
          &tunnel_ref_key,
          tunnel_id_ref_lifetime
            .as_secs()
            .try_into()
            .unwrap_or(i64::MAX),
        )
        .await;
      // Update expiration; if that does not add an expiration, check if the key is known to not exist
      // If it is known-non-existent, try to reinsert it into the dataset from the encoded copy we saved
      if conn.expire(&tunnel_key, tunnel_entry_lifetime_secs).await == Ok(false)
        && conn.exists(&tunnel_key).await == Ok(false)
      {
        // Try to set the value back to the expected state, but don't overwrite existing keys to do so
        conn
          .set::<(), _, _>(
            &tunnel_key,
            entry_encoded.as_slice(),
            Some(Expiration::EX(tunnel_entry_lifetime_secs)),
            Some(SetOptions::NX),
            false,
          )
          .await
          .ok();
      }
    }
    Ok(())
  }

  async fn deregister_by_rid(
    registration_map: Arc<RegistrationMap>,
    conn: RedisClient,
    rid: String,
  ) -> Result<Option<R>, RedisRegistryError>
  where
    R: serde::de::DeserializeOwned,
  {
    // If we have the item in our local map, stop refreshing it, and delete the database-side key
    let encoded_entry: Option<Vec<u8>> =
      if let Some((_, owned_renewer)) = registration_map.remove(&rid) {
        // Cancel the renewer if it is still allocated
        owned_renewer.upgrade().map(|v| v.cancel());
        // The remote key is ours; delete it while getting it, if it's there at all
        conn.getdel(Self::tunnel_rid_key(rid.as_str())).await?
      } else {
        // Read the value at the RID, if present, otherwise act as if the ref-key didn't exist
        conn.get(Self::tunnel_rid_key(rid.as_str())).await?
      };
    Ok(match encoded_entry {
      Some(encoded_entry) => serde_json::from_slice(encoded_entry.as_slice())?,
      None => None,
    })
  }
}

#[derive(thiserror::Error, Debug)]
pub enum RedisRegistryError {
  #[error("Registry redis error: {0}")]
  Redis(
    #[from]
    #[backtrace]
    RedisError,
  ),
  #[error("Registry serialization error: {0}")]
  SerializationError(
    #[from]
    #[backtrace]
    serde_json::Error,
  ),
  #[error("Could not find a non-conflicting key after {num_attempts} attempts")]
  RepeatKeyConflicts { num_attempts: usize },
}

// Tunnel names are their equivalent redis key
impl From<TunnelName> for RedisKey {
  fn from(val: TunnelName) -> Self {
    val.0.as_str().into()
  }
}

#[derive(Debug, Clone)]
pub struct RedisIdentifier {
  tunnel_name: TunnelName,
  rid: String,
  _registration: Option<Arc<Registration>>,
}

impl std::hash::Hash for RedisIdentifier {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    self.tunnel_name.hash(state);
    self.rid.hash(state);
    // `self._registration` is intentionally omitted; Cancellation tokens
    // aren't hashable, and it isn't actually necessary here anyway.
  }
}

impl PartialEq for RedisIdentifier {
  fn eq(&self, other: &Self) -> bool {
    self.tunnel_name == other.tunnel_name && self.rid == other.rid
  }
}

impl Eq for RedisIdentifier {}

type Ident = RedisIdentifier;

impl<R> TunnelRegistry for RedisRegistry<R>
where
  R: serde::ser::Serialize + serde::de::DeserializeOwned + Send + Sync + Debug + Clone + 'static,
{
  type Identifier = RedisIdentifier;

  type Record = R;

  type Error = RedisRegistryError;

  fn lookup<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let tunnel_name = tunnel_name.clone();
    let pool = self.pool.clone();
    async move {
      // Get a connection from the pool
      let conn = pool.next_connect(false).await;
      // Wait for it to connect if it isn't already connected; error if failed
      conn.wait_for_connect().await?;
      // Read the reference key, if present, mapping tunnel-name to its RID
      let rid: Option<String> = conn.get(Self::tunnel_key_for(&tunnel_name)).await?;
      // `let-else` can't come soon enough
      // https://rust-lang.github.io/rfcs/3137-let-else.html
      let rid: String = if let Some(rid) = rid {
        rid
      } else {
        return Ok(None);
      };
      // Read the value at the RID, if present, otherwise act as if the ref-key didn't exist
      let encoded_entry: Option<Vec<u8>> = conn.get(Self::tunnel_rid_key(rid.as_str())).await?;
      Ok(match encoded_entry {
        Some(encoded_entry) => serde_json::from_slice(encoded_entry.as_slice())?,
        None => None,
      })
    }
    .boxed()
  }

  fn register<'a>(
    &'a self,
    tunnel_name: TunnelName,
    record: &'a Self::Record,
  ) -> BoxFuture<'static, Result<Self::Identifier, Self::Error>> {
    let pool = self.pool.clone();
    let core_canceller = self.core_canceller.clone();
    let config = self.config.clone();
    let record = record.clone();
    let tunnel_expiration = Expiration::EX(
      self
        .config
        .tunnel_entry_lifetime
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX),
    );
    let tunnel_ref_expiration = Expiration::EX(
      self
        .config
        .tunnel_id_ref_lifetime
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX),
    );
    let tunnel_ref_key = Self::tunnel_key_for(&tunnel_name);
    let registration_map = Arc::clone(&self.active_registration_map);
    async move {
      // Encode and save the record for use in redis calls below
      let encoded = serde_json::to_vec(&record)?;
      // Get a connection from the pool
      let conn = pool.next_connect(false).await;
      // Wait for it to connect if it isn't already connected; error if failed
      conn.wait_for_connect().await?;
      // Create-associated entry by repeating SETNX until the key is newly created, in a
      // transaction with EXPIRE to ensure that any created keys are marked for cleanup.
      let rid = {
        const MAX_ITERATIONS: usize = 10;
        let mut iteration = 0usize;
        loop {
          if iteration >= MAX_ITERATIONS {
            return Err(Self::Error::RepeatKeyConflicts {
              num_attempts: MAX_ITERATIONS,
            });
          }
          iteration += 1;
          // Create a new RID and key
          let rid = uuid::Uuid::new_v4().to_string();
          let rid_key = Self::tunnel_rid_key(&rid);
          // Set the value for that key, retrying if it already exists
          if conn
            .set::<(), _, _>(
              rid_key,
              encoded.as_slice(),
              Some(tunnel_expiration.clone()),
              Some(SetOptions::NX),
              false,
            )
            .await
            .is_ok()
          {
            // If we successfully set the key, break to use that RID in the final reference
            break rid;
          }
        }
      };

      conn
        .set(
          tunnel_ref_key,
          &rid,
          Some(tunnel_ref_expiration),
          None,
          false,
        )
        .await?;

      let registration = Self::register_for_renewals(
        registration_map,
        pool,
        &core_canceller,
        config.as_ref(),
        &tunnel_name,
        &rid,
        encoded,
      );

      Ok(Ident {
        tunnel_name,
        rid,
        _registration: Some(registration),
      })
    }
    .boxed()
  }

  fn deregister<'a>(
    &'a self,
    tunnel_name: &'a TunnelName,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    let registration_map = Arc::clone(&self.active_registration_map);
    let tunnel_name = tunnel_name.clone();
    let pool = self.pool.clone();
    async move {
      // Get a connection from the pool
      let conn = pool.next_connect(false).await;
      // Wait for it to connect if it isn't already connected; error if failed
      conn.wait_for_connect().await?;
      // Read the reference key, if present, mapping tunnel-name to its RID
      let rid: Option<String> = conn.getdel(Self::tunnel_key_for(&tunnel_name)).await?;
      // `let-else` can't come soon enough
      // https://rust-lang.github.io/rfcs/3137-let-else.html
      let rid: String = if let Some(rid) = rid {
        rid
      } else {
        return Ok(None);
      };
      Self::deregister_by_rid(registration_map, conn, rid).await
    }
    .boxed()
  }

  fn deregister_identifier<'a>(
    &'a self,
    identifier: Self::Identifier,
  ) -> BoxFuture<'static, Result<Option<Self::Record>, Self::Error>> {
    // Self::deregister(&self, &identifier.0)
    // We can leave the ID-ref present since redis will TTL it, and lacks a "Delete only if matching" option...
    let registration_map = Arc::clone(&self.active_registration_map);
    let pool = self.pool.clone();
    async move {
      // Get a connection from the pool
      let conn = pool.next_connect(false).await;
      // Wait for it to connect if it isn't already connected; error if failed
      conn.wait_for_connect().await?;
      let mut identifier = identifier;
      // Drop the identifier after getting the ID from it, in order to decrement our hold on the registration
      let rid = std::mem::replace(&mut identifier.rid, String::new());
      drop(identifier);
      // Destroy the entry from the redis store and- if present- cancel it and remove it from the registration map
      Self::deregister_by_rid(registration_map, conn, rid).await
    }
    .boxed()
  }
}
