#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TunnelId(u64);

impl TunnelId {
  pub fn new(inner: u64) -> TunnelId {
    Self(inner)
  }

  pub fn inner(&self) -> u64 {
    self.0
  }
}

impl From<u64> for TunnelId {
  fn from(inner: u64) -> Self {
    Self::new(inner)
  }
}

impl Into<u64> for TunnelId {
  fn into(self) -> u64 {
    self.inner()
  }
}

pub trait TunnelIDGenerator {
  fn next(&self) -> TunnelId;
}

pub struct MonotonicAtomicGenerator {
  next: std::sync::atomic::AtomicU64,
}

impl MonotonicAtomicGenerator {
  pub fn new(next: u64) -> Self {
    Self {
      next: std::sync::atomic::AtomicU64::new(next),
    }
  }

  pub fn next(&self) -> TunnelId {
    TunnelId::new(self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
  }
}

impl TunnelIDGenerator for MonotonicAtomicGenerator {
  fn next(&self) -> TunnelId {
    MonotonicAtomicGenerator::next(&self)
  }
}

impl std::fmt::Debug for TunnelId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TunnelID")
      .field("inner", &self.inner())
      .finish()
  }
}
