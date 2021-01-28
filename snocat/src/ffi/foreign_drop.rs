use lazy_static::lazy_static;

pub type ForeignDropCallback = unsafe extern "C" fn(ptr: *const ());

lazy_static! {
  static ref DROPPERS: std::sync::RwLock<Vec<ForeignDropCallback>> =
    std::sync::RwLock::new(Vec::new());
}

// Registers a callback that disposes of a remote delegate for an FFI callback
pub fn register_foreign_drop_callback(dropper: ForeignDropCallback) -> () {
  #[cfg(test)]
  {
    println!(
      "(registering foreign dropper callback {:?})",
      &dropper as *const _
    );
  }
  let mut droppers = DROPPERS
    .write()
    .expect("Must be able to lock droppers when registering");
  if !droppers.contains(&dropper) {
    droppers.push(dropper);
  }
}

pub fn foreign_drop(item: *const ()) -> () {
  let droppers = DROPPERS.read().expect("Dropper listing had a panic?");
  let local = droppers.clone();
  drop(droppers);
  tracing::event!(
    tracing::Level::TRACE,
    target = "foreign_drop",
    dropper_count = local.len(),
    ?item,
  );
  for dropper in local {
    unsafe {
      dropper(item);
    }
  }
}

pub trait ForeignDrop {
  type Raw;
  fn from_raw(raw: Self::Raw) -> Self;
}

/// Creates a type that handles Drop on remote FFI delegates via the foreign drop callback system
#[macro_export]
macro_rules! DroppableCallback {
  ($sname:ident, fn($($k:ident:$t: ty),* $(,)?)) => {
    DroppableCallback($sname, fn($($k, $t),*) -> ())
  };
  ($sname:ident, fn($($k:ident:$t: ty),* $(,)?) -> $ret:ty) => {
    #[repr(transparent)]
    pub struct $sname {
      delegate: unsafe extern "C" fn($($k: $t),*) -> $ret,
    }
    impl $sname {
      pub unsafe fn from_raw(delegate: unsafe extern "C" fn($($k: $t),*) -> $ret) -> Self {
        Self { delegate }
      }
      pub fn invoke(&self, $($k: $t),*) -> $ret {
        unsafe { (self.delegate)($($k),*) }
      }
    }
    impl $crate::ffi::foreign_drop::ForeignDrop for $sname {
      type Raw = unsafe extern "C" fn($($t),*) -> $ret;
      fn from_raw(delegate: Self::Raw) -> Self {
        Self { delegate }
      }
    }
    impl std::fmt::Debug for $sname {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
          f,
          "({} :f {:?}",
          stringify!($sname),
          &self.delegate as *const _ as *const (),
        )
      }
    }
    impl Drop for $sname {
      fn drop(&mut self) -> () {
        $crate::ffi::foreign_drop::foreign_drop(&self.delegate as *const _ as *const ());
      }
    }
    unsafe impl Send for $sname {}
    unsafe impl Sync for $sname {}
    impl std::convert::From<unsafe extern "C" fn($($t),*) -> $ret> for $sname {
      fn from(delegate: unsafe extern "C" fn($($k: $t),*) -> $ret) -> Self {
        Self { delegate }
      }
    }
    impl std::convert::From<extern "C" fn($($t),*) -> $ret> for $sname {
      fn from(delegate: extern "C" fn($($k: $t),*) -> $ret) -> Self {
        Self { delegate }
      }
    }
  };
}
