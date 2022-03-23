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
  tunnel_id_ref_lifetime: Duration,
  /// Time before expiring tunnel entries at redis-unique-key locations
  ///
  /// Only durations higher than seconds count
  tunnel_entry_lifetime: Duration,
  /// How often the background thread will attempt to refresh expiry of its items
  ///
  /// If a refresh is attempted and the target is missing, and no current target
  /// exists at the name mapping, it will attempt reregistration.
  renewal_rate: Duration,
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
#[derive(Clone)]
pub struct RedisRegistry<R> {
  config: Arc<RedisRegistryConfig>,
  pool: Arc<DynamicRedisPool>,
  active_registration_map: Arc<RegistrationMap>,
  phantom_item: PhantomData<R>,
  // Cancels all renewal jobs if the registry itself is dropped; is parent to all renewal task tokens
  core_canceller: Arc<Dropkick<CancellationToken>>,
}

impl<R> Debug for RedisRegistry<R>
where
  R: Debug,
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(std::any::type_name::<RedisRegistry<R>>())
      .field("pool_size", &self.pool.size())
      .finish()
  }
}

impl<R> RedisRegistry<R> {
  #[must_use]
  pub fn new<Pool: Into<Arc<DynamicRedisPool>>>(config: RedisRegistryConfig, pool: Pool) -> Self {
    Self {
      config: Arc::new(config),
      pool: pool.into(),
      phantom_item: PhantomData,
      active_registration_map: Arc::new(RegistrationMap::default()),
      core_canceller: Arc::new(Dropkick::new(CancellationToken::new())),
    }
  }
}

impl<R> RedisRegistry<R>
where
  R: 'static,
{
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
    tunnel_id_ref_lifetime: Duration,
    tunnel_entry_lifetime: Duration,
    renewal_rate: Duration,
  ) -> Result<(), anyhow::Error> {
    let tunnel_key = Self::tunnel_rid_key(&rid);
    let tunnel_ref_key = Self::tunnel_key_for(&tunnel_name);
    let tunnel_entry_lifetime_secs = tunnel_entry_lifetime
      .as_secs()
      .try_into()
      .unwrap_or(i64::MAX);
    // Ensure we don't smash the redis server with an absurdly-small delay; minimum is 1 second in non-test envs
    let renewal_rate = renewal_rate.max({
      if cfg!(test) {
        Duration::from_millis(10)
      } else {
        Duration::from_secs(1)
      }
    });
    // Run the loop until asked to exit
    // We'll be cancelled if either the core canceller is disposed *or* the entry itself is deleted
    while !canceller.is_cancelled() {
      futures::future::select(
        tokio::time::sleep(renewal_rate).boxed(),
        canceller.cancelled().boxed(),
      )
      .await;
      if canceller.is_cancelled() {
        break;
      }
      let conn = Self::get_pool_connection(pool.clone()).await?;
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

  async fn get_pool_connection(
    pool: Arc<DynamicRedisPool>,
  ) -> Result<RedisClient, RedisRegistryError> {
    // Get a connection from the pool
    let conn = pool.next_connect(false).await;
    // Wait for it to connect if it isn't already connected; error if failed
    conn.wait_for_connect().await?;
    Ok(conn)
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
      let conn = Self::get_pool_connection(pool).await?;
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
      let conn = Self::get_pool_connection(pool.clone()).await?;
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
      let conn = Self::get_pool_connection(pool).await?;
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
      let conn = Self::get_pool_connection(pool).await?;
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

#[cfg(all(test, feature = "integration-redis"))]
mod integration_tests {
  use std::{fmt::Debug, sync::Arc, time::Duration};

  use fred::{
    pool::DynamicRedisPool,
    types::{ReconnectPolicy, RedisConfig, ServerConfig},
  };
  use uuid::Uuid;

  use crate::{common::protocol::tunnel::TunnelName, ext::future::FutureExtExt};

  use super::super::TunnelRegistry;
  use super::{RedisRegistry, RedisRegistryConfig};

  #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
  struct TestEntry {
    name: String,
    id: u32,
  }

  #[derive(Clone)]
  #[non_exhaustive]
  struct TestReg<R> {
    pub registry: RedisRegistry<R>,
    pub pool: Arc<DynamicRedisPool>,
    pub config: Arc<RedisRegistryConfig>,
  }

  impl<R> Debug for TestReg<R>
  where
    R: Debug,
  {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.debug_struct("TestReg")
        .field("registry", &self.registry)
        .field(
          "pool",
          &format!("{{{}}}", std::any::type_name::<DynamicRedisPool>()),
        )
        .finish_non_exhaustive()
    }
  }

  async fn create_test_pool() -> DynamicRedisPool {
    let policy = ReconnectPolicy::Constant {
      attempts: 1,
      max_attempts: 2,
      delay: 25,
    };
    let pool = DynamicRedisPool::new(
      RedisConfig {
        server: ServerConfig::Centralized {
          host: "127.0.0.1".to_owned(),
          port: 6379,
        },
        fail_fast: true,
        ..Default::default()
      },
      Some(policy),
      1,
      2,
    );
    pool.connect().await;
    {
      let conn = pool.next_connect(false).await;
      conn
        .wait_for_connect()
        .poll_until(tokio::time::sleep(Duration::from_secs(5)))
        .await
        .expect("Timeout connecting to Redis for integration tests")
        .expect("Must successfully connect to redis to run integration tests on 127.0.0.1:6379");
      let _: () = conn.info(None).await.expect(
        "Must fetch redis info prior to performing integration tests to confirm connectivity",
      );
    }
    pool
  }

  async fn create_test_registry<R>(pool: Arc<DynamicRedisPool>) -> RedisRegistry<R> {
    RedisRegistry::<R>::new(
      RedisRegistryConfig {
        renewal_rate: Duration::from_millis(500),
        tunnel_entry_lifetime: Duration::from_secs(1),
        tunnel_id_ref_lifetime: Duration::from_secs(2),
        ..Default::default()
      },
      pool,
    )
  }

  async fn test_items<R>() -> TestReg<R> {
    let pool = Arc::new(create_test_pool().await);
    let registry = create_test_registry(pool.clone()).await;
    let config = registry.config.clone();
    TestReg {
      registry,
      pool,
      config,
    }
  }

  #[tokio::test]
  async fn store_and_destroy() {
    let TestReg { registry, .. } = test_items::<TestEntry>().await;
    let foo_name = TunnelName::new(Uuid::new_v4().to_string());
    let foo = TestEntry {
      name: foo_name.raw().to_owned(),
      id: 12345,
    };
    let ident = registry
      .register(foo_name, &foo)
      .await
      .expect("Registration must succeed");
    let recovered = registry
      .deregister_identifier(ident)
      .await
      .expect("Deregistration must succeed")
      .expect("Must have an instance of the test entry");
    assert_eq!(
      foo, recovered,
      "Deregistration by identifier must return an instance of the deregistered element"
    );
  }

  #[tokio::test]
  async fn expiration_refreshes() {
    let TestReg { registry, .. } = test_items::<TestEntry>().await;
    let foo_name = TunnelName::new(Uuid::new_v4().to_string());
    let foo = TestEntry {
      name: foo_name.raw().to_owned(),
      id: 12345,
    };
    let ident = registry
      .register(foo_name, &foo)
      .await
      .expect("Registration must succeed");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let recovered = registry
      .deregister_identifier(ident)
      .await
      .expect("Deregistration must succeed")
      .expect("Must have an instance of the test entry");
    assert_eq!(
      foo, recovered,
      "Deregistration by identifier must return an instance of the deregistered element"
    );
  }

  /// Verifies that a RID key is preserved and re-emplaced when expired/deleted
  ///
  /// Simulates when a RID key has been expired out from under us-
  /// possibly due to needing to "catch up" after a long pause, or
  /// because the database was wiped out from under us.
  #[tokio::test]
  async fn renewal() {
    let TestReg { registry, pool, .. } = test_items::<TestEntry>().await;
    let foo_name = TunnelName::new(Uuid::new_v4().to_string());
    let foo = TestEntry {
      name: foo_name.raw().to_owned(),
      id: 12345,
    };
    let ident = registry
      .register(foo_name, &foo)
      .await
      .expect("Registration must succeed");

    {
      let rid_key = RedisRegistry::<TestEntry>::tunnel_rid_key(&ident.rid);
      let conn = pool.next_connect(true).await;
      let deleted_count: usize = conn
        .del(rid_key)
        .await
        .expect("Must successfully delete key");
      assert!(
        deleted_count == 1,
        "Delete reported a non-1 value for number of keys deleted"
      );
    }

    tokio::time::sleep(Duration::from_secs(5)).await;
    let recovered = registry
      .deregister_identifier(ident)
      .await
      .expect("Deregistration must succeed")
      .expect("Must have an instance of the test entry");
    assert_eq!(
      foo, recovered,
      "Deregistration by identifier must return an instance of the deregistered entry"
    );
  }

  #[tokio::test]
  async fn cross_registry_lookup() {
    let TestReg {
      registry: reg_a, ..
    } = test_items::<TestEntry>().await;
    let foo_name = TunnelName::new(Uuid::new_v4().to_string());
    let foo = TestEntry {
      name: foo_name.raw().to_owned(),
      id: 12345,
    };
    let ident = reg_a
      .register(foo_name.clone(), &foo)
      .await
      .expect("Registration must succeed");

    let TestReg {
      registry: reg_b, ..
    } = test_items::<TestEntry>().await;

    let found = reg_b
      .lookup(&foo_name)
      .await
      .expect("Lookup must succeed")
      .expect("Must have an instance of the test entry");
    assert_eq!(
      foo, found,
      "Lookup must find an instance of the expected entry"
    );

    reg_a
      .deregister_identifier(ident)
      .await
      .expect("Deregistration must succeed")
      .expect("Must have an instance of the test entry");

    let should_be_empty = reg_b
      .lookup(&foo_name)
      .await
      .expect("Lookup of empty entry must succeed");
    assert_eq!(should_be_empty, None, "Lookup of known-deleted entry should not result in an entry in a known-consistent configuration");
  }

  /// Verifies that renewal/refreshing stops after a registry instance is destroyed
  #[tokio::test]
  async fn registry_core_expiration() {
    let TestReg {
      registry: reg_a,
      config,
      ..
    } = test_items::<TestEntry>().await;
    let foo_name = TunnelName::new(Uuid::new_v4().to_string());
    let foo = TestEntry {
      name: foo_name.raw().to_owned(),
      id: 12345,
    };
    let ident = reg_a
      .register(foo_name.clone(), &foo)
      .await
      .expect("Registration must succeed");

    let TestReg {
      registry: reg_b, ..
    } = test_items::<TestEntry>().await;

    drop(reg_a);

    // Note that we're still holding `ident` alive in order to ensure that core cancellation
    // occurs even if an identifier remains alive, as core cancellation should stop all
    // related background tasks for the [RedisRegistry].

    tokio::time::sleep(config.tunnel_entry_lifetime * 2).await;

    let should_be_empty = reg_b
      .lookup(&foo_name)
      .await
      .expect("Lookup of empty entry must succeed");
    assert_eq!(should_be_empty, None, "Lookup of known-deleted entry should not result in an entry in a known-consistent configuration");

    // Explicitly dropping it here instead of letting a _var hold it less obviously
    drop(ident);
  }

  /// Verifies that renewal/refreshing stops after the last identifier is removed
  #[tokio::test]
  async fn registry_ident_expiration() {
    let TestReg {
      registry: reg_a,
      config,
      ..
    } = test_items::<TestEntry>().await;
    let foo_name = TunnelName::new(Uuid::new_v4().to_string());
    let foo = TestEntry {
      name: foo_name.raw().to_owned(),
      id: 12345,
    };
    let ident = reg_a
      .register(foo_name.clone(), &foo)
      .await
      .expect("Registration must succeed");

    let TestReg {
      registry: reg_b, ..
    } = test_items::<TestEntry>().await;

    // Drop the last identifier for the entry, to stop renewal from occurring for just that key
    drop(ident);

    tokio::time::sleep(config.tunnel_entry_lifetime * 2).await;

    let should_be_empty = reg_b
      .lookup(&foo_name)
      .await
      .expect("Lookup of empty entry must succeed");
    assert_eq!(should_be_empty, None, "Lookup of known-deleted entry should not result in an entry in a known-consistent configuration");
  }
}
