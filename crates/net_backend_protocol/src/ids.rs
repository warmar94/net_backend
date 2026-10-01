//! Id newtypes. Every id is a signed 64-bit integer (a `BIGINT` column on every supported
//! database) and travels as a plain JSON number: `{"user_id":42}`.
//!
//! The newtypes keep a user id from being passed where a room id is expected. They are
//! `#[serde(transparent)]`, so the JSON is exactly the number.
//!
//! JavaScript clients: numbers above 2^53 lose precision in `JSON.parse`. Server-generated ids
//! start at 1 and grow by one, so this only matters after 9 quadrillion rows; ids that come from
//! outside (a Steam id) travel as strings instead.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl $name {
            /// The id with this value.
            pub const fn new(value: i64) -> Self {
                Self(value)
            }

            /// The value.
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for i64 {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

id_type!(
    /// A user (account) id.
    UserId
);
id_type!(
    /// A chat room id (public rooms, direct messages and group rooms alike).
    RoomId
);
id_type!(
    /// A chat message id. Ids grow over time within a server, so a newer message has a larger id.
    MessageId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_plain_numbers() {
        assert_eq!(serde_json::to_string(&UserId(42)).ok().as_deref(), Some("42"));
        assert_eq!(serde_json::from_str::<RoomId>("-7").ok(), Some(RoomId(-7)));
        assert_eq!(serde_json::from_str::<MessageId>(&i64::MAX.to_string()).ok(), Some(MessageId(i64::MAX)));
        assert!(serde_json::from_str::<UserId>("\"42\"").is_err());
        assert_eq!(UserId::new(5).get(), 5);
        assert_eq!(i64::from(UserId::from(9)), 9);
        assert_eq!(RoomId(3).to_string(), "3");
    }
}
