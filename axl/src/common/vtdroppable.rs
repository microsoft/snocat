#[repr(C)]
pub struct VTDroppable {
  data: *mut (),
  drop_vtable: unsafe extern "C" fn(*mut ()),
  type_id: std::any::TypeId,
}

unsafe impl Send for VTDroppable { }

impl Drop for VTDroppable {
  fn drop(&mut self) {
    let data = self.data;
    if data.is_null() {
      panic!("Tried to drop a null pointer!");
    }
    let dropper = self.drop_vtable;
    unsafe { dropper(self.data); }
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
    VTDroppable { data: raw as *mut (), drop_vtable: drop_t::<T>, type_id }
  }

  pub fn into_boxed_drop(self: VTDroppable) -> Box<dyn Drop> {
    Box::new(self)
  }

  pub fn as_typed_ref<T: 'static>(&self) -> &T {
    let type_id = std::any::TypeId::of::<T>();
    let type_name: &'static str = std::any::type_name::<T>();
    if self.type_id != type_id {
      panic!(format!("Undropped ref of type {:?} as incorrect type {:?} ({})", &self.type_id, type_id, type_name));
    }
    unsafe { &*(self.data as *mut T) }
  }

  pub fn as_typed_ref_mut<T: 'static>(&mut self) -> &mut T {
    let type_id = std::any::TypeId::of::<T>();
    let type_name: &'static str = std::any::type_name::<T>();
    if self.type_id != type_id {
      panic!(format!("Undropped mut ref of type {:?} as incorrect type {:?} ({})", &self.type_id, type_id, type_name));
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
      panic!(format!("Extracted from type {:?} as incorrect type {:?} ({})", &self.type_id, type_id, type_name));
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
