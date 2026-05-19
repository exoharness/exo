use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct Uuid7(pub Uuid);

impl Uuid7 {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn timestamp(&self) -> Option<DateTime<Utc>> {
        let timestamp = self.0.get_timestamp()?;
        let (seconds, nanos) = timestamp.to_unix();
        DateTime::from_timestamp(seconds as i64, nanos)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for Uuid7 {
    fn default() -> Self {
        Self::now()
    }
}

impl Display for Uuid7 {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl From<Uuid> for Uuid7 {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

impl From<Uuid7> for Uuid {
    fn from(value: Uuid7) -> Self {
        value.0
    }
}

impl FromStr for Uuid7 {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self)
    }
}

pub trait HasUuid7Id {
    fn uuid7_id(&self) -> Uuid7;

    fn created_at(&self) -> Option<DateTime<Utc>> {
        self.uuid7_id().timestamp()
    }
}

#[macro_export]
macro_rules! impl_has_uuid7_id {
    ($type:ty, $field:ident) => {
        impl $crate::HasUuid7Id for $type {
            fn uuid7_id(&self) -> $crate::Uuid7 {
                self.$field
            }
        }
    };
}
