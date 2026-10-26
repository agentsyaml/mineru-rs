use crate::official::{OfficialError, OfficialErrorKind};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

macro_rules! default_scalar_visits {
    ($value:ty) => {
        fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(<$value>::default())
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(<$value>::default())
        }
        fn visit_i128<E>(self, _: i128) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(<$value>::default())
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(<$value>::default())
        }
        fn visit_u128<E>(self, _: u128) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(<$value>::default())
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(<$value>::default())
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(<$value>::default())
        }
    };
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RestoreRouteHint {
    suffix: Option<String>,
    producer_name: Option<String>,
}

impl RestoreRouteHint {
    pub(crate) fn is_native_csv(&self) -> bool {
        matches!(self.suffix.as_deref(), Some("csv" | "tsv"))
            && self.producer_name.as_deref() == Some("mineru-rs")
    }
}

pub(crate) fn inspect_json(json: &str) -> Result<RestoreRouteHint, OfficialError> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let hint = RestoreRouteHint::deserialize(&mut deserializer)
        .map_err(|e| OfficialError::new(OfficialErrorKind::InvalidInput, e.to_string()))?;
    deserializer
        .end()
        .map_err(|e| OfficialError::new(OfficialErrorKind::InvalidInput, e.to_string()))?;
    Ok(hint)
}

pub(crate) async fn inspect_json_bounded(
    json: String,
    timeout: Duration,
) -> Result<(RestoreRouteHint, String), OfficialError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut guard = super::csv::RestoreCancelGuard(Some(cancelled.clone()));
    let task_cancelled = cancelled.clone();
    let task = tokio::task::spawn_blocking(move || {
        if task_cancelled.load(Ordering::Relaxed) {
            return Err(OfficialError::new(
                OfficialErrorKind::Cancelled,
                "restore inspection cancelled",
            ));
        }
        inspect_json(&json).map(|hint| (hint, json))
    });
    let result = match tokio::time::timeout(timeout, task).await {
        Ok(result) => {
            result.map_err(|e| OfficialError::new(OfficialErrorKind::Upstream, e.to_string()))?
        }
        Err(_) => {
            return Err(OfficialError::new(
                OfficialErrorKind::Deadline,
                "restore inspection deadline exceeded",
            ));
        }
    };
    guard.disarm();
    result
}

impl<'de> Deserialize<'de> for RestoreRouteHint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(RootVisitor)
    }
}

struct RootVisitor;

impl<'de> Visitor<'de> for RootVisitor {
    type Value = RestoreRouteHint;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut hint = RestoreRouteHint::default();
        while let Some(key) = map.next_key::<String>()? {
            if key == "metadata" {
                let metadata: MetadataHint = map.next_value()?;
                hint = RestoreRouteHint {
                    suffix: metadata.suffix,
                    producer_name: metadata.producer_name,
                };
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(hint)
    }

    default_scalar_visits!(RestoreRouteHint);

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(RestoreRouteHint::default())
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(RestoreRouteHint::default())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(RestoreRouteHint::default())
    }
}

#[derive(Default)]
struct MetadataHint {
    suffix: Option<String>,
    producer_name: Option<String>,
}

impl<'de> Deserialize<'de> for MetadataHint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(MetadataVisitor)
    }
}

struct MetadataVisitor;

impl<'de> Visitor<'de> for MetadataVisitor {
    type Value = MetadataHint;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("document metadata")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut hint = MetadataHint::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "file_suffix" => hint.suffix = map.next_value::<OptionalString>()?.0,
                "producer" => hint.producer_name = map.next_value::<ProducerHint>()?.name,
                _ => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok(hint)
    }

    default_scalar_visits!(MetadataHint);
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(MetadataHint::default())
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(MetadataHint::default())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(MetadataHint::default())
    }
}

#[derive(Default)]
struct ProducerHint {
    name: Option<String>,
}

impl<'de> Deserialize<'de> for ProducerHint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(ProducerVisitor)
    }
}

struct ProducerVisitor;

impl<'de> Visitor<'de> for ProducerVisitor {
    type Value = ProducerHint;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a producer object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut hint = ProducerHint::default();
        while let Some(key) = map.next_key::<String>()? {
            if key == "name" {
                hint.name = map.next_value::<OptionalString>()?.0;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(hint)
    }

    default_scalar_visits!(ProducerHint);
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(ProducerHint::default())
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(ProducerHint::default())
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(ProducerHint::default())
    }
}

#[derive(Default)]
struct OptionalString(Option<String>);

impl<'de> Deserialize<'de> for OptionalString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OptionalStringVisitor)
    }
}

struct OptionalStringVisitor;

impl<'de> Visitor<'de> for OptionalStringVisitor {
    type Value = OptionalString;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string or another JSON value")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(OptionalString(Some(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(OptionalString(Some(value)))
    }

    default_scalar_visits!(OptionalString);

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(OptionalString(None))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(OptionalString(None))
    }
}
