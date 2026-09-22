//! Files a spawn or a message brings with it, named by path.
//!
//! GraphQL carries no bytes: a `POST /graphql` body is JSON, and the `Upload`
//! scalar needs a multipart request this server does not accept on that route.
//! So an attachment here names a file that is already inside the run's working
//! directory, exactly as the REST routes' JSON `parts` list does, and both go
//! through the one reader in
//! [`core::attachments`](crate::commands::serve::core::attachments).

use async_graphql::{Enum, InputObject};
use leviath_core::mime::InboundPart;

use super::super::super::core::attachments;
use super::super::super::core::error::ServeError;
use super::super::inputs::RegionRef;

/// How a part reaches the model that reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum Delivery {
    /// As the bytes themselves, for a model that takes this type natively.
    Native,
    /// As text, extracted from the bytes.
    Text,
    /// As a short description standing in for the bytes, which stay on disk
    /// for a tool to open.
    StandIn,
}

impl From<Delivery> for leviath_core::mime::Delivery {
    fn from(deliver: Delivery) -> Self {
        match deliver {
            Delivery::Native => Self::Native,
            Delivery::Text => Self::Text,
            Delivery::StandIn => Self::StandIn,
        }
    }
}

/// One file to attach, by its path inside the run's working directory.
#[derive(Debug, InputObject)]
pub(crate) struct AttachmentWrite {
    /// The file, relative to the run's working directory. A path that resolves
    /// outside it is refused with `FORBIDDEN`.
    pub(crate) path: String,
    /// The context region to put it in. The region the text it arrives with
    /// lands in, when this is absent.
    pub(crate) region: Option<RegionRef>,
    /// The name the part carries. The file's own name, when this is absent.
    pub(crate) name: Option<String>,
    /// The type of the bytes, when the name and the bytes do not say.
    pub(crate) mime_type: Option<String>,
    /// How it should reach the model. The run decides, when this is absent.
    pub(crate) deliver: Option<Delivery>,
    /// Text stored beside it, which the model reads with it.
    pub(crate) caption: Option<String>,
}

/// Read every attachment a request listed, inside `workdir`.
///
/// One failing file fails the request: a spawn that started with half the
/// files it was given is a run nobody asked for.
pub(crate) fn parts_of(
    listed: Vec<AttachmentWrite>,
    workdir: &std::path::Path,
    max_bytes: u64,
) -> Result<Vec<InboundPart>, ServeError> {
    let mut parts = Vec::with_capacity(listed.len());
    for item in listed {
        let mut part = attachments::read_within(&item.path, workdir, max_bytes)?;
        part.region = item.region.map(|region| region.name);
        if let Some(name) = item.name {
            part.name = name;
        }
        if let Some(declared) = item.mime_type {
            part.mime_type = Some(attachments::mime_type(&item.path, &declared)?);
        }
        part.deliver = item.deliver.map(leviath_core::mime::Delivery::from);
        part.caption = item.caption;
        parts.push(part);
    }
    Ok(parts)
}
