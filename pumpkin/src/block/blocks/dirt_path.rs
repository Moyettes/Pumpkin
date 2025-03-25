use std::sync::Arc;

use crate::block::pumpkin_block::PumpkinBlock;
use crate::server::Server;
use crate::world::BlockFlags;
use crate::world::World;
use async_trait::async_trait;
use pumpkin_data::block::Block;
use pumpkin_data::block::HorizontalFacing;
use pumpkin_macros::pumpkin_block;
use pumpkin_protocol::server::play::SUseItemOn;
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::block::BlockDirection;
use pumpkin_world::chunk::TickPriority;

#[pumpkin_block("minecraft:dirt_path")]
pub struct DirtPathBlock;

#[async_trait]
impl PumpkinBlock for DirtPathBlock {
    async fn on_scheduled_tick(&self, world: &Arc<World>, _block: &Block, pos: &BlockPos) {
        // TODO: push up entities
        world
            .set_block_state(pos, Block::DIRT.default_state_id, BlockFlags::NOTIFY_ALL)
            .await;
    }

    async fn on_place(
        &self,
        _server: &Server,
        world: &World,
        block: &Block,
        _face: &BlockDirection,
        pos: &BlockPos,
        _use_item_on: &SUseItemOn,
        _player_direction: &HorizontalFacing,
        _other: bool,
    ) -> u16 {
        if !self.can_place_at(world, pos).await {
            return Block::DIRT.default_state_id;
        }
        block.default_state_id
    }

    async fn get_state_for_neighbor_update(
        &self,
        world: &World,
        block: &Block,
        state: u16,
        pos: &BlockPos,
        direction: &BlockDirection,
        _neighbor_pos: &BlockPos,
        _neighbor_state: u16,
    ) -> u16 {
        if direction == &BlockDirection::Up && !self.can_place_at(world, pos).await {
            world
                .schedule_block_tick(block, *pos, 1, TickPriority::Normal)
                .await;
        }
        state
    }

    async fn can_place_at(&self, world: &World, pos: &BlockPos) -> bool {
        let state = world.get_block_state(&pos.up()).await.unwrap();
        !state.is_solid // TODO: add fence gata block
    }
}
