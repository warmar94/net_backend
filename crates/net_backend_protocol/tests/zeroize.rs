//! The secret types overwrite their memory with zeros when they are dropped: a global allocator
//! looks at every freed block for a marker text. A plain `String` with the marker is the control
//! (its freed block still holds the marker). Own test binary: it installs the allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use net_backend_protocol::auth::{LoginRequest, TokenPair};
use net_backend_protocol::{AccessToken, Password, RefreshToken, Secret, UnixMillis};

const MARKER: &str = "zz-wipe-marker-7d41c0";

static ARMED: AtomicBool = AtomicBool::new(false);
static SEEN: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, plus a look at every freed block of up to 4 KiB while armed.
struct Checker;

// SAFETY: every call is forwarded to the system allocator unchanged; `dealloc` only reads the
// block it is about to free (test code: the block may hold bytes that were never written).
unsafe impl GlobalAlloc for Checker {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size();
        if ARMED.load(Ordering::SeqCst) && (MARKER.len()..=4096).contains(&size) {
            let block = unsafe { std::slice::from_raw_parts(ptr, size) };
            if block.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()) {
                SEEN.fetch_add(1, Ordering::SeqCst);
            }
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Checker = Checker;

/// A heap string holding the marker, built in place (no reallocation leaves a copy behind).
fn marked() -> String {
    let mut text = String::with_capacity(64);
    text.push_str(MARKER);
    text.push_str("-tail");
    text
}

/// How many freed blocks held the marker while `work` ran.
fn freed_with_marker(work: impl FnOnce()) -> usize {
    SEEN.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
    work();
    ARMED.store(false, Ordering::SeqCst);
    SEEN.load(Ordering::SeqCst)
}

#[test]
fn every_secret_type_wipes_its_allocation_and_each_clone() {
    // The control: a plain String leaves the marker in the freed block.
    assert_eq!(freed_with_marker(|| drop(marked())), 1, "the check sees an unwiped block");

    let seen = freed_with_marker(|| {
        let access = AccessToken::new(marked());
        let copy = access.clone();
        drop(access);
        drop(copy);
        drop(RefreshToken::new(marked()));
        drop(Password::from(marked()));
        drop(Secret::new(marked()));
        let pair = TokenPair::new(AccessToken::new(marked()), UnixMillis(1), RefreshToken::new(marked()), UnixMillis(2));
        drop(pair.clone());
        drop(pair);
        drop(LoginRequest::new("player@example.com", marked()));
    });
    assert_eq!(seen, 0, "a secret left its text in freed memory");

    // A secret decoded from JSON is its own allocation, wiped too.
    let json = format!("\"{MARKER}\"");
    let seen = freed_with_marker(|| {
        let token: AccessToken = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(token.expose(), MARKER);
        drop(token);
    });
    assert_eq!(seen, 0, "a decoded secret left its text in freed memory");
}
