//! Ordered, borrowed JSON objects at the v2 boundary. In particular, sorting
//! the API map or cache write TTLs would change which value is first.

use std::fmt;

use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

pub(super) struct RawProvider<'a> {
    pub fields: Object<'a>,
    pub models: Vec<(String, &'a RawValue)>,
}

impl<'a> RawProvider<'a> {
    pub fn parse(raw: &'a RawValue) -> serde_json::Result<Self> {
        let fields = Object::parse(raw)?;
        let models = Object::parse(fields.required("models")?)?.0;
        Ok(Self { fields, models })
    }
}

pub(super) struct RawModel<'a> {
    pub fields: Object<'a>,
}

impl<'a> RawModel<'a> {
    pub fn parse(raw: &'a RawValue) -> serde_json::Result<Self> {
        let fields = Object::parse(raw)?;
        // V1 can carry `type` but has neither of these v2 object members. Each
        // row must have the v2 shape, not just the first row in the document.
        for member in ["type", "capabilities", "api"] {
            fields.required(member)?;
        }
        Ok(Self { fields })
    }
}

pub(super) struct Object<'a>(pub Vec<(String, &'a RawValue)>);

impl<'a> Object<'a> {
    pub fn parse(raw: &'a RawValue) -> serde_json::Result<Self> {
        serde_json::from_str(raw.get())
    }

    pub fn raw(&self, name: &str) -> Option<&'a RawValue> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, raw)| *raw)
    }

    pub fn required(&self, name: &str) -> serde_json::Result<&'a RawValue> {
        self.raw(name)
            .ok_or_else(|| serde::de::Error::custom(format_args!("missing v2 member {name:?}")))
    }

    pub fn read<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        serde_json::from_str(self.raw(name)?.get()).ok()
    }

    pub fn object(&self, name: &str) -> Option<Self> {
        Self::parse(self.raw(name)?).ok()
    }

    pub fn array(&self, name: &str) -> Option<Vec<&'a RawValue>> {
        serde_json::from_str(self.raw(name)?.get()).ok()
    }
}

impl<'de> Deserialize<'de> for Object<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedObject;

        impl<'de> Visitor<'de> for OrderedObject {
            type Value = Object<'de>;

            fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt.write_str("a JSON object")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut fields = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    fields.push(entry);
                }
                Ok(Object(fields))
            }
        }

        deserializer.deserialize_map(OrderedObject)
    }
}
