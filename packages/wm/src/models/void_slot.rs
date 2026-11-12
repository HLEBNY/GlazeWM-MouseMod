use std::{
  cell::{Ref, RefCell, RefMut},
  collections::VecDeque,
  rc::Rc,
};

use anyhow::Context;
use uuid::Uuid;
use wm_common::{ContainerDto, GapsConfig, TilingDirection, VoidSlotDto};
use wm_platform::Rect;

use crate::{
  impl_common_getters, impl_container_debug,
  impl_position_getters_as_resizable, impl_tiling_size_getters,
  models::{
    Container, DirectionContainer, TilingContainer, WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters,
  },
};

#[derive(Clone)]
pub struct VoidSlot(Rc<RefCell<VoidSlotInner>>);

struct VoidSlotInner {
  id: Uuid,
  parent: Option<Container>,
  children: VecDeque<Container>,
  child_focus_order: VecDeque<Uuid>,
  tiling_size: f32,
  gaps_config: GapsConfig,
}

impl VoidSlot {
  /// Creates a detached `VoidSlot` placeholder for an empty tiling slot.
  #[must_use]
  pub fn new(gaps_config: GapsConfig) -> Self {
    let slot = VoidSlotInner {
      id: Uuid::new_v4(),
      parent: None,
      children: VecDeque::new(),
      child_focus_order: VecDeque::new(),
      tiling_size: 1.0,
      gaps_config,
    };

    Self(Rc::new(RefCell::new(slot)))
  }

  /// Converts this `VoidSlot` into its IPC DTO representation.
  pub fn to_dto(&self) -> anyhow::Result<ContainerDto> {
    let rect = self.to_rect()?;

    Ok(ContainerDto::Void(VoidSlotDto {
      id: self.id(),
      parent_id: self.parent().map(|parent| parent.id()),
      tiling_size: self.tiling_size(),
      width: rect.width(),
      height: rect.height(),
      x: rect.x(),
      y: rect.y(),
    }))
  }
}

impl_container_debug!(VoidSlot);
impl_common_getters!(VoidSlot);
impl_tiling_size_getters!(VoidSlot);
impl_position_getters_as_resizable!(VoidSlot);
