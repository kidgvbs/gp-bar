//! RAII libxpc bindings — the transport the app and the privileged helper
//! talk over.
//!
//! Ownership model (from the SDK headers, `xpc/connection.h` and
//! `xpc/xpc.h`; review findings **B8**, **B9**, **B10** applied here):
//!
//! * [`XpcObject`] — an **owned** xpc object (`+1` reference: created
//!   dictionaries, reply dictionaries). `Drop` releases it. Cannot be
//!   double-released (move semantics).
//! * [`XpcConnection`] — an **owned** connection created by
//!   `xpc_connection_create_mach_service` (`+1` reference). `Drop` cancels
//!   and releases it, so no operation can leak a connection.
//! * [`XpcEvent`] — an object the runtime only **lends** us: the event passed
//!   to a handler block, and (for listeners) the accepted peer connection.
//!   Per `connection.h`: "you do not hold a reference on them" and "ownership
//!   of the event object is NOT implicitly transferred". A borrowed view with
//!   a lifetime, never released.
//! * [`XpcPeer`] — the borrowed accepted connection, with the accept/reject
//!   protocol the header mandates (`set_event_handler`+`resume`, or `cancel`).
//!
//! No public function accepts a raw pointer except the explicitly `unsafe`
//! borrow constructors, so no safe code path can cause UB (B8). Handler blocks
//! are `RcBlock`s that are **dropped after** each `xpc_*` call that takes
//! them: libxpc retains its own copy, so nothing leaks and nothing is freed
//! early (B9). The extern statics use a zero-sized opaque type instead of
//! `c_void` (B10).
//!
//! Signatures are transcribed from
//! `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/xpc/*`
//! — read from the header, not intuited.

use std::ffi::{CStr, c_char, c_void};

use block2::{Block, RcBlock};

/// Raw xpc object pointer, as C sees it. Only ever handled inside this
/// module; the public API exposes owned/borrowed wrappers instead.
pub type XpcRaw = *mut c_void;

/// The handler block type libxpc expects (`xpc_handler_t`):
/// `void (^)(xpc_object_t)`.
pub type HandlerBlock = RcBlock<dyn Fn(XpcRaw)>;

/// Opaque zero-sized type for the extern type singletons (`XPC_TYPE_*`
/// expand to `&_xpc_type_*`). The idiomatic C-opaque-object marker (B10):
/// we only ever take its address.
#[repr(C)]
struct XpcTypeOpaque {
    _private: [u8; 0],
}

// `unsafe extern`: edition 2024 requires the block to be marked unsafe, and
// every call below carries its own justification.
unsafe extern "C" {
    static _xpc_type_connection: XpcTypeOpaque;
    static _xpc_type_dictionary: XpcTypeOpaque;
    static _xpc_type_error: XpcTypeOpaque;

    fn xpc_connection_create_mach_service(
        name: *const c_char,
        targetq: XpcRaw,
        flags: u64,
    ) -> XpcRaw;
    fn xpc_connection_set_event_handler(connection: XpcRaw, handler: XpcRaw);
    fn xpc_connection_resume(connection: XpcRaw);
    fn xpc_connection_cancel(connection: XpcRaw);
    fn xpc_connection_send_message(connection: XpcRaw, message: XpcRaw);
    fn xpc_connection_send_message_with_reply(
        connection: XpcRaw,
        message: XpcRaw,
        replyq: XpcRaw,
        handler: XpcRaw,
    );
    fn xpc_connection_get_euid(connection: XpcRaw) -> u32;

    fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const XpcRaw,
        count: usize,
    ) -> XpcRaw;
    fn xpc_dictionary_create_reply(original: XpcRaw) -> XpcRaw;
    fn xpc_dictionary_get_remote_connection(xdict: XpcRaw) -> XpcRaw;
    fn xpc_dictionary_set_string(xdict: XpcRaw, key: *const c_char, value: *const c_char);
    fn xpc_dictionary_set_int64(xdict: XpcRaw, key: *const c_char, value: i64);
    fn xpc_dictionary_set_bool(xdict: XpcRaw, key: *const c_char, value: bool);
    fn xpc_dictionary_set_value(xdict: XpcRaw, key: *const c_char, value: XpcRaw);
    fn xpc_dictionary_get_string(xdict: XpcRaw, key: *const c_char) -> *const c_char;
    fn xpc_dictionary_get_int64(xdict: XpcRaw, key: *const c_char) -> i64;
    fn xpc_dictionary_get_bool(xdict: XpcRaw, key: *const c_char) -> bool;
    /// Borrowed: "does not grant the caller a reference" (xpc.h). NULL when
    /// the key is absent.
    fn xpc_dictionary_get_value(xdict: XpcRaw, key: *const c_char) -> XpcRaw;

    /// `objects == NULL, count == 0` is the empty array (`XPC_RETURNS_RETAINED`).
    fn xpc_array_create(objects: *const XpcRaw, count: usize) -> XpcRaw;
    fn xpc_array_set_string(xarray: XpcRaw, index: usize, string: *const c_char);
    /// Returns 0 when the object is not an array (xpc.h), so a wrong type is
    /// an empty list rather than a crash.
    fn xpc_array_get_count(xarray: XpcRaw) -> usize;
    /// NULL when the element is not a string. The index MUST be `< count`.
    fn xpc_array_get_string(xarray: XpcRaw, index: usize) -> *const c_char;
    fn xpc_get_type(object: XpcRaw) -> *const c_void;
    fn xpc_release(object: XpcRaw);

    fn xpc_connection_set_target_queue(connection: XpcRaw, queue: XpcRaw);

    fn dispatch_get_global_queue(identifier: isize, flags: u64) -> XpcRaw;
    /// `attr == NULL` is the serial attribute (no concurrent attribute flag).
    fn dispatch_queue_create(label: *const c_char, attr: XpcRaw) -> XpcRaw;
    /// The idiomatic main loop for a libdispatch-based daemon (B11): parks
    /// the calling thread forever; never returns.
    fn dispatch_main() -> !;
}

/// `XPC_CONNECTION_MACH_SERVICE_LISTENER` (connection.h: `1 << 0`).
const XPC_CONNECTION_MACH_SERVICE_LISTENER: u64 = 1 << 0;

// -- owned object ------------------------------------------------------------------

/// An owned xpc object with a `+1` reference, released on drop.
pub struct XpcObject(std::ptr::NonNull<c_void>);

impl XpcObject {
    /// Adopt a `+1` reference returned by libxpc. Private: the only sources
    /// are the FFI calls in this module, so every `XpcObject` is valid by
    /// construction and all public methods are genuinely safe (B8).
    fn from_owned(raw: XpcRaw) -> Option<Self> {
        std::ptr::NonNull::new(raw).map(Self)
    }

    /// A new empty dictionary.
    pub fn dict_new() -> Self {
        let raw = unsafe { xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0) };
        // xpc_dictionary_create with count 0 cannot fail for a valid process.
        Self::from_owned(raw).expect("xpc_dictionary_create returned NULL")
    }

    /// Set a string value. Interior NULs are stripped, never panicked on:
    /// this runs inside C callbacks where a panic aborts the process (B7).
    pub fn set_string(&self, key: &CStr, value: &str) {
        let cleaned = value.replace('\0', "");
        // Unreachable: `cleaned` cannot contain NUL after the replace above.
        let c_value = std::ffi::CString::new(cleaned).expect("interior NUL already stripped");
        unsafe { xpc_dictionary_set_string(self.0.as_ptr(), key.as_ptr(), c_value.as_ptr()) };
    }

    /// Set an integer value.
    pub fn set_int64(&self, key: &CStr, value: i64) {
        unsafe { xpc_dictionary_set_int64(self.0.as_ptr(), key.as_ptr(), value) };
    }

    /// Set a boolean value.
    pub fn set_bool(&self, key: &CStr, value: bool) {
        unsafe { xpc_dictionary_set_bool(self.0.as_ptr(), key.as_ptr(), value) };
    }

    /// Set an array of strings.
    ///
    /// Ownership (B8/B9), mirroring [`XpcObject::set_string`]: the array is
    /// created `+1`, `xpc_dictionary_set_value` retains its own reference,
    /// and our `XpcObject` drops — releasing exactly our `+1` — at the end of
    /// this call. Nothing leaks and the dictionary never holds a dead object.
    /// Elements with an interior NUL are stripped like in `set_string`, never
    /// panicked on (B7).
    pub fn set_string_array(&self, key: &CStr, values: &[String]) {
        let array = unsafe { xpc_array_create(std::ptr::null(), 0) };
        let Some(array) = XpcObject::from_owned(array) else {
            // Cannot allocate an array: send nothing rather than a half-built
            // one. The daemon sees a missing key and fails the arity check.
            return;
        };
        for (index, value) in values.iter().enumerate() {
            let cleaned = value.replace('\0', "");
            // Unreachable: NULs are stripped above.
            let Ok(c_value) = std::ffi::CString::new(cleaned) else {
                continue;
            };
            unsafe { xpc_array_set_string(array.0.as_ptr(), index, c_value.as_ptr()) };
        }
        unsafe { xpc_dictionary_set_value(self.0.as_ptr(), key.as_ptr(), array.0.as_ptr()) };
    }
}

impl Drop for XpcObject {
    fn drop(&mut self) {
        unsafe { xpc_release(self.0.as_ptr()) };
    }
}

// -- owned connection ---------------------------------------------------------------

/// An owned client/listener connection (`+1` reference from
/// `xpc_connection_create_mach_service`). Cancelled and released on drop.
pub struct XpcConnection(std::ptr::NonNull<c_void>);

impl XpcConnection {
    fn from_owned(raw: XpcRaw) -> Option<Self> {
        std::ptr::NonNull::new(raw).map(Self)
    }

    /// A ready client connection to a Mach service: created, given a no-op
    /// event handler (connection-level errors surface through the reply
    /// handler; see `vpn::call_helper`) and activated. Dropping it is always
    /// safe because it is always resumed first.
    pub fn connect(service: &CStr) -> Option<Self> {
        let raw = unsafe {
            xpc_connection_create_mach_service(service.as_ptr(), std::ptr::null_mut(), 0)
        };
        let conn = Self::from_owned(raw)?;
        conn.set_event_handler(noop_handler());
        conn.resume();
        Some(conn)
    }

    /// A listener connection for the daemon's Mach service. NOT activated
    /// here: per the header the caller must install the accept handler first,
    /// then call [`XpcConnection::resume`].
    pub fn listen(service: &CStr) -> Option<Self> {
        let raw = unsafe {
            xpc_connection_create_mach_service(
                service.as_ptr(),
                std::ptr::null_mut(),
                XPC_CONNECTION_MACH_SERVICE_LISTENER,
            )
        };
        Self::from_owned(raw)
    }

    /// Install the event handler block. libxpc retains its own copy; our
    /// `RcBlock` is dropped at the end of this call — no per-connection
    /// block leak (B9).
    pub fn set_event_handler(&self, block: HandlerBlock) {
        let raw = block_ptr(&block);
        unsafe { xpc_connection_set_event_handler(self.0.as_ptr(), raw) };
    }

    /// Activate the connection (listener or client).
    pub fn resume(&self) {
        unsafe { xpc_connection_resume(self.0.as_ptr()) };
    }

    /// Send a message and invoke `block` when the reply (or a connection
    /// error) arrives. The block is retained by libxpc until it fires; our
    /// copy is dropped at the end of this call (B9). Never blocks the
    /// caller, so the client enforces its own timeout (`vpn::call_helper`).
    pub fn send_message_with_reply(&self, message: &XpcObject, block: HandlerBlock) {
        let raw = block_ptr(&block);
        unsafe {
            xpc_connection_send_message_with_reply(
                self.0.as_ptr(),
                message.0.as_ptr(),
                global_queue(),
                raw,
            )
        };
    }
}

impl Drop for XpcConnection {
    fn drop(&mut self) {
        // Cancel first: releasing the last reference on an inactive
        // connection is UB per connection.h, and cancel makes the teardown
        // well-defined. The cancel event fires on the (no-op / final) handler.
        unsafe {
            xpc_connection_cancel(self.0.as_ptr());
            xpc_release(self.0.as_ptr());
        }
    }
}

// -- borrowed event -----------------------------------------------------------------

/// An object the runtime lends to a handler block: a message, an error
/// dictionary, or (for listeners) an accepted connection.
///
/// Owned by libxpc for the duration of the handler invocation; must never
/// be released by us. The lifetime ties it to the handler scope.
pub struct XpcEvent<'a> {
    raw: std::ptr::NonNull<c_void>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> XpcEvent<'a> {
    /// # Safety
    ///
    /// `raw` must be the event object libxpc passed to the currently running
    /// handler block (listener event, peer message, or reply handler): valid
    /// for the duration of the invocation, owned by the runtime (never
    /// released by us). This is the only public constructor, so review B8's
    /// rule holds: the one way to obtain a borrowed event is explicitly
    /// unsafe with its contract spelled out.
    pub unsafe fn from_borrowed(raw: XpcRaw) -> Option<Self> {
        std::ptr::NonNull::new(raw).map(|raw| Self {
            raw,
            _marker: std::marker::PhantomData,
        })
    }

    /// Whether this event is a connection (listener accept).
    pub fn is_connection(&self) -> bool {
        self.type_is(type_connection())
    }

    /// Whether this event is a dictionary (a message or a reply).
    pub fn is_dictionary(&self) -> bool {
        self.type_is(type_dictionary())
    }

    /// The well-known `XPC_ERROR_*` dictionaries (connection broken,
    /// cancelled, interrupted...) are dictionaries too — check this BEFORE
    /// treating an event as a data reply.
    pub fn is_error(&self) -> bool {
        self.type_is(type_error())
    }

    fn type_is(&self, ty: *const c_void) -> bool {
        let actual = unsafe { xpc_get_type(self.raw.as_ptr()) };
        actual == ty
    }

    /// A string value from the dictionary, lossily converted: libxpc owns the
    /// buffer and never hands out an interior NUL there, but a lossy read
    /// cannot panic inside a C callback (B7).
    pub fn get_string(&self, key: &CStr) -> Option<String> {
        let raw = unsafe { xpc_dictionary_get_string(self.raw.as_ptr(), key.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(raw) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// An integer value from the dictionary (`0` when absent).
    pub fn get_int64(&self, key: &CStr) -> i64 {
        unsafe { xpc_dictionary_get_int64(self.raw.as_ptr(), key.as_ptr()) }
    }

    /// A boolean value from the dictionary (`false` when absent).
    pub fn get_bool(&self, key: &CStr) -> bool {
        unsafe { xpc_dictionary_get_bool(self.raw.as_ptr(), key.as_ptr()) }
    }

    /// Every string element of an array value (`[]` when the key is absent,
    /// not an array, or empty). Lossy like [`XpcEvent::get_string`] — a
    /// non-UTF-8 element cannot panic inside a C callback (B7). Non-string
    /// elements are skipped, and the count is read from the array itself, so
    /// the index is always in range.
    pub fn get_string_array(&self, key: &CStr) -> Vec<String> {
        let array = unsafe { xpc_dictionary_get_value(self.raw.as_ptr(), key.as_ptr()) };
        if array.is_null() {
            return Vec::new();
        }
        let count = unsafe { xpc_array_get_count(array) };
        let mut values = Vec::with_capacity(count.min(64));
        for index in 0..count {
            let raw = unsafe { xpc_array_get_string(array, index) };
            if raw.is_null() {
                continue;
            }
            values.push(
                unsafe { CStr::from_ptr(raw) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        values
    }

    /// The accepted connection, when this event is a connection event.
    /// Borrowed: the runtime owns it and disposes of it after it goes
    /// invalid (connection.h) — we must not release it, only accept or
    /// reject it.
    pub fn as_peer(&self) -> Option<XpcPeer<'a>> {
        if self.is_connection() {
            Some(XpcPeer {
                raw: self.raw,
                _marker: std::marker::PhantomData,
            })
        } else {
            None
        }
    }

    /// Create the reply dictionary for this request (`+1`, owned). Can be
    /// called at most once per request: a second call returns NULL.
    pub fn create_reply(&self) -> Option<XpcObject> {
        let raw = unsafe { xpc_dictionary_create_reply(self.raw.as_ptr()) };
        XpcObject::from_owned(raw)
    }

    /// Send `reply` (created with [`XpcEvent::create_reply`]) back on the
    /// connection this request arrived on. The remote connection is only
    /// borrowed from the request, valid while the request is. Returns false
    /// if the request carries no reply context (one-way send).
    pub fn send_reply(&self, reply: &XpcObject) -> bool {
        let remote = unsafe { xpc_dictionary_get_remote_connection(self.raw.as_ptr()) };
        if remote.is_null() {
            return false;
        }
        unsafe { xpc_connection_send_message(remote, reply.0.as_ptr()) };
        true
    }
}

// -- borrowed peer connection --------------------------------------------------------

/// A peer connection accepted through the listener. Borrowed: the runtime
/// owns it; after it goes invalid it delivers one final error event and
/// disposes of itself. The header mandates accept-or-reject, which is the
/// whole public surface here.
pub struct XpcPeer<'a> {
    raw: std::ptr::NonNull<c_void>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl XpcPeer<'_> {
    /// The peer's euid, snapshot at connection time (connection.h).
    pub fn euid(&self) -> u32 {
        unsafe { xpc_connection_get_euid(self.raw.as_ptr()) }
    }

    /// Accept the connection: install its message handler and activate it
    /// (the two actions the header requires). libxpc retains the block (B9).
    pub fn accept(&self, block: HandlerBlock) {
        let raw = block_ptr(&block);
        unsafe {
            xpc_connection_set_event_handler(self.raw.as_ptr(), raw);
            xpc_connection_resume(self.raw.as_ptr());
        }
    }

    /// Reject the connection; the runtime disposes of it.
    pub fn cancel(&self) {
        unsafe { xpc_connection_cancel(self.raw.as_ptr()) };
    }
}

// -- helpers -------------------------------------------------------------------------

/// The `XPC_TYPE_*` singletons as plain pointers (`XPC_TYPE_*` in C expand
/// to `&_xpc_type_*`). Edition 2024: `&raw const` on an extern static takes
/// the address without reading it, so no `unsafe` block is needed here (B10).
fn type_connection() -> *const c_void {
    (&raw const _xpc_type_connection).cast()
}

fn type_dictionary() -> *const c_void {
    (&raw const _xpc_type_dictionary).cast()
}

fn type_error() -> *const c_void {
    (&raw const _xpc_type_error).cast()
}

/// The block pointer to hand to libxpc: blocks are #[repr(C)] structs whose
/// address IS the C block reference.
fn block_ptr(block: &HandlerBlock) -> XpcRaw {
    (&**block as *const Block<dyn Fn(XpcRaw)>).cast_mut().cast()
}

/// The default-priority global concurrent queue for reply handlers.
/// `DISPATCH_QUEUE_PRIORITY_DEFAULT` is 0.
fn global_queue() -> XpcRaw {
    unsafe { dispatch_get_global_queue(0, 0) }
}

/// A no-op event handler for client connections: request/reply clients only
/// ever see connection-level errors here, which the reply handler reports
/// anyway; swallowing them avoids logging the benign cancel event on every
/// ping.
fn noop_handler() -> HandlerBlock {
    RcBlock::new(|_event: XpcRaw| {})
}

/// Park the calling thread inside libdispatch forever — the daemon main
/// loop (B11).
pub fn run_forever() -> ! {
    unsafe { dispatch_main() }
}

// -- dispatch queues ----------------------------------------------------------------

/// A **serial** dispatch queue (review B5: every handler — listener accept,
/// peer message, watchdog bookkeeping — is dispatched through one serial
/// queue, so no two handlers run concurrently).
///
/// Owned by libdispatch for the process's whole life and deliberately never
/// released: it must outlive every connection whose events it dispatches, and
/// `dispatch_main` never returns. The raw handle stays private so no public
/// function accepts a raw pointer (B8).
#[derive(Clone, Copy)]
pub struct SerialQueue(XpcRaw);

impl SerialQueue {
    /// Create the serial queue; the `label` is what `sample` and Instruments
    /// show. `None` only if libdispatch cannot allocate the queue.
    pub fn new(label: &CStr) -> Option<Self> {
        // NULL attributes == serial (no DISPATCH_QUEUE_CONCURRENT).
        let raw = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null_mut()) };
        std::ptr::NonNull::new(raw).map(|raw| Self(raw.as_ptr()))
    }
}

impl XpcConnection {
    /// Route this connection's events onto `queue`. Call BEFORE `resume`: the
    /// header requires the target queue to be set on an inactive connection.
    pub fn set_target_queue(&self, queue: SerialQueue) {
        unsafe { xpc_connection_set_target_queue(self.0.as_ptr(), queue.0) };
    }
}

impl XpcPeer<'_> {
    /// Route this accepted connection's events onto `queue`, before `accept`.
    pub fn set_target_queue(&self, queue: SerialQueue) {
        unsafe { xpc_connection_set_target_queue(self.raw.as_ptr(), queue.0) };
    }
}
