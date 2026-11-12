use ambassador::delegatable_trait;
use wm_common::TilingDirection;
use wm_platform::Direction;

use super::CommonGetters;
use crate::models::{TilingContainer, TilingWindow};

#[delegatable_trait]
pub trait TilingDirectionGetters: CommonGetters {
  fn tiling_direction(&self) -> TilingDirection;

  fn set_tiling_direction(&self, tiling_direction: TilingDirection);

  /// Traverses down a container in search of a descendant in the given
  /// direction. For example, for `Direction::Right`, get the right-most
  /// container.
  ///
  /// Any non-tiling containers and `VoidSlot`s are skipped.
  fn descendant_in_direction(
    &self,
    direction: &Direction,
  ) -> Option<TilingWindow> {
    if self.tiling_direction()
      != TilingDirection::from_direction(direction)
    {
      for child in self.child_focus_order() {
        let Ok(child) = child.as_tiling_container() else {
          continue;
        };
        if let Some(window) =
          descendant_window_from_tiling(child, direction)
        {
          return Some(window);
        }
      }
      return None;
    }

    let ordered: Box<dyn Iterator<Item = TilingContainer> + '_> =
      match direction {
        Direction::Up | Direction::Left => {
          Box::new(self.tiling_children())
        }
        _ => Box::new(
          self.tiling_children().collect::<Vec<_>>().into_iter().rev(),
        ),
      };

    for child in ordered {
      if let Some(window) = descendant_window_from_tiling(child, direction)
      {
        return Some(window);
      }
    }

    None
  }
}

fn descendant_window_from_tiling(
  child: TilingContainer,
  direction: &Direction,
) -> Option<TilingWindow> {
  match child {
    TilingContainer::Split(split_child) => {
      split_child.descendant_in_direction(direction)
    }
    TilingContainer::TilingWindow(window) => Some(window),
    TilingContainer::Void(_) => None,
  }
}

/// Implements the `TilingDirectionGetters` trait for a given struct.
///
/// Expects that the struct has a wrapping `RefCell` containing a struct
/// with a `tiling_direction` field.
#[macro_export]
macro_rules! impl_tiling_direction_getters {
  ($struct_name:ident) => {
    impl TilingDirectionGetters for $struct_name {
      fn tiling_direction(&self) -> TilingDirection {
        self.0.borrow().tiling_direction.clone()
      }

      fn set_tiling_direction(&self, tiling_direction: TilingDirection) {
        self.0.borrow_mut().tiling_direction = tiling_direction;
      }
    }
  };
}
