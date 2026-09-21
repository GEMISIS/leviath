//! The `putYoloProfiles` field: replacing the whole yolo profiles file.

use super::super::error::IntoGraphql;
use super::super::types::machine::YoloProfiles;

/// Replace the yolo profiles file.
///
/// The whole file, because the file is the unit: `--yolo=<name>` names a
/// profile inside it and the profiles refer to each other, so writing one at a
/// time would let a save leave the set inconsistent. Parsed before it is
/// written, so a file that would not load is refused rather than saved and
/// discovered at the next spawn.
pub(crate) async fn put_yolo_profiles(text: String) -> async_graphql::Result<YoloProfiles> {
    super::super::super::yolo::write_profiles(&text).gql()?;
    Ok(super::super::query::yolo_profiles())
}
