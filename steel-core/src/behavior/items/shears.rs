//! Shears item behavior (`ShearsItem`).
//!
//! Right-clicking a growing plant head that is not at max age crops it, matching
//! vanilla `ShearsItem.useOn`.

use steel_macros::item_behavior;
use steel_registry::{blocks::block_state_ext::BlockStateExt, sound_events, vanilla_game_events};
use steel_utils::types::UpdateFlags;

use crate::behavior::blocks::vegetation::GrowingPlantHeadBlock;
use crate::behavior::{BLOCK_BEHAVIORS, InteractionResult, ItemBehavior, UseOnContext};
use crate::entity::Entity;
use crate::world::game_event::GameEventContext;

/// Behavior for shears.
#[item_behavior]
pub struct ShearsItem;

impl ItemBehavior for ShearsItem {
    fn use_on(&self, context: &mut UseOnContext) -> InteractionResult {
        let pos = context.hit_result.block_pos;
        let state = context.world.get_block_state(pos);
        if !BLOCK_BEHAVIORS
            .get_behavior(state.get_block())
            .is_growing_plant_head()
            || GrowingPlantHeadBlock::is_max_age(state)
        {
            return InteractionResult::Pass;
        }

        context.world.play_block_sound(
            &sound_events::BLOCK_GROWING_PLANT_CROP,
            pos,
            1.0,
            1.0,
            Some(context.player.id()),
        );
        let new_state = GrowingPlantHeadBlock::get_max_age_state(state);
        context
            .world
            .set_block(pos, new_state, UpdateFlags::UPDATE_ALL);
        context.world.game_event(
            &vanilla_game_events::BLOCK_CHANGE,
            pos,
            &GameEventContext::new(Some(context.player), Some(new_state)),
        );

        let has_infinite_materials = context.player.has_infinite_materials();
        context
            .inv
            .with_item(|item| item.hurt_and_break(1, has_infinite_materials));

        InteractionResult::Success
    }
}

#[cfg(test)]
mod tests {
    use steel_registry::{init_vanilla_registry, vanilla_blocks};

    use crate::behavior::{BLOCK_BEHAVIORS, init_behaviors};

    #[test]
    fn growing_plant_heads_are_identified() {
        init_vanilla_registry();
        init_behaviors();

        assert!(
            BLOCK_BEHAVIORS
                .get_behavior(&vanilla_blocks::KELP)
                .is_growing_plant_head()
        );
        assert!(
            BLOCK_BEHAVIORS
                .get_behavior(&vanilla_blocks::CAVE_VINES)
                .is_growing_plant_head()
        );
        assert!(
            BLOCK_BEHAVIORS
                .get_behavior(&vanilla_blocks::WEEPING_VINES)
                .is_growing_plant_head()
        );
        assert!(
            BLOCK_BEHAVIORS
                .get_behavior(&vanilla_blocks::TWISTING_VINES)
                .is_growing_plant_head()
        );
        assert!(
            !BLOCK_BEHAVIORS
                .get_behavior(&vanilla_blocks::KELP_PLANT)
                .is_growing_plant_head()
        );
        assert!(
            !BLOCK_BEHAVIORS
                .get_behavior(&vanilla_blocks::DIRT)
                .is_growing_plant_head()
        );
    }
}
