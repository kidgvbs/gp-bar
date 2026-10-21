//! Minimal raw libxpc bindings: dictionary request/reply protocol between
//! GP Bar and its `gp-helper` LaunchDaemon.
//!
//! Symbols are taken from the macOS SDK headers (`/usr/include/xpc`). The
//! `XPC_TYPE_*` macros expand to `&_xpc_type_*`, so the underlying objects
//! are linkable extern statics.
//!
//! The `xpc_*` symbols are re-exported through libSystem (see libSystem.tbd
//! in the SDK), which rustc links by default — no explicit `#[link]` is
//! needed, and there is no `-lxpc` to be found in the Command Line Tools SDK.
//!
//! `xpc_connection_send_message_with_reply_sync` must only be called from a
//! plain thread, never from inside an XPC event handler block (see
//! connection.h) — every call site here runs through `spawn_blocking`.

// Not every binding/wrapper is used by every consumer of this module (app
// side vs daemon side): they are all part of the same libxpc surface.
// The type names intentionally mirror the C API.
#![allow(dead_code)]
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void};

use block2::RcBlock;

pub type xpc_object_t = *mut c_void;
pub type xpc_connection_t = *mut c_void;
pub type xpc_type_t = *const c_void;
pub type dispatch_queue_t = *mut c_void;

pub const XPC_CONNECTION_MACH_SERVICE_LISTENER: u64 = 1 << 0;

// (already covered by the module-level allow above)
#[allow(dead_code)]
extern "C" {
  // Type singletons. In C: `#define XPC_TYPE_DICTIONARY (&_xpc_type_dictionary)`.
  static _xpc_type_connection: c_void;
  static _xpc_type_dictionary: c_void;
  static _xpc_type_error: c_void;

  fn xpc_connection_create_mach_service(
    name: *const c_char,
    targetq: dispatch_queue_t,
    flags: u64,
  ) -> xpc_connection_t;
  fn xpc_connection_set_event_handler(conn: xpc_connection_t, handler: *mut c_void);
  fn xpc_connection_resume(conn: xpc_connection_t);
  fn xpc_connection_cancel(conn: xpc_connection_t);
  fn xpc_connection_send_message(conn: xpc_connection_t, msg: xpc_object_t);
  fn xpc_connection_send_message_with_reply_sync(
    conn: xpc_connection_t,
    msg: xpc_object_t,
  ) -> xpc_object_t;
  /// Audit session id of the peer. See `proto` docs for the (limited) use
  /// the daemon makes of it.
  fn xpc_connection_get_asid(conn: xpc_connection_t) -> u32;

  fn xpc_dictionary_create(
    keys: *const *const c_char,
    values: *const xpc_object_t,
    count: usize,
  ) -> xpc_object_t;
  fn xpc_dictionary_create_reply(request: xpc_object_t) -> xpc_object_t;
  fn xpc_dictionary_get_remote_connection(request: xpc_object_t) -> xpc_connection_t;
  fn xpc_dictionary_set_string(dict: xpc_object_t, key: *const c_char, value: *const c_char);
  fn xpc_dictionary_set_int64(dict: xpc_object_t, key: *const c_char, value: i64);
  fn xpc_dictionary_set_bool(dict: xpc_object_t, key: *const c_char, value: bool);
  fn xpc_dictionary_get_string(dict: xpc_object_t, key: *const c_char) -> *const c_char;
  fn xpc_dictionary_get_int64(dict: xpc_object_t, key: *const c_char) -> i64;
  fn xpc_dictionary_get_bool(dict: xpc_object_t, key: *const c_char) -> bool;
  fn xpc_get_type(object: xpc_object_t) -> xpc_type_t;
  fn xpc_release(object: xpc_object_t);
}

pub fn is_dictionary(object: xpc_object_t) -> bool {
  unsafe { !object.is_null() && xpc_get_type(object) == &_xpc_type_dictionary }
}

pub fn is_connection(object: xpc_object_t) -> bool {
  unsafe { !object.is_null() && xpc_get_type(object) == &_xpc_type_connection }
}

pub fn is_error(object: xpc_object_t) -> bool {
  unsafe { !object.is_null() && xpc_get_type(object) == &_xpc_type_error }
}

/// The daemon-side listener for the Mach service (on demand via launchd).
pub fn create_listener(service: &std::ffi::CStr) -> xpc_connection_t {
  unsafe {
    xpc_connection_create_mach_service(
      service.as_ptr(),
      std::ptr::null_mut(),
      XPC_CONNECTION_MACH_SERVICE_LISTENER,
    )
  }
}

/// The app-side client connection to the same Mach service.
pub fn create_client(service: &std::ffi::CStr) -> xpc_connection_t {
  unsafe { xpc_connection_create_mach_service(service.as_ptr(), std::ptr::null_mut(), 0) }
}

/// Set the event handler to a block that stays alive for the process
/// lifetime. libxpc copies handler blocks; leaking our side is always safe.
pub fn set_event_handler(conn: xpc_connection_t, block: RcBlock<dyn Fn(xpc_object_t)>) {
  let raw = &*block as *const block2::Block<dyn Fn(xpc_object_t)> as *mut c_void;
  std::mem::forget(block);
  unsafe { xpc_connection_set_event_handler(conn, raw) };
}

pub fn resume(conn: xpc_connection_t) {
  unsafe { xpc_connection_resume(conn) };
}

pub fn cancel(conn: xpc_connection_t) {
  unsafe { xpc_connection_cancel(conn) };
}

pub fn dict_new() -> xpc_object_t {
  unsafe { xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0) }
}

pub fn dict_set_string(dict: xpc_object_t, key: &std::ffi::CStr, value: &str) {
  let value = std::ffi::CString::new(value).expect("no interior NUL");
  unsafe { xpc_dictionary_set_string(dict, key.as_ptr(), value.as_ptr()) };
}

pub fn dict_set_bool(dict: xpc_object_t, key: &std::ffi::CStr, value: bool) {
  unsafe { xpc_dictionary_set_bool(dict, key.as_ptr(), value) };
}

pub fn dict_get_string(dict: xpc_object_t, key: &std::ffi::CStr) -> Option<String> {
  let raw = unsafe { xpc_dictionary_get_string(dict, key.as_ptr()) };
  if raw.is_null() {
    return None;
  }
  Some(
    unsafe { std::ffi::CStr::from_ptr(raw) }
      .to_string_lossy()
      .into_owned(),
  )
}

pub fn dict_get_int64(dict: xpc_object_t, key: &std::ffi::CStr) -> i64 {
  unsafe { xpc_dictionary_get_int64(dict, key.as_ptr()) }
}

pub fn dict_get_bool(dict: xpc_object_t, key: &std::ffi::CStr) -> bool {
  unsafe { xpc_dictionary_get_bool(dict, key.as_ptr()) }
}

/// Fire-and-forget send (used by the daemon to reply on the remote
/// connection).
pub fn send(conn: xpc_connection_t, msg: xpc_object_t) {
  unsafe { xpc_connection_send_message(conn, msg) };
}

/// Blocking request/reply. Must run on a plain thread, never from inside an
/// event handler block (per connection.h).
pub fn send_with_reply_sync(conn: xpc_connection_t, msg: xpc_object_t) -> xpc_object_t {
  unsafe { xpc_connection_send_message_with_reply_sync(conn, msg) }
}

pub fn make_reply(request: xpc_object_t) -> xpc_object_t {
  unsafe { xpc_dictionary_create_reply(request) }
}

pub fn remote_connection(request: xpc_object_t) -> xpc_connection_t {
  unsafe { xpc_dictionary_get_remote_connection(request) }
}

pub fn reply_set_int64(reply: xpc_object_t, key: &std::ffi::CStr, value: i64) {
  unsafe { xpc_dictionary_set_int64(reply, key.as_ptr(), value) };
}

pub fn asid(conn: xpc_connection_t) -> u32 {
  unsafe { xpc_connection_get_asid(conn) }
}

pub fn release(object: xpc_object_t) {
  unsafe { xpc_release(object) };
}
