// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use ffi_support::{rust_string_to_c, ByteBuffer, FfiStr};

const ALLOCATION_TRACING: bool = true;

// define_bytebuffer_destructor!(snocat_free_buffer);
// define_string_destructor!(snocat_free_string);
// console-tracing versions of data destructors and allocators for development
#[no_mangle]
pub extern "C" fn snocat_free_buffer(v: ::ffi_support::ByteBuffer) {
  ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
    if ALLOCATION_TRACING {
      println!(" - byte[{:?}]", v.as_slice().len());
    }
    v.destroy()
  })
}

#[no_mangle]
pub unsafe extern "C" fn snocat_free_string(s: *mut std::os::raw::c_char) {
  ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
    if ALLOCATION_TRACING {
      println!(
        " - STR {:?} : {:?}",
        &s,
        if s.is_null() {
          ""
        } else {
          let sffi = FfiStr::from_raw(s);
          sffi.as_str()
        }
      );
    }
    if !s.is_null() {
      ::ffi_support::destroy_c_string(s)
    }
  });
}
// Allow remote allocation of data

/// Allocate a buffer of the requested size, and return an object with a pointer to it; (size, data)
#[no_mangle]
pub extern "C" fn snocat_alloc_buffer(byte_length: u32) -> ::ffi_support::ByteBuffer {
  ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
    let buf = ::ffi_support::ByteBuffer::new_with_size(byte_length as usize);
    if ALLOCATION_TRACING {
      println!(" + byte[{:?}]", byte_length);
    }
    buf
  })
}

/// Allocate a buffer of the provided data, and return an object with a pointer to it; (size, data)
#[no_mangle]
pub extern "C" fn snocat_alloc_buffer_from(
  start: *const u8,
  byte_length: u32,
) -> ::ffi_support::ByteBuffer {
  ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
    let mut buf = ::ffi_support::ByteBuffer::new_with_size(byte_length as usize);
    if byte_length > 0 {
      let source = unsafe { &*std::ptr::slice_from_raw_parts::<u8>(start, byte_length as usize) };
      buf.as_mut_slice().copy_from_slice(source);
    }
    if ALLOCATION_TRACING {
      println!(" + byte[{:?}]", byte_length);
    }
    buf
  })
}

#[no_mangle]
pub unsafe extern "C" fn snocat_alloc_string(s: FfiStr) {
  ::ffi_support::abort_on_panic::with_abort_on_panic(|| {
    let rs: *mut std::os::raw::c_char = rust_string_to_c(s.as_str());
    if ALLOCATION_TRACING {
      println!(" + STR {:?} : {:?}", &rs, s.as_str());
    }
    rs
  });
}

mod pointers_as_u64 {
  use crate::ffi::allocators::RawByteBuffer;

  pub fn serialize<S: serde::ser::Serializer, T>(ptr: &*const T, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64((*ptr as *const ()) as u64)
  }

  pub fn deserialize<'de, T, D: serde::de::Deserializer<'de>>(
    deserializer: D,
  ) -> Result<*const T, D::Error> {
    let numeric: u64 = serde::de::Deserialize::deserialize(deserializer)?;
    Ok(numeric as *const T)
  }

  #[cfg(test)]
  #[test]
  fn round_trip_deserialize() {
    let x = RawByteBuffer {
      len: 25,
      data: 13u64 as *const _,
    };
    let encoded = serde_json::to_string(&x).unwrap();
    assert_eq!(encoded, "{\"len\":25,\"data\":13}");
    let res: RawByteBuffer = serde_json::from_str(&encoded).unwrap();
    assert_eq!(res.data as u64, 13);
    assert_eq!(res.len, 25);
  }
}

#[repr(C)]
#[derive(::serde::Serialize, ::serde::Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct RawByteBuffer {
  pub len: i64,
  #[serde(with = "pointers_as_u64")]
  pub data: *const u8,
}
impl RawByteBuffer {
  #[inline]
  pub fn destroy(self) {
    let s = unsafe { std::mem::transmute::<RawByteBuffer, ByteBuffer>(self) };
    s.destroy();
  }
}

unsafe impl Send for RawByteBuffer {}

pub fn get_bytebuffer_raw(buffer: ByteBuffer) -> RawByteBuffer {
  unsafe { std::mem::transmute::<ByteBuffer, RawByteBuffer>(buffer) }
}
