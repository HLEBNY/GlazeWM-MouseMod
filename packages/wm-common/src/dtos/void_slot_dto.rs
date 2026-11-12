use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// User-friendly representation of a void slot container.
///
/// Used for IPC and debug logging.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoidSlotDto {
  pub id: Uuid,
  pub parent_id: Option<Uuid>,
  pub tiling_size: f32,
  pub width: i32,
  pub height: i32,
  pub x: i32,
  pub y: i32,
}
