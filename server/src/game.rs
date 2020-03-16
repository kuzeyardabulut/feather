use crate::chunk_entities::{
    on_chunk_cross_update_chunk_entities, on_entity_despawn_update_chunk_entities, ChunkEntities,
};
use crate::chunk_logic::{ChunkHolders, ChunkUnloadQueue, ChunkWorkerHandle};
use crate::config::Config;
use crate::io::{NetworkIoManager, NewClientInfo};
use crate::join::{on_chunk_send_join_player, on_player_join_send_join_game};
use crate::network::Network;
use crate::view::{
    on_chunk_cross_update_chunks, on_chunk_load_send_to_clients,
    on_player_join_trigger_chunk_cross, ChunksToSend,
};
use crate::{chunk_logic, player};
use bumpalo::Bump;
use feather_blocks::Block;
use feather_core::level::LevelData;
use feather_core::world::ChunkMap;
use feather_core::{BlockPosition, ChunkPosition, Packet, Position};
use fecs::{Entity, IntoQuery, Read, World};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

/// Uber-resource storing almost all data needed to run the game.
///
/// This type includes the chunk map for accessing blocks,
/// time data, acceleration structures, and practically all
/// game state, except entities, which are stored in the `World`.
pub struct Game {
    /// The IO handle.
    pub io_handle: NetworkIoManager,
    /// The server configuration.
    pub config: Arc<Config>,
    /// The server tick count, measured in ticks
    /// since program startup.
    pub tick_count: u64,
    /// The server player count.
    ///
    /// (This value is stored in an `Arc` so it can
    /// be shared with the status ping threads.)
    pub player_count: Arc<AtomicU32>,
    /// Information about the world, such as spawn position
    /// and world type.
    pub level: LevelData,
    /// The chunk map.
    pub chunk_map: ChunkMap,
    /// Bump allocator. Reset every tick.
    pub bump: Bump,
    /// Chunk worker handle used for communication with
    /// the chunk worker.
    pub chunk_worker_handle: ChunkWorkerHandle,
    /// Queue of chunks to be unloaded.
    pub chunk_unload_queue: ChunkUnloadQueue,
    pub chunk_holders: ChunkHolders,
    pub chunks_to_send: ChunksToSend,
    pub chunk_entities: ChunkEntities,
}

impl Game {
    /// Retrieves the block at the given position,
    /// or `None` if the block's chunk is not loaded.
    pub fn block_at(&self, pos: BlockPosition) -> Option<Block> {
        self.chunk_map.block_at(pos)
    }

    /// Sets the block at the given position.
    ///
    /// If the block's chunk's is not loaded, returns `false`;
    /// otherwise, returns `true`.
    pub fn set_block_at(&mut self, world: &mut World, pos: BlockPosition, block: Block) -> bool {
        let old_block = match self.block_at(pos) {
            Some(block) => block,
            None => return false,
        };

        self.on_block_update(world, pos, old_block, block);

        self.chunk_map.set_block_at(pos, block)
    }

    /// Despawns an entity. This should be used instead of `World::despawn`
    /// as it properly handles events.
    pub fn despawn(&mut self, entity: Entity, world: &mut World) {
        self.on_entity_despawn(world, entity);
        world.despawn(entity);
    }

    /// Spawns a player with the given `PlayerInfo`.
    pub fn spawn_player(&mut self, info: NewClientInfo, world: &mut World) {
        let entity = player::create(world, info);
        self.on_player_join(world, entity);
    }

    /* UTILITY FUNCTIONS */
    /// Broadcasts a packet to all online players.
    pub fn broadcast_global(&self, world: &World, packet: impl Packet, neq: Option<Entity>) {
        self.broadcast_global_boxed(world, Box::new(packet), neq);
    }

    /// Broadcasts a boxed packet to all online players.
    pub fn broadcast_global_boxed(
        &self,
        world: &World,
        packet: Box<dyn Packet>,
        neq: Option<Entity>,
    ) {
        for (entity, network) in <Read<Network>>::query().iter_entities(world.inner()) {
            if neq.map(|neq| neq == entity).unwrap_or(false) {
                continue;
            }

            network.send_boxed(packet.box_clone());
        }
    }

    /// Broadcasts a packet to all players able to see a given chunk.
    pub fn broadcast_chunk_update(
        &self,
        world: &World,
        packet: impl Packet,
        chunk: ChunkPosition,
        neq: Option<Entity>,
    ) {
        self.broadcast_chunk_update_boxed(world, Box::new(packet), chunk, neq);
    }

    /// Broadcasts a boxed packet to all players able to see a given chunk.
    pub fn broadcast_chunk_update_boxed(
        &self,
        world: &World,
        packet: Box<dyn Packet>,
        chunk: ChunkPosition,
        neq: Option<Entity>,
    ) {
        // we can use the chunk holders structure to accelerate this
        for entity in self.chunk_holders.holders_for(chunk) {
            if neq.map(|neq| neq == *entity).unwrap_or(false) {
                continue;
            }

            if let Some(network) = world.try_get::<Network>(*entity) {
                network.send_boxed(packet.box_clone());
            }
        }
    }

    /// Broadcasts a packet to all players able to see a given entity.
    pub fn broadcast_entity_update(
        &self,
        world: &World,
        packet: impl Packet,
        entity: Entity,
        neq: Option<Entity>,
    ) {
        self.broadcast_entity_update_boxed(world, Box::new(packet), entity, neq);
    }

    /// Broadcasts a boxed packet to all players able to see a given entity.
    pub fn broadcast_entity_update_boxed(
        &self,
        world: &World,
        packet: Box<dyn Packet>,
        entity: Entity,
        neq: Option<Entity>,
    ) {
        // Send the packet to all players who have a hold on the entity's chunk.
        let entity_chunk = world.get::<Position>(entity).chunk();
        self.broadcast_chunk_update_boxed(world, packet, entity_chunk, neq);
    }

    /* EVENT HANDLERS */
    /// Called when a block is updated.
    pub fn on_block_update(
        &mut self,
        _world: &mut World,
        _pos: BlockPosition,
        _old: Block,
        _new: Block,
    ) {
    }

    /// Called when an entity is despawned/removed.
    ///
    /// Note that this is called __before__ the entity is deleted from the world.
    /// As such, components of the entity can still be accessed.
    pub fn on_entity_despawn(&mut self, world: &mut World, entity: Entity) {
        chunk_logic::on_entity_despawn_remove_chunk_holder(self, world, entity);
        on_entity_despawn_update_chunk_entities(self, world, entity);
    }

    /// Called when a player joins.
    pub fn on_player_join(&mut self, world: &mut World, player: Entity) {
        on_player_join_trigger_chunk_cross(self, world, player);
        on_player_join_send_join_game(self, world, player);
    }

    /// Called when a chunk loads successfully.
    pub fn on_chunk_load(&mut self, world: &mut World, chunk: ChunkPosition) {
        on_chunk_load_send_to_clients(self, world, chunk);
    }

    /// Called when a chunk fails to load.
    pub fn on_chunk_load_fail(&mut self, _world: &mut World, _chunk: ChunkPosition) {}

    /// Called when a chunk holder is released.
    pub fn on_chunk_holder_release(&mut self, chunk: ChunkPosition, _holder: Entity) {
        chunk_logic::on_chunk_holder_release_unload_chunk(self, chunk);
    }

    /// Called when an entity crosses into a new chunk.
    pub fn on_chunk_cross(
        &mut self,
        world: &mut World,
        entity: Entity,
        old: Option<ChunkPosition>,
        new: ChunkPosition,
    ) {
        on_chunk_cross_update_chunks(self, world, entity, old, new);
        on_chunk_cross_update_chunk_entities(self, entity, old, new);
    }

    /// Called when a chunk is sent to a client.
    pub fn on_chunk_send(&self, world: &mut World, chunk: ChunkPosition, player: Entity) {
        on_chunk_send_join_player(self, world, chunk, player);
    }
}

#[system]
pub fn increment_tick_count(game: &mut Game) {
    game.tick_count += 1;
}
