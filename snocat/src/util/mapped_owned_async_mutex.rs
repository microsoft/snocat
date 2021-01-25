use tokio::sync::{Mutex, OwnedMutexGuard};

pub struct MappedOwnedMutexGuard<U, T> {
  mutex: OwnedMutexGuard<U>,
  mapper: Box<dyn (for<'a> Fn(&'a U) -> &'a T) + Send + 'static>,
}

impl<U, T> MappedOwnedMutexGuard<U, T> {
  pub fn new<FMap: (for<'a> Fn(&'a U) -> &'a T) + Send + 'static>(
    item: OwnedMutexGuard<U>,
    map: FMap,
  ) -> Self {
    Self {
      mutex: item,
      mapper: Box::new(map),
    }
  }

  fn map(&self) -> &T {
    (self.mapper)(&*self.mutex)
  }

  fn map_mut(&mut self) -> &mut T {
    let immutable_ref = self.map();
    // We're mutable on the outside, so we're safe to make mutable on the inside
    #[allow(mutable_transmutes)]
    let mutable_unsafe_ref = unsafe { std::mem::transmute::<&T, &mut T>(immutable_ref) };
    mutable_unsafe_ref
  }

  pub fn extract(self) -> OwnedMutexGuard<U> {
    self.mutex
  }
}

impl<U, T> AsRef<T> for MappedOwnedMutexGuard<U, T> {
  fn as_ref(&self) -> &T {
    self.map()
  }
}

impl<U, T> AsMut<T> for MappedOwnedMutexGuard<U, T> {
  fn as_mut(&mut self) -> &mut T {
    self.map_mut()
  }
}
