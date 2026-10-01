//! Cursor pagination: a list endpoint answers a [`Page`] with up to `limit` items and, when there
//! are more, an opaque [`Cursor`] to pass back for the next page.
//!
//! Cursors (not offsets) because lists such as chat history grow while a client pages through
//! them: an offset would skip or repeat items, a cursor ("everything older than this") does not.
//! A cursor is opaque: clients pass it back unchanged and never build or parse one.
//!
//! Over HTTP the request is a query string (`?cursor=…&limit=50`); over WebSocket the same fields
//! sit in the request's `data`.

use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ValidationDetails};

/// The number of items a page holds when the request names no `limit`.
pub const DEFAULT_PAGE_LIMIT: u32 = 50;

/// The largest `limit` a request may ask for (larger values are clamped by [`PageRequest::limit_or_default`]).
pub const MAX_PAGE_LIMIT: u32 = 100;

/// The longest cursor a server issues or accepts, in bytes.
pub const MAX_CURSOR_BYTES: usize = 512;

/// An opaque position in a list, issued by the server. Pass it back unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(String);

impl Cursor {
    /// A cursor with this text (servers create them; clients only pass them back).
    pub fn new(cursor: impl Into<String>) -> Self {
        Self(cursor.into())
    }

    /// The cursor text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which page to fetch: the cursor of the previous page (none = the first page) and how many
/// items at most.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PageRequest {
    /// Where to continue (`next_cursor` of the previous page); `None` for the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// At most this many items (default [`DEFAULT_PAGE_LIMIT`], at most [`MAX_PAGE_LIMIT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl PageRequest {
    /// The first page with the default limit.
    pub fn first() -> Self {
        Self::default()
    }

    /// The page after `cursor`.
    pub fn after(cursor: Cursor) -> Self {
        Self { cursor: Some(cursor), limit: None }
    }

    /// The same request with this limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The rule: a cursor is at most [`MAX_CURSOR_BYTES`] (an over-long limit is clamped, not an error).
    pub fn validate(&self) -> Result<(), ApiError> {
        let mut details = ValidationDetails::new();
        if self.cursor.as_ref().is_some_and(|c| c.as_str().len() > MAX_CURSOR_BYTES) {
            details.add("cursor", format!("is longer than {MAX_CURSOR_BYTES} bytes"));
        }
        details.into_result()
    }

    /// The limit to apply: the requested one clamped to `1..=MAX_PAGE_LIMIT`, or
    /// [`DEFAULT_PAGE_LIMIT`].
    pub fn limit_or_default(&self) -> u32 {
        self.limit.map_or(DEFAULT_PAGE_LIMIT, |limit| limit.clamp(1, MAX_PAGE_LIMIT))
    }
}

/// One page of a list.
///
/// JSON: `{"items":[…],"next_cursor":"…"}`; `next_cursor` is absent (or `null`) on the last page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Page<T> {
    /// The items, in the order the endpoint documents (chat history: newest first).
    pub items: Vec<T>,
    /// The cursor for the next page; `None` when this is the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}

impl<T> Page<T> {
    /// A page with these items and, if there are more, the cursor of the next page.
    pub fn new(items: Vec<T>, next_cursor: Option<Cursor>) -> Self {
        Self { items, next_cursor }
    }

    /// Whether this is the last page.
    pub fn is_last(&self) -> bool {
        self.next_cursor.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_clamped() {
        assert_eq!(PageRequest::first().limit_or_default(), DEFAULT_PAGE_LIMIT);
        assert_eq!(PageRequest::first().with_limit(0).limit_or_default(), 1);
        assert_eq!(PageRequest::first().with_limit(10_000).limit_or_default(), MAX_PAGE_LIMIT);
        assert_eq!(PageRequest::after(Cursor::new("c")).with_limit(7).limit_or_default(), 7);
        assert!(PageRequest::after(Cursor::new("c".repeat(MAX_CURSOR_BYTES))).validate().is_ok());
        assert!(PageRequest::after(Cursor::new("c".repeat(MAX_CURSOR_BYTES + 1))).validate().is_err());
    }

    #[test]
    fn page_json() {
        let page = Page::new(vec![1, 2], Some(Cursor::new("abc")));
        assert_eq!(serde_json::to_string(&page).ok().as_deref(), Some(r#"{"items":[1,2],"next_cursor":"abc"}"#));
        let last: Page<i32> = serde_json::from_str(r#"{"items":[]}"#).unwrap_or_else(|_| Page::new(vec![9], None));
        assert!(last.is_last() && last.items.is_empty());
        let null_cursor: Option<Page<i32>> = serde_json::from_str(r#"{"items":[3],"next_cursor":null}"#).ok();
        assert_eq!(null_cursor, Some(Page::new(vec![3], None)));
        assert_eq!(serde_json::to_string(&PageRequest::first()).ok().as_deref(), Some("{}"));
    }
}
