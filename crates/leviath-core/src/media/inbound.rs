//! A part on its way in: bytes a caller attached to a task, a region value
//! or a message, before the run's store has them.
//!
//! This is the one shape every ingress speaks. `lev run --attach` builds
//! one from a file, the HTTP API from an upload or a workdir path, the
//! Agent Client Protocol from a content block, a sub-agent spawn from a
//! parent's part. The bytes ride base64 on the newline-JSON control socket,
//! which is a text transport; once a run has them they live in its blob
//! store and only a [`super::BlobRef`] travels further.

use serde::{Deserialize, Serialize};

use super::{Delivery, MediaType};

/// Bytes a caller attached, and where they should land.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InboundPart {
    /// The region to write the part to. `None` means wherever the text it
    /// came with lands: the task region at spawn, the target region of a
    /// message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The part's name, usually the file name it came from.
    pub name: String,
    /// The type the caller declared, when it did. Sniffed otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<MediaType>,
    /// How the part should reach a model, when the caller has a preference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<Delivery>,
    /// Text to write beside the part in the same entry, when the caller gave
    /// some (an `--attach` with no caption writes the part alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// The bytes, base64 on the wire.
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

impl std::fmt::Debug for InboundPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundPart")
            .field("region", &self.region)
            .field("name", &self.name)
            .field("media_type", &self.media_type)
            .field("deliver", &self.deliver)
            .field("caption", &self.caption)
            .field("data", &format_args!("{} bytes", self.data.len()))
            .finish()
    }
}

impl InboundPart {
    /// A part from a file's bytes, named after the file.
    pub fn from_bytes(name: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            region: None,
            name: name.into(),
            media_type: None,
            deliver: None,
            caption: None,
            data,
        }
    }

    /// The same part, bound for `region`.
    pub fn in_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// The same part, with its type declared.
    pub fn typed(mut self, media_type: MediaType) -> Self {
        self.media_type = Some(media_type);
        self
    }

    /// The same part, with a delivery preference.
    pub fn delivered(mut self, deliver: Delivery) -> Self {
        self.deliver = Some(deliver);
        self
    }

    /// The same part, with text to write beside it.
    pub fn captioned(mut self, caption: impl Into<String>) -> Self {
        self.caption = Some(caption.into());
        self
    }
}

/// Bytes as a base64 string, for the transports that carry JSON.
mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_round_trips_as_base64() {
        let part = InboundPart::from_bytes("a.png", vec![1, 2, 3])
            .in_region("art")
            .typed(MediaType::parse("image/png").unwrap())
            .delivered(Delivery::Native)
            .captioned("the hero");
        let json = serde_json::to_string(&part).unwrap();
        assert!(json.contains("\"data\":\"AQID\""), "{json}");
        let back: InboundPart = serde_json::from_str(&json).unwrap();
        assert_eq!(back, part);
        assert!(format!("{part:?}").contains("3 bytes"));
        assert!(!format!("{part:?}").contains("[1, 2, 3]"));
        assert!(serde_json::from_str::<InboundPart>("{\"name\":\"x\",\"data\":\"!!\"}").is_err());
        assert!(serde_json::from_str::<InboundPart>("{\"name\":\"x\",\"data\":5}").is_err());
        let bare: InboundPart = serde_json::from_str("{\"name\":\"x\",\"data\":\"\"}").unwrap();
        assert!(bare.data.is_empty() && bare.region.is_none());
        assert_eq!(InboundPart::default().name, "");
    }
}
