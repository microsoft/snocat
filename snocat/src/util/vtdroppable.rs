// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
#[repr(C)]
pub struct VTDroppable {
  data: *mut (),
  drop_vtable: unsafe extern "C" fn(*mut ()),
  type_id: std::any::TypeId,
}

unsafe impl Send for VTDroppable {}

impl Drop for VTDroppable {
  fn drop(&mut self) {
    let data = self.data;
    if data.is_null() {
      panic!("Tried to drop a null pointer!");
    }
    let dropper = self.drop_vtable;
    unsafe {
      dropper(self.data);
    }
  }
}

impl VTDroppable {
  pub fn get_raw<T: 'static>(target: T) -> VTDroppable {
    unsafe extern "C" fn drop_t<Y>(v: *mut ()) -> () {
      let x: Box<Y> = Box::from_raw(v as *mut Y);
      drop(x);
    }
    let raw = Box::into_raw(Box::new(target));
    let type_id: std::any::TypeId = std::any::TypeId::of::<T>();
    VTDroppable {
      data: raw as *mut (),
      drop_vtable: drop_t::<T>,
      type_id,
    }
  }

  pub fn into_boxed_drop(self: VTDroppable) -> Box<dyn Drop> {
    Box::new(self)
  }

  pub fn as_typed_ref<T: 'static>(&self) -> &T {
    let type_id = std::any::TypeId::of::<T>();
    let type_name: &'static str = std::any::type_name::<T>();
    if self.type_id != type_id {
      panic!(
        "Undropped ref of type {:?} as incorrect type {:?} ({})",
        &self.type_id, type_id, type_name
      );
    }
    unsafe { &*(self.data as *mut T) }
  }

  pub fn as_typed_ref_mut<T: 'static>(&mut self) -> &mut T {
    let type_id = std::any::TypeId::of::<T>();
    let type_name: &'static str = std::any::type_name::<T>();
    if self.type_id != type_id {
      panic!(
        "Undropped mut ref of type {:?} as incorrect type {:?} ({})",
        &self.type_id, type_id, type_name
      );
    }
    unsafe { &mut *(self.data as *mut T) }
  }

  pub fn try_as_typed_ref<T: 'static>(&self) -> Result<&T, ()> {
    let type_id = std::any::TypeId::of::<T>();
    if self.type_id != type_id {
      Err(())
    } else {
      Ok(unsafe { &*(self.data as *mut T) })
    }
  }

  pub fn try_as_typed_ref_mut<T: 'static>(&mut self) -> Result<&mut T, ()> {
    let type_id = std::any::TypeId::of::<T>();
    if self.type_id != type_id {
      Err(())
    } else {
      Ok(unsafe { &mut *(self.data as *mut T) })
    }
  }

  pub fn extract_typed<T: 'static>(mut self) -> T {
    let type_id = std::any::TypeId::of::<T>();
    let type_name: &'static str = std::any::type_name::<T>();
    if self.type_id != type_id {
      panic!(
        "Extracted from type {:?} as incorrect type {:?} ({})",
        &self.type_id, type_id, type_name
      );
    }
    let x: Box<T> = unsafe { Box::from_raw(self.data as *mut T) };
    self.data = std::ptr::null_mut(); // Clear out the content, it's invalid now
    std::mem::drop(self.type_id); // In case this type actually holds more than just an int
    std::mem::forget(self); // Prevent VTDroppable's Drop from being called on invalid values
    *x
  }

  /// Note that a failed extraction returns the unextracted form, which will be dropped if ignored
  pub fn try_extract_typed<T: 'static>(mut self) -> Result<T, Self> {
    let type_id = std::any::TypeId::of::<T>();
    if self.type_id != type_id {
      return Err(self);
    }
    let x: Box<T> = unsafe { Box::from_raw(self.data as *mut T) };
    self.data = std::ptr::null_mut(); // Clear out the content, it's invalid now
    std::mem::drop(self.type_id); // In case this type actually holds more than just an int
    std::mem::forget(self); // Prevent VTDroppable's Drop from being called on invalid values
    Ok(*x)
  }
}

#[cfg(test)]
mod tests {
  use crate::util::vtdroppable::VTDroppable;

  /// Utility class which executes a callback upon being dropped.
  struct DropIt<F: FnMut() -> ()>(Option<F>);
  impl<F: FnMut() -> ()> Drop for DropIt<F> {
    fn drop(&mut self) {
      if let Some(mut cb) = std::mem::replace(&mut self.0, None) {
        cb();
      }
    }
  }
  impl<F: FnMut() -> ()> DropIt<F> {
    pub fn with(f: F) -> DropIt<F> {
      DropIt(Some(f))
    }
  }

  #[test]
  fn try_verified_referencing() {
    let test_value: &str = "hello world";
    // We don't use mut until later because we want to verify that these don't require mutability
    let vt = VTDroppable::get_raw(test_value);
    assert!(matches!(vt.try_as_typed_ref::<&str>(), Ok(x) if x == &test_value)); // Downcasting
    assert!(matches!(
      vt.try_as_typed_ref::<&dyn std::any::Any>(),
      Err(_)
    )); // Only downcast to Self
    assert!(matches!(vt.try_as_typed_ref::<u32>(), Err(_))); // Checked downcasts for concrete types
    assert_eq!(*vt.as_typed_ref::<&str>(), test_value); // Checked downcasts for concrete types
                                                        // Now try the mutable ones
    let mut vt = vt;
    assert!(matches!(vt.try_as_typed_ref_mut::<&str>(), Ok(x) if x == &test_value));
    assert!(matches!(vt.try_as_typed_ref_mut::<&dyn Drop>(), Err(_)));
    assert!(matches!(vt.try_as_typed_ref_mut::<u32>(), Err(_)));
    assert_eq!(*vt.as_typed_ref_mut::<&str>(), test_value);
  }

  #[test]
  fn try_extraction() {
    let test_value: &str = "hello world";
    assert!(matches!(
      VTDroppable::get_raw(test_value).try_extract_typed::<&str>(),
      Ok(_)
    ));
    assert!(matches!(
      VTDroppable::get_raw(test_value).try_extract_typed::<&dyn Drop>(),
      Err(_)
    ));
    // Verify that extraction failures return an error with the still-packed value intact
    assert_eq!(
      VTDroppable::get_raw(test_value)
        .try_extract_typed::<&dyn Drop>()
        .err()
        .and_then(|x: VTDroppable| x.try_extract_typed::<&str>().ok()),
      Some(test_value)
    );
    assert_eq!(
      VTDroppable::get_raw(test_value).extract_typed::<&str>(),
      test_value
    );
  }

  /// Assert that dropping the opaque container calls the appropriate Drop implementation internally
  #[test]
  fn drops_when_opaque() {
    // Verify DropIt utility...
    {
      let mut has_dropped = false;
      let test_value = DropIt::with(|| {
        has_dropped = true;
      });
      std::mem::drop(test_value);
      assert!(has_dropped);
    }
    // Verify drop using dropper
    {
      // This version uses an Arc/Mutex to work around the 'static requirement of VTDroppable
      let has_dropped = std::sync::Arc::new(std::sync::Mutex::new(false));
      {
        let dropped_writer = has_dropped.clone();
        let test_value = DropIt::with(move || {
          *dropped_writer.lock().unwrap() = true;
        });
        let droppable = VTDroppable::get_raw(test_value);
        std::mem::drop(droppable);
      }
      assert!(*has_dropped.lock().unwrap());
    }
  }
}
