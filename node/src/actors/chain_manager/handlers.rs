use actix::{fut::WrapFuture, prelude::*};
use futures::future::Future;
use std::{
    collections::BTreeMap,
    collections::{HashMap, HashSet},
    convert::TryFrom,
    net::SocketAddr,
};

use witnet_data_structures::{
    chain::{
        get_utxo_info, Block, ChainState, CheckpointBeacon, DataRequestInfo, DataRequestReport,
        Epoch, Hash, Hashable, NodeStats, PublicKeyHash, Reputation, SuperBlockVote, UtxoInfo,
    },
    error::{ChainInfoError, TransactionError::DataRequestNotFound},
    transaction::{DRTransaction, Transaction, VTTransaction},
    types::LastBeacon,
};
use witnet_util::timestamp::get_timestamp;
use witnet_validations::validations::validate_rad_request;

use super::{transaction_factory, ChainManager, ChainManagerError, StateMachine, SyncTarget};
use crate::{
    actors::{
        chain_manager::BlockCandidate,
        messages::{
            AddBlocks, AddCandidates, AddCommitReveal, AddSuperBlockVote, AddTransaction,
            Broadcast, BuildDrt, BuildVtt, EpochNotification, GetBalance, GetBlocksEpochRange,
            GetDataRequestReport, GetHighestCheckpointBeacon, GetMemoryTransaction, GetMempool,
            GetMempoolResult, GetNodeStats, GetReputation, GetReputationAll, GetReputationStatus,
            GetReputationStatusResult, GetState, GetSuperBlockVotes, GetUtxoInfo, PeersBeacons,
            SendLastBeacon, SessionUnitResult, SetLastBeacon, TryMineBlock,
        },
        sessions_manager::SessionsManager,
    },
    storage_mngr,
    utils::mode_consensus,
};

pub const SYNCED_BANNER: &str = r"
███████╗██╗   ██╗███╗   ██╗ ██████╗███████╗██████╗ ██╗
██╔════╝╚██╗ ██╔╝████╗  ██║██╔════╝██╔════╝██╔══██╗██║
███████╗ ╚████╔╝ ██╔██╗ ██║██║     █████╗  ██║  ██║██║
╚════██║  ╚██╔╝  ██║╚██╗██║██║     ██╔══╝  ██║  ██║╚═╝
███████║   ██║   ██║ ╚████║╚██████╗███████╗██████╔╝██╗
╚══════╝   ╚═╝   ╚═╝  ╚═══╝ ╚═════╝╚══════╝╚═════╝ ╚═╝
╔════════════════════════════════════════════════════╗
║ This node has finished bootstrapping and is now    ║
║ working at full steam in validating transactions,  ║
║ proposing blocks and resolving data requests.      ║
╟────────────────────────────────────────────────────╢
║ You can now sit back and enjoy Witnet.             ║
╟────────────────────────────────────────────────────╢
║ Wait... Are you still there? You want more fun?    ║
║ Go to https://docs.witnet.io/node-operators/cli/   ║
║ to learn how to monitor the progress of your node  ║
║ (balance, reputation, proposed blocks, etc.)       ║
╚════════════════════════════════════════════════════╝";

////////////////////////////////////////////////////////////////////////////////////////
// ACTOR MESSAGE HANDLERS
////////////////////////////////////////////////////////////////////////////////////////

/// Payload for the notification for all epochs
#[derive(Clone, Debug)]
pub struct EveryEpochPayload;

/// Handler for EpochNotification<EveryEpochPayload>
impl Handler<EpochNotification<EveryEpochPayload>> for ChainManager {
    type Result = ();

    #[allow(clippy::cognitive_complexity)]
    fn handle(&mut self, msg: EpochNotification<EveryEpochPayload>, ctx: &mut Context<Self>) {
        log::debug!("Periodic epoch notification received {:?}", msg.checkpoint);
        let current_timestamp = get_timestamp();
        log::debug!(
            "Timestamp diff: {}, Epoch timestamp: {}. Current timestamp: {}",
            current_timestamp as i64 - msg.timestamp as i64,
            msg.timestamp,
            current_timestamp
        );

        let last_checked_epoch = self.current_epoch;
        let current_epoch = msg.checkpoint;
        self.current_epoch = Some(current_epoch);

        log::debug!(
            "EpochNotification received while StateMachine is in state {:?}",
            self.sm_state
        );
        let chain_beacon = self.get_chain_beacon();
        log::debug!(
            "Chain state ---> checkpoint: {}, hash_prev_block: {}",
            chain_beacon.checkpoint,
            chain_beacon.hash_prev_block
        );

        // Clear pending transactions HashSet
        self.transactions_pool.clear_pending_transactions();

        // Handle case consensus not achieved
        if !self.peers_beacons_received {
            log::warn!("No beacon messages received from peers. Moving to WaitingConsensus state");
            self.sm_state = StateMachine::WaitingConsensus;
            // Clear candidates
            self.candidates.clear();
            self.seen_candidates.clear();
        }

        if let Some(last_checked_epoch) = last_checked_epoch {
            if msg.checkpoint - last_checked_epoch != 1 {
                log::warn!(
                    "Missed epoch notification {}. Moving to WaitingConsensus state",
                    last_checked_epoch + 1
                );
                self.sm_state = StateMachine::WaitingConsensus;
            }
        }

        self.peers_beacons_received = false;
        match self.sm_state {
            StateMachine::WaitingConsensus => {
                if let Some(chain_info) = &self.chain_state.chain_info {
                    // Send last beacon because otherwise the network cannot bootstrap
                    let sessions_manager = SessionsManager::from_registry();
                    let last_beacon = LastBeacon {
                        highest_block_checkpoint: chain_info.highest_block_checkpoint,
                        highest_superblock_checkpoint: self.get_superblock_beacon(),
                    };
                    sessions_manager.do_send(SetLastBeacon {
                        beacon: last_beacon.clone(),
                    });
                    sessions_manager.do_send(Broadcast {
                        command: SendLastBeacon { last_beacon },
                        only_inbound: true,
                    });
                }
            }
            StateMachine::Synchronizing => {}
            StateMachine::AlmostSynced | StateMachine::Synced => {
                match self.chain_state {
                    ChainState {
                        reputation_engine: Some(ref mut rep_engine),
                        ..
                    } => {
                        if self.epoch_constants.is_none()
                            || self.vrf_ctx.is_none()
                            || self.secp.is_none()
                        {
                            log::error!("{}", ChainManagerError::ChainNotReady);
                            return;
                        }

                        // Consolidate the best candidate
                        if let Some(BlockCandidate {
                            block,
                            utxo_diff,
                            reputation: _,
                            vrf_proof: _,
                        }) = self.best_candidate.take()
                        {
                            // Persist block and update ChainState
                            self.consolidate_block(ctx, block, utxo_diff);
                        } else if msg.checkpoint > 0 {
                            let previous_epoch = msg.checkpoint - 1;
                            log::warn!(
                                "There was no valid block candidate to consolidate for epoch {}",
                                previous_epoch
                            );

                            // Update ActiveReputationSet in case of epochs without blocks
                            if let Err(e) = rep_engine.ars_mut().update(vec![], previous_epoch) {
                                log::error!(
                                    "Error updating empty reputation with no blocks: {}",
                                    e
                                );
                            }
                        }

                        // Send last beacon on block consolidation
                        let sessions_manager = SessionsManager::from_registry();
                        let beacon = self
                            .chain_state
                            .chain_info
                            .as_ref()
                            .unwrap()
                            .highest_block_checkpoint;
                        let superblock_beacon = self.get_superblock_beacon();
                        let last_beacon = LastBeacon {
                            highest_block_checkpoint: beacon,
                            highest_superblock_checkpoint: superblock_beacon,
                        };
                        sessions_manager.do_send(SetLastBeacon {
                            beacon: last_beacon.clone(),
                        });
                        sessions_manager.do_send(Broadcast {
                            command: SendLastBeacon { last_beacon },
                            only_inbound: true,
                        });

                        // TODO: Review time since commits are clear and new ones are received before to mining
                        // Remove commits because they expire every epoch
                        self.transactions_pool.clear_commits();

                        // Mining
                        if self.mining_enabled {
                            // Block mining is now triggered by SessionsManager on peers beacon timeout
                            // Data request mining MUST finish BEFORE the block has been mined!!!!
                            // The transactions must be included into this block, both the transactions from
                            // our node and the transactions from other nodes
                            self.try_mine_data_request(ctx);
                        }

                        // Clear candidates
                        self.candidates.clear();
                        self.seen_candidates.clear();

                        log::debug!(
                            "Transactions pool size: {} value transfer, {} data request",
                            self.transactions_pool.vt_len(),
                            self.transactions_pool.dr_len()
                        );
                    }

                    _ => {
                        log::error!("No ChainInfo loaded in ChainManager");
                    }
                }
            }
        }

        self.peers_beacons_received = false;
    }
}

/// Handler for GetHighestBlockCheckpoint message
impl Handler<GetHighestCheckpointBeacon> for ChainManager {
    type Result = Result<CheckpointBeacon, failure::Error>;

    fn handle(
        &mut self,
        _msg: GetHighestCheckpointBeacon,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        if let Some(chain_info) = &self.chain_state.chain_info {
            Ok(chain_info.highest_block_checkpoint)
        } else {
            log::error!("No ChainInfo loaded in ChainManager");

            Err(ChainInfoError::ChainInfoNotFound.into())
        }
    }
}

/// Handler for GetSuperBlockVotes message
impl Handler<GetSuperBlockVotes> for ChainManager {
    type Result = Result<HashSet<SuperBlockVote>, failure::Error>;

    fn handle(&mut self, _msg: GetSuperBlockVotes, _ctx: &mut Context<Self>) -> Self::Result {
        Ok(self
            .chain_state
            .superblock_state
            .get_current_superblock_votes())
    }
}

/// Handler for GetNodeStats message
impl Handler<GetNodeStats> for ChainManager {
    type Result = Result<NodeStats, failure::Error>;

    fn handle(&mut self, _msg: GetNodeStats, _ctx: &mut Context<Self>) -> Self::Result {
        Ok(self.chain_state.node_stats.clone())
    }
}

/// Handler for AddBlocks message
impl Handler<AddBlocks> for ChainManager {
    type Result = SessionUnitResult;

    fn handle(&mut self, mut msg: AddBlocks, ctx: &mut Context<Self>) {
        log::debug!(
            "AddBlocks received while StateMachine is in state {:?}",
            self.sm_state
        );

        let consensus_constants = self.consensus_constants();

        match self.sm_state {
            StateMachine::WaitingConsensus => {
                // In WaitingConsensus state, only allow AddBlocks when the argument is
                // the genesis block
                if msg.blocks.len() == 1 && msg.blocks[0].hash() == consensus_constants.genesis_hash
                {
                    let block = msg.blocks.into_iter().next().unwrap();
                    match self.process_requested_block(ctx, block) {
                        Ok(()) => {
                            log::debug!("Successfully consolidated genesis block");

                            // Set last beacon because otherwise the network cannot bootstrap
                            let sessions_manager = SessionsManager::from_registry();
                            let last_beacon = LastBeacon {
                                highest_block_checkpoint: self.get_chain_beacon(),
                                highest_superblock_checkpoint: self.get_superblock_beacon(),
                            };
                            sessions_manager.do_send(SetLastBeacon {
                                beacon: last_beacon,
                            });
                        }
                        Err(e) => log::error!("Failed to consolidate genesis block: {}", e),
                    }
                }
            }
            StateMachine::Synchronizing => {
                if self.sync_target.is_none() {
                    log::warn!("Target Beacon is None");
                    // If we are not synchronizing, forget about when we started synchronizing
                    if self.sm_state != StateMachine::Synchronizing {
                        self.sync_waiting_for_add_blocks_since = None;
                    }
                    return;
                }

                if msg.blocks.is_empty() {
                    log::debug!("Received an empty AddBlocks message");
                    self.sm_state = StateMachine::WaitingConsensus;
                    self.initialize_from_storage(ctx);
                    log::info!("Restored chain state from storage");
                    // If we are not synchronizing, forget about when we started synchronizing
                    if self.sm_state != StateMachine::Synchronizing {
                        self.sync_waiting_for_add_blocks_since = None;
                    }
                    return;
                }

                let sync_target = self.sync_target.clone().unwrap();
                let superblock_period = u32::from(consensus_constants.superblock_period);

                // Hack: remove first block
                // For some reason the other node sends one extra block, and we cannot consolidate
                // it because it is our top block.
                msg.blocks.remove(0);
                /*
                // Hack: remove genesis block
                if msg.blocks[0].block_header.beacon.checkpoint == 0 && msg.blocks[0].block_header.beacon.hash_prev_block == consensus_constants.bootstrap_hash && msg.blocks[0].hash() == consensus_constants.genesis_hash {
                    msg.blocks.remove(0);
                }
                */

                let (blocks1, blocks2, blocks3, blocks4, epoch2) = split_blocks_batch_at_target(
                    msg.blocks,
                    self.current_epoch.unwrap(),
                    &sync_target,
                    superblock_period,
                );

                // * Process blocks1
                // * If target not reached, done
                // * Create target superblock
                // * Consolidate chain state and persist blocks1
                // * Process blocks2
                // * Persist blocks2
                // * Create superblock (target+1)
                // * Process blocks3

                let (batch_succeeded, num_processed_blocks) =
                    self.process_blocks_batch(ctx, &sync_target, &blocks1);

                if !batch_succeeded {
                    // This branch will happen if this node has forked, but the network has
                    // a valid consensus. In that case we would want to restore the node to
                    // the state just before the fork, and restart the synchronization.

                    // This branch could also happen when one peer has sent us an invalid block batch.
                    // Ideally we would mark it as a bad peer and restart the
                    // synchronization process, but that's not implemented yet.
                    // Note that in order to correctly restart the synchronization process,
                    // restoring the chain state from storage is not enough,
                    // as that storage was overwritten at the end of the last successful batch.

                    // In any case, the current behavior is to go back to WaitingConsensus
                    // state and restart the synchronization on the next PeersBeacons message.
                    log::error!("Received invalid blocks batch");
                    self.sm_state = StateMachine::WaitingConsensus;

                    // If we are not synchronizing, forget about when we started synchronizing
                    if self.sm_state != StateMachine::Synchronizing {
                        self.sync_waiting_for_add_blocks_since = None;
                    }
                    return;
                }

                let mut epoch_of_the_last_block = None;
                if blocks2.is_some() {
                    if num_processed_blocks == 0 {
                        log::debug!("1 Sync done, 0 blocks processed");
                    } else {
                        let last_processed_block = &blocks1[num_processed_blocks - 1];
                        epoch_of_the_last_block =
                            Some(last_processed_block.block_header.beacon.checkpoint);
                        log::debug!("1 Sync done up to block #{} because that is the last checkpoint according to superblock #{}", epoch_of_the_last_block.unwrap(), sync_target.superblock.checkpoint);
                    }
                } else {
                    // Request next blocks batch
                    log::debug!("1 Target not reached, request blocks batch");
                    self.request_blocks_batch(ctx);
                    return;
                }

                let blocks2 = blocks2.unwrap_or_default();
                let blocks3 = blocks3.unwrap_or_default();

                // We need to construct the last superblock if the current superblock
                // is different from the target superblock
                let current_superblock_checkpoint =
                    self.chain_state.superblock_state.get_beacon().checkpoint;
                let need_to_construct_superblock =
                    sync_target.superblock.checkpoint != current_superblock_checkpoint;
                let epoch_during_which_we_should_construct_the_target_superblock =
                    sync_target.superblock.checkpoint * superblock_period;

                // We need to construct the target superblock in order to be able to
                // validate the next superblocks
                if need_to_construct_superblock {
                    // Update ARS if there were no blocks right before the epoch during
                    // which we should construct the target superblock
                    if let Some(epoch_of_the_last_block) = epoch_of_the_last_block {
                        if epoch_during_which_we_should_construct_the_target_superblock != epoch_of_the_last_block + 1 {
                            log::debug!("Updating reputation for empty blocks between {} and {}", epoch_of_the_last_block, epoch_during_which_we_should_construct_the_target_superblock);
                            let rep_engine = self.chain_state.reputation_engine.as_mut().unwrap();

                            if let Err(e) = rep_engine.ars_mut().update_empty(epoch_during_which_we_should_construct_the_target_superblock) {
                                log::error!("Error updating reputation before processing block: {}", e);
                            }
                        }
                    } else {
                        log::debug!("Updating reputation for empty blocks between {} and {}", "<unknown>", epoch_during_which_we_should_construct_the_target_superblock);
                        let rep_engine = self.chain_state.reputation_engine.as_mut().unwrap();

                        if let Err(e) = rep_engine.ars_mut().update_empty(epoch_during_which_we_should_construct_the_target_superblock) {
                            log::error!("Error updating reputation before processing block: {}", e);
                        }
                    }
                    // We need to persist blocks in order to be able to construct the
                    // superblock
                    // TODO: verify that superblock creation does not start until all
                    // the blocks have been persisted
                    self.persist_blocks_batch(ctx, blocks1);
                    let to_be_stored =
                        self.chain_state.data_request_pool.finished_data_requests();
                    self.persist_data_requests(ctx, to_be_stored);
                    // Create superblocks while synchronizing but do not broadcast them
                    // This is needed to ensure that we can validate the received superblocks later on
                    // TODO: this is needed to check synchronization target
                    log::debug!("Will construct superblock during synchronization. Superblock index: {} Epoch {}", sync_target.superblock.checkpoint, epoch_during_which_we_should_construct_the_target_superblock);
                    actix::fut::Either::A(self.construct_superblock(ctx, epoch_during_which_we_should_construct_the_target_superblock)
                        .and_then({ let sync_target = sync_target.clone(); move |superblock, act, ctx| {
                            if superblock.hash() == sync_target.superblock.hash_prev_block {
                                // In synchronizing state, the consensus beacon is the one we just created
                                act.chain_state.chain_info.as_mut().unwrap().highest_superblock_checkpoint =
                                    act.chain_state.superblock_state.get_beacon();
                                log::info!("Consensus while sync! Superblock {:?}", act.get_superblock_beacon());
                                // Persist current_chain_state with
                                // current_superblock_state and the new superblock beacon
                                act.last_chain_state = act.chain_state.clone();
                                act.persist_chain_state(ctx);

                                actix::fut::ok(())
                            } else {
                                // The superblock hash is different from what it should be.
                                log::error!("Mismatching superblock. Target: {:?} Created #{} {} {:?}", sync_target, superblock.index, superblock.hash(), superblock);
                                act.sm_state = StateMachine::WaitingConsensus;
                                act.initialize_from_storage(ctx);
                                log::info!("Restored chain state from storage");

                                // If we are not synchronizing, forget about when we started synchronizing
                                if act.sm_state != StateMachine::Synchronizing {
                                    act.sync_waiting_for_add_blocks_since = None;
                                }
                                actix::fut::err(())
                            }
                        }}))
                } else {
                    // No need to construct a superblock again,
                    actix::fut::Either::B(actix::fut::ok(()))
                }
                    .and_then(move |(), act, ctx| {
                        // Process remaining blocks
                        let (batch_succeeded, num_processed_blocks) = act.process_blocks_batch(ctx, &sync_target, &blocks2);

                        if !batch_succeeded {
                            act.sm_state = StateMachine::WaitingConsensus;
                            // TODO: return actix error
                            return actix::fut::err(());
                        }

                        if num_processed_blocks == 0 {
                            log::debug!("2 Sync done, 0 blocks processed");
                        } else {
                            let last_processed_block = &blocks2[num_processed_blocks - 1];
                            epoch_of_the_last_block = Some(last_processed_block.block_header.beacon.checkpoint);
                            log::debug!("2 Sync done up to block #{}", epoch_of_the_last_block.unwrap());
                        }

                        if let Some(second_superblock_index) = epoch2 {
                            // Update ARS if there were no blocks right before the epoch during
                            // which we should construct the target superblock
                            let epoch_during_which_we_should_construct_the_second_superblock = second_superblock_index * superblock_period;
                            if let Some(epoch_of_the_last_block) = epoch_of_the_last_block {
                                if epoch_during_which_we_should_construct_the_second_superblock != epoch_of_the_last_block + 1 {
                                    log::debug!("Updating reputation for empty blocks between {} and {}", epoch_of_the_last_block, epoch_during_which_we_should_construct_the_second_superblock);
                                    let rep_engine = act.chain_state.reputation_engine.as_mut().unwrap();

                                    if let Err(e) = rep_engine.ars_mut().update_empty(epoch_during_which_we_should_construct_the_second_superblock) {
                                        log::error!("Error updating reputation before processing block: {}", e);
                                    }
                                }
                            } else {
                                log::debug!("Updating reputation for empty blocks between {} and {}", "<unknown>", epoch_during_which_we_should_construct_the_second_superblock);
                                let rep_engine = act.chain_state.reputation_engine.as_mut().unwrap();

                                if let Err(e) = rep_engine.ars_mut().update_empty(epoch_during_which_we_should_construct_the_second_superblock) {
                                    log::error!("Error updating reputation before processing block: {}", e);
                                }
                            }
                            // We need to persist blocks in order to be able to construct the
                            // superblock
                            // TODO: verify that superblock creation does not start until all
                            // the blocks have been persisted
                            act.persist_blocks_batch(ctx, blocks2);
                            let to_be_stored =
                                act.chain_state.data_request_pool.finished_data_requests();
                            act.persist_data_requests(ctx, to_be_stored);

                            log::info!("Block sync target achieved, go to WaitingConsensus state");
                            // Target achived, go back to state 1
                            act.sm_state = StateMachine::WaitingConsensus;

                            // We must construct the second superblock in order to
                            // be able to validate the votes for this superblock
                            // later
                            log::debug!("Will construct the second superblock during synchronization. Superblock index: {} Epoch {}", sync_target.superblock.checkpoint + 1, epoch_during_which_we_should_construct_the_second_superblock);
                            actix::fut::Either::A(act.construct_superblock(ctx, epoch_during_which_we_should_construct_the_second_superblock).map(|_superblock, _, _| {
                                // Ignore the created superblock: do not
                                // broadcast votes
                            }))
                        } else {
                            actix::fut::Either::B(actix::fut::ok(()))
                        }.and_then(move |(), act, ctx| {
                            // Process remaining blocks
                            let (batch_succeeded, num_processed_blocks) = act.process_blocks_batch(ctx, &sync_target, &blocks3);

                            if !batch_succeeded {
                                log::error!("3 Received invalid blocks batch");
                                act.sm_state = StateMachine::WaitingConsensus;

                                // If we are not synchronizing, forget about when we started synchronizing
                                if act.sm_state != StateMachine::Synchronizing {
                                    act.sync_waiting_for_add_blocks_since = None;
                                }
                                return actix::fut::err(());
                            }

                            if num_processed_blocks == 0 {
                                log::debug!("3 Sync done, 0 blocks processed");
                            } else {
                                let last_processed_block = &blocks3[num_processed_blocks - 1];
                                let epoch_of_the_last_block = Some(last_processed_block.block_header.beacon.checkpoint);
                                log::debug!("3 Sync done up to block #{}", epoch_of_the_last_block.unwrap());
                            }

                            log::info!("Block sync target achieved, go to WaitingConsensus state");
                            // Target achived, go back to state 1
                            act.sm_state = StateMachine::WaitingConsensus;

                            if blocks4.is_some() {
                                log::error!("This sync batch will not work because this node is one superblock behind, retry");
                            }

                            actix::fut::ok(())
                        }).wait(ctx);

                        actix::fut::ok(())
                    })
                    .wait(ctx);
            }
            StateMachine::AlmostSynced | StateMachine::Synced => {}
        };

        // If we are not synchronizing, forget about when we started synchronizing
        if self.sm_state != StateMachine::Synchronizing {
            self.sync_waiting_for_add_blocks_since = None;
        }
    }
}

/// Split the blocks batch into 4 parts:
/// * All the blocks up to the last block according to superblock target
/// * The blocks needed to create the superblock with index target+1
/// * The remaining blocks that can be used to create superblock with index target+2
/// * Blocks that are above superblock target+2
///
/// Assumes that the blocks are sorted by checkpoint, and no two blocks have the
/// same checkpoint, and superblock_period is at least 1.
// TODO: return slices instead of vectors?
// TODO: refactor this, use a function that splits into 2 parts
#[allow(clippy::type_complexity)]
fn split_blocks_batch_at_target(
    blocks: Vec<Block>,
    epoch: u32,
    sync_target: &SyncTarget,
    superblock_period: u32,
) -> (
    Vec<Block>,
    Option<Vec<Block>>,
    Option<Vec<Block>>,
    Option<Vec<Block>>,
    Option<u32>,
) {
    let first_epoch_part_2 = sync_target.superblock.checkpoint * superblock_period;
    let first_epoch_part_3 = (sync_target.superblock.checkpoint + 1) * superblock_period;
    let first_epoch_part_4 = (sync_target.superblock.checkpoint + 2) * superblock_period;
    log::debug!(
        "Splitting blocks batch at epochs. part_2 >= {}, part_3 >= {}",
        first_epoch_part_2,
        first_epoch_part_3
    );
    //let first_epoch_part_3 = first_epoch_part_2 + superblock_period;

    let mut part_1 = blocks;
    let mut part_2 = None;
    let mut part_3 = None;
    let mut part_4 = None;

    // TODO: search in reverse, should be faster
    let position_first_elem_part_2 = part_1
        .iter()
        .position(|block| block.block_header.beacon.checkpoint >= first_epoch_part_2);
    if let Some(i) = position_first_elem_part_2 {
        part_2 = Some(part_1.split_off(i));
        let position_first_elem_part_3 = part_2
            .as_ref()
            .unwrap()
            .iter()
            .position(|block| block.block_header.beacon.checkpoint >= first_epoch_part_3);
        if let Some(i) = position_first_elem_part_3 {
            part_3 = Some(part_2.as_mut().unwrap().split_off(i));
            let position_first_elem_part_4 = part_3
                .as_ref()
                .unwrap()
                .iter()
                .position(|block| block.block_header.beacon.checkpoint >= first_epoch_part_4);
            if let Some(i) = position_first_elem_part_4 {
                part_4 = Some(part_3.as_mut().unwrap().split_off(i));
            }
        }
    }

    // Edge case: if the last block of part_1 has epoch first_epoch_part_2 - 1,
    // part_2 should be Some(vec![]), not None
    if part_2.is_none()
        && part_1.last().is_some()
        && part_1
            .last()
            .as_ref()
            .unwrap()
            .block_header
            .beacon
            .checkpoint
            == first_epoch_part_2 - 1
    {
        part_2 = Some(vec![]);
    }

    // Edge case: if the last block of part_2 has epoch first_epoch_part_3 - 1,
    // part_3 should be Some(vec![]), not None
    if part_3.is_none()
        && part_2.is_some()
        && part_2.as_ref().unwrap().last().is_some()
        && part_2
            .as_ref()
            .unwrap()
            .last()
            .as_ref()
            .unwrap()
            .block_header
            .beacon
            .checkpoint
            == first_epoch_part_3 - 1
    {
        part_3 = Some(vec![]);
    }

    // Edge case: if the last block of part_3 has epoch first_epoch_part_4 - 1,
    // part_4 should be Some(vec![]), not None
    if part_4.is_none()
        && part_3.is_some()
        && part_3.as_ref().unwrap().last().is_some()
        && part_3
            .as_ref()
            .unwrap()
            .last()
            .as_ref()
            .unwrap()
            .block_header
            .beacon
            .checkpoint
            == first_epoch_part_4 - 1
    {
        part_4 = Some(vec![]);
    }

    let next_index = if epoch / superblock_period == sync_target.superblock.checkpoint {
        None
    } else {
        Some(epoch / superblock_period)
    };

    (part_1, part_2, part_3, part_4, next_index)
}

/// Handler for AddCandidates message
impl Handler<AddCandidates> for ChainManager {
    type Result = SessionUnitResult;

    fn handle(&mut self, msg: AddCandidates, _ctx: &mut Context<Self>) {
        // AddCandidates is needed in all states
        for block in msg.blocks {
            self.process_candidate(block);
        }
    }
}

/// Handler for AddSuperBlockVote message
impl Handler<AddSuperBlockVote> for ChainManager {
    type Result = Result<(), failure::Error>;

    fn handle(
        &mut self,
        AddSuperBlockVote { superblock_vote }: AddSuperBlockVote,
        ctx: &mut Context<Self>,
    ) -> Self::Result {
        self.add_superblock_vote(superblock_vote, ctx)
    }
}

/// Handler for AddTransaction message
impl Handler<AddTransaction> for ChainManager {
    type Result = ResponseActFuture<Self, (), failure::Error>;

    fn handle(&mut self, msg: AddTransaction, _ctx: &mut Context<Self>) -> Self::Result {
        let timestamp_now = get_timestamp();
        self.add_transaction(msg, timestamp_now)
    }
}

/// Handler for GetBlocksEpochRange
impl Handler<GetBlocksEpochRange> for ChainManager {
    type Result = Result<Vec<(Epoch, Hash)>, ChainManagerError>;

    fn handle(
        &mut self,
        GetBlocksEpochRange {
            range,
            limit,
            limit_from_end,
        }: GetBlocksEpochRange,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        log::debug!("GetBlocksEpochRange received {:?}", range);

        // Accept this message in any state
        // TODO: we should only accept this message in Synced state, but that breaks the
        // JSON-RPC getBlockChain method

        // Iterator over all the blocks in the given range
        let block_chain_range = self
            .chain_state
            .block_chain
            .range(range)
            .map(|(k, v)| (*k, *v));

        if limit == 0 {
            // Return all the blocks from this epoch range
            let hashes: Vec<(Epoch, Hash)> = block_chain_range.collect();

            Ok(hashes)
        } else if limit_from_end {
            let mut hashes: Vec<(Epoch, Hash)> = block_chain_range
                // Take the last "limit" blocks
                .rev()
                .take(limit)
                .collect();

            // Reverse again to return them in non-reversed order
            hashes.reverse();

            Ok(hashes)
        } else {
            let hashes: Vec<(Epoch, Hash)> = block_chain_range
                // Take the first "limit" blocks
                .take(limit)
                .collect();

            Ok(hashes)
        }
    }
}

impl PeersBeacons {
    /// Pretty-print a map {beacon: [peers]}
    pub fn pretty_format(&self) -> String {
        let mut beacon_peers_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in self.pb.iter() {
            let v = v
                .as_ref()
                // TODO: this method ignores the superblock beacon
                .map(|x| x.highest_block_checkpoint)
                .map(|x| format!("#{} {}", x.checkpoint, x.hash_prev_block))
                .unwrap_or_else(|| "NO BEACON".to_string());
            beacon_peers_map.entry(v).or_default().push(k.to_string());
        }

        format!("{:?}", beacon_peers_map)
    }

    /// Run the consensus on the beacons, will return the most common beacon.
    /// We do not take into account our beacon to calculate the consensus.
    /// The beacons are `Option<CheckpointBeacon>`, so peers that have not
    /// sent us a beacon are counted as `None`. Keeping that in mind, we
    /// reach consensus as long as consensus_threshold % of peers agree.
    pub fn block_consensus(&self, consensus_threshold: usize) -> Option<CheckpointBeacon> {
        // We need to add `num_missing_peers` times NO BEACON, to take into account
        // missing outbound peers.
        let num_missing_peers = self.outbound_limit
            .map(|outbound_limit| {
                // TODO: is it possible to receive more than outbound_limit beacons?
                // (it shouldn't be possible)
                assert!(self.pb.len() <= outbound_limit as usize, "Received more beacons than the outbound_limit. Check the code for race conditions.");
                usize::try_from(outbound_limit).unwrap() - self.pb.len()
            })
            // The outbound limit is set when the SessionsManager actor is initialized, so here it
            // cannot be None. But if it is None, set num_missing_peers to 0 in order to calculate
            // consensus with the existing beacons.
            .unwrap_or(0);

        mode_consensus(
            self.pb
                .iter()
                .map(|(_p, b)| {
                    b.as_ref()
                        .map(|last_beacon| last_beacon.highest_block_checkpoint)
                })
                .chain(std::iter::repeat(None).take(num_missing_peers)),
            consensus_threshold,
        )
        // Flatten result:
        // None (consensus % below threshold) should be treated the same way as
        // Some(None) (most of the peers did not send a beacon)
        .and_then(|x| x)
    }

    // TODO: this is a copy-paste of block_consensus, refactor?
    /// Run the consensus on the beacons, will return the most common beacon.
    /// We do not take into account our beacon to calculate the consensus.
    /// The beacons are `Option<CheckpointBeacon>`, so peers that have not
    /// sent us a beacon are counted as `None`. Keeping that in mind, we
    /// reach consensus as long as consensus_threshold % of peers agree.
    pub fn superblock_consensus(&self, consensus_threshold: usize) -> Option<(LastBeacon, bool)> {
        // We need to add `num_missing_peers` times NO BEACON, to take into account
        // missing outbound peers.
        let num_missing_peers = self.outbound_limit
            .map(|outbound_limit| {
                // TODO: is it possible to receive more than outbound_limit beacons?
                // (it shouldn't be possible)
                assert!(self.pb.len() <= outbound_limit as usize, "Received more beacons than the outbound_limit. Check the code for race conditions.");
                usize::try_from(outbound_limit).unwrap() - self.pb.len()
            })
            // The outbound limit is set when the SessionsManager actor is initialized, so here it
            // cannot be None. But if it is None, set num_missing_peers to 0 in order to calculate
            // consensus with the existing beacons.
            .unwrap_or(0);

        mode_consensus(
            self.pb
                .iter()
                .map(|(_p, b)| {
                    b.as_ref()
                        .map(|last_beacon| last_beacon.highest_superblock_checkpoint)
                })
                .chain(std::iter::repeat(None).take(num_missing_peers)),
            consensus_threshold,
        )
        // Flatten result:
        // None (consensus % below threshold) should be treated the same way as
        // Some(None) (most of the peers did not send a beacon)
        .and_then(|x| x)
        .map(|superblock_consensus| {
            // If there is superblock consensus, we also need to set the block consensus.
            // We will use all the beacons that set superblock_beacon to superblock_consensus
            // There are 3 cases:
            // * A majority of beacons agree on a block. We will use this block as the
            // block_consensus and unregister all the peers that voted a different block
            // * There is a majority of beacons but it is below consensus_threshold. We will use
            // this majority as the block_consensus, and we will unregister the peers that voted on
            // a different superblock
            // * There is a tie. Will use any block as the consensus. So if there are 4 votes for
            // A, 4 votes for B, and 1 vote for C, the consensus can be A, B, or C.
            let block_beacons: Vec<_> = self
                .pb
                .iter()
                .filter_map(|(_p, b)| {
                    b.as_ref().and_then(|last_beacon| {
                        if last_beacon.highest_superblock_checkpoint == superblock_consensus {
                            Some(last_beacon.highest_block_checkpoint)
                        } else {
                            None
                        }
                    })
                })
                .collect();
            // Case 1
            let block_consensus_mode = mode_consensus(block_beacons.iter(), consensus_threshold);

            let (block_consensus, is_there_block_consensus) = if let Some(x) = block_consensus_mode
            {
                (*x, true)
            } else {
                (
                    // Case 2
                    mode_consensus(block_beacons.iter(), 0)
                        .copied()
                        .unwrap_or_else(|| {
                            // Case 3
                            // TODO: choose one at random?
                            block_beacons[0]
                        }),
                    false,
                )
            };

            (
                LastBeacon {
                    highest_superblock_checkpoint: superblock_consensus,
                    highest_block_checkpoint: block_consensus,
                },
                is_there_block_consensus,
            )
        })
    }

    /// Collects the peers to unregister based on the beacon they reported and the beacon to be compared it with
    pub fn decide_peers_to_unregister(&self, beacon: CheckpointBeacon) -> Vec<SocketAddr> {
        // Unregister peers which have a different beacon
        (&self.pb)
            .iter()
            .filter_map(|(p, b)| {
                if b.as_ref()
                    .map(|last_beacon| last_beacon.highest_block_checkpoint != beacon)
                    .unwrap_or(true)
                {
                    Some(*p)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Collects the peers to unregister based on the beacon they reported and the beacon to be compared it with
    pub fn decide_peers_to_unregister_s(&self, superbeacon: CheckpointBeacon) -> Vec<SocketAddr> {
        // Unregister peers which have a different beacon
        (&self.pb)
            .iter()
            .filter_map(|(p, b)| {
                if b.as_ref()
                    .map(|last_beacon| last_beacon.highest_superblock_checkpoint != superbeacon)
                    .unwrap_or(true)
                {
                    Some(*p)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Collects the peers that have not sent us a beacon
    pub fn peers_with_no_beacon(&self) -> Vec<SocketAddr> {
        // Unregister peers which have not sent us a beacon
        (&self.pb)
            .iter()
            .filter_map(|(p, b)| if b.is_none() { Some(*p) } else { None })
            .collect()
    }
}

impl Handler<PeersBeacons> for ChainManager {
    type Result = <PeersBeacons as Message>::Result;

    // FIXME(#676): Remove clippy skip error
    #[allow(clippy::cognitive_complexity)]
    fn handle(&mut self, peers_beacons: PeersBeacons, ctx: &mut Context<Self>) -> Self::Result {
        log::debug!(
            "PeersBeacons received while StateMachine is in state {:?}",
            self.sm_state
        );

        log::debug!("Received beacons: {}", peers_beacons.pretty_format());

        // Activate peers beacons index to continue synced
        if !peers_beacons.pb.is_empty() {
            self.peers_beacons_received = true;
        }

        // Calculate the consensus, or None if there is no consensus
        let consensus_threshold = self.consensus_c as usize;
        let beacon_consensus = peers_beacons.superblock_consensus(consensus_threshold);
        let outbound_limit = peers_beacons.outbound_limit;
        let pb_len = peers_beacons.pb.len();
        let peers_needed_for_consensus = outbound_limit
            .map(|x| {
                // ceil(x * consensus_threshold / 100)
                (usize::from(x) * consensus_threshold + 99) / 100
            })
            .unwrap_or(1);
        let peers_with_no_beacon = peers_beacons.peers_with_no_beacon();
        let peers_to_unregister = if let Some((consensus, is_there_block_consensus)) =
            beacon_consensus.as_ref()
        {
            if *is_there_block_consensus {
                peers_beacons.decide_peers_to_unregister(consensus.highest_block_checkpoint)
            } else {
                peers_beacons.decide_peers_to_unregister_s(consensus.highest_superblock_checkpoint)
            }
        } else if pb_len < peers_needed_for_consensus {
            // Not enough outbound peers, do not unregister any peers
            log::debug!(
                "Got {} peers but need at least {} to calculate the consensus",
                pb_len,
                peers_needed_for_consensus
            );
            vec![]
        } else {
            // No consensus: if state is AlmostSynced unregister those that are not coincident with ours.
            // Else, unregister all peers
            if self.sm_state == StateMachine::AlmostSynced || self.sm_state == StateMachine::Synced
            {
                log::warn!("Lack of peer consensus while state is `AlmostSynced`: peers that do not coincide with our last beacon will be unregistered");
                peers_beacons.decide_peers_to_unregister(self.get_chain_beacon())
            } else {
                log::warn!("Lack of peer consensus: all peers will be unregistered");
                peers_beacons.pb.into_iter().map(|(p, _b)| p).collect()
            }
        };

        let beacon_consensus = beacon_consensus.map(|(beacon, _)| beacon);

        let peers_to_unregister = match self.sm_state {
            StateMachine::WaitingConsensus => {
                // As soon as there is consensus, we set the target beacon to the consensus
                // and set the state to Synchronizing
                match beacon_consensus {
                    Some(LastBeacon {
                        highest_superblock_checkpoint: superblock_consensus,
                        highest_block_checkpoint: consensus_beacon,
                    }) => {
                        self.sync_target = Some(SyncTarget {
                            block: consensus_beacon,
                            superblock: superblock_consensus,
                        });
                        log::debug!("Sync target {:?}", self.sync_target);

                        let our_beacon = self.get_chain_beacon();
                        log::debug!(
                            "Consensus beacon: {:?} Our beacon {:?}",
                            (consensus_beacon, superblock_consensus),
                            our_beacon
                        );

                        let consensus_constants = self.consensus_constants();

                        // Check if we are already synchronized
                        // TODO: use superblock beacon
                        self.sm_state = if consensus_beacon.hash_prev_block
                            == consensus_constants.bootstrap_hash
                        {
                            log::debug!("The consensus is that there is no genesis block yet");

                            StateMachine::WaitingConsensus
                        } else if our_beacon == consensus_beacon {
                            StateMachine::AlmostSynced
                        } else if our_beacon.checkpoint == consensus_beacon.checkpoint
                            && our_beacon.hash_prev_block != consensus_beacon.hash_prev_block
                        {
                            // Fork case
                            log::warn!(
                                "[CONSENSUS]: We are on {:?} but the network is on {:?}",
                                our_beacon,
                                consensus_beacon
                            );

                            self.initialize_from_storage(ctx);
                            log::info!("Restored chain state from storage");

                            StateMachine::WaitingConsensus
                        } else {
                            // Review candidates
                            let consensus_block_hash = consensus_beacon.hash_prev_block;
                            let candidate = self.candidates.remove(&consensus_block_hash);
                            // Clear candidates, as they are only valid for one epoch
                            self.candidates.clear();
                            self.seen_candidates.clear();
                            // TODO: Be functional my friend
                            if let Some(consensus_block) = candidate {
                                match self.process_requested_block(ctx, consensus_block) {
                                    Ok(()) => {
                                        log::info!(
                                            "Consolidate consensus candidate. AlmostSynced state"
                                        );
                                        StateMachine::AlmostSynced
                                    }
                                    Err(e) => {
                                        log::debug!(
                                            "Failed to consolidate consensus candidate: {}",
                                            e
                                        );

                                        self.request_blocks_batch(ctx);

                                        StateMachine::Synchronizing
                                    }
                                }
                            } else {
                                self.request_blocks_batch(ctx);

                                StateMachine::Synchronizing
                            }
                        };

                        Ok(peers_to_unregister)
                    }
                    // No consensus: unregister all peers
                    None => Ok(peers_to_unregister),
                }
            }
            StateMachine::Synchronizing => {
                match beacon_consensus {
                    Some(LastBeacon {
                        highest_superblock_checkpoint: superblock_consensus,
                        highest_block_checkpoint: consensus_beacon,
                    }) => {
                        self.sync_target = Some(SyncTarget {
                            block: consensus_beacon,
                            superblock: superblock_consensus,
                        });

                        let our_beacon = self.get_chain_beacon();

                        // Check if we are already synchronized
                        self.sm_state = if our_beacon == consensus_beacon {
                            StateMachine::AlmostSynced
                        } else if our_beacon.checkpoint == consensus_beacon.checkpoint
                            && our_beacon.hash_prev_block != consensus_beacon.hash_prev_block
                        {
                            // Fork case
                            log::warn!(
                                "[CONSENSUS]: We are on {:?} but the network is on {:?}",
                                our_beacon,
                                consensus_beacon
                            );

                            self.initialize_from_storage(ctx);
                            log::info!("Restored chain state from storage");

                            StateMachine::WaitingConsensus
                        } else {
                            StateMachine::Synchronizing
                        };

                        Ok(peers_to_unregister)
                    }
                    // No consensus: unregister all peers
                    None => {
                        self.sm_state = StateMachine::WaitingConsensus;

                        Ok(peers_to_unregister)
                    }
                }
            }
            StateMachine::AlmostSynced | StateMachine::Synced => {
                let our_beacon = self.get_chain_beacon();
                match beacon_consensus {
                    Some(LastBeacon {
                        highest_block_checkpoint: consensus_beacon,
                        ..
                    }) if consensus_beacon == our_beacon => {
                        if self.sm_state == StateMachine::AlmostSynced {
                            // This is the only point in the whole base code for the state
                            // machine to move into `Synced` state.
                            log::debug!("Moving from AlmostSynced to Synced state");
                            log::info!("{}", SYNCED_BANNER);
                            self.sm_state = StateMachine::Synced;
                            self.add_temp_superblock_votes(ctx).unwrap();
                        }
                        Ok(peers_to_unregister)
                    }
                    Some(LastBeacon {
                        highest_block_checkpoint: consensus_beacon,
                        ..
                    }) => {
                        // We are out of consensus!
                        log::warn!(
                            "[CONSENSUS]: We are on {:?} but the network is on {:?}",
                            our_beacon,
                            consensus_beacon,
                        );

                        // If we are synced, it does not matter what blocks have been consolidated
                        // by our outbound peers, we will stay synced until the next superblock
                        // vote

                        Ok(peers_with_no_beacon)
                    }
                    None => {
                        // If we are synced and the consensus beacon is not the same as our beacon, then
                        // we need to rewind one epoch
                        if pb_len == 0 {
                            log::warn!(
                                "[CONSENSUS]: We have not received any beacons for this epoch"
                            );
                        } else {
                            // There is no consensus because of a tie, do not rewind?
                            // For example this could happen when each peer reports a different beacon...
                            log::warn!(
                                "[CONSENSUS]: We are on {:?} but the network has no consensus",
                                our_beacon
                            );
                        }

                        // If we are synced, it does not matter what blocks have been consolidated
                        // by our outbound peers, we will stay synced until the next superblock
                        // vote

                        Ok(peers_with_no_beacon)
                    }
                }
            }
        };

        if self.sm_state == StateMachine::Synchronizing {
            if let Some(sync_start_epoch) = self.sync_waiting_for_add_blocks_since {
                let current_epoch = self.current_epoch.unwrap();
                let how_many_epochs_are_we_willing_to_wait_for_one_block_batch = 10;
                if current_epoch - sync_start_epoch
                    >= how_many_epochs_are_we_willing_to_wait_for_one_block_batch
                {
                    log::warn!("Timeout for waiting for blocks achieved. Requesting blocks again.");
                    self.request_blocks_batch(ctx);
                }
            }
        }

        peers_to_unregister
    }
}

impl Handler<BuildVtt> for ChainManager {
    type Result = ResponseActFuture<Self, Hash, failure::Error>;

    fn handle(&mut self, msg: BuildVtt, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Box::new(actix::fut::err(
                ChainManagerError::NotSynced {
                    current_state: self.sm_state,
                }
                .into(),
            ));
        }
        let timestamp = u64::try_from(get_timestamp()).unwrap();
        match transaction_factory::build_vtt(
            msg.vto,
            msg.fee,
            &mut self.chain_state.own_utxos,
            self.own_pkh.unwrap(),
            &self.chain_state.unspent_outputs_pool,
            timestamp,
            self.tx_pending_timeout,
            msg.utxo_strategy,
        ) {
            Err(e) => {
                log::error!("Error when building value transfer transaction: {}", e);
                Box::new(actix::fut::err(e.into()))
            }
            Ok(vtt) => {
                let fut = transaction_factory::sign_transaction(&vtt, vtt.inputs.len())
                    .into_actor(self)
                    .then(|s, act, ctx| match s {
                        Ok(signatures) => {
                            let transaction =
                                Transaction::ValueTransfer(VTTransaction::new(vtt, signatures));
                            let tx_hash = transaction.hash();
                            Box::new(
                                act.handle(AddTransaction { transaction }, ctx)
                                    .map(move |_, _, _| tx_hash),
                            )
                        }
                        Err(e) => {
                            log::error!("Failed to sign value transfer transaction: {}", e);

                            let res: Box<
                                dyn ActorFuture<
                                    Item = Hash,
                                    Error = failure::Error,
                                    Actor = ChainManager,
                                >,
                            > = Box::new(actix::fut::err(e));
                            res
                        }
                    });

                Box::new(fut)
            }
        }
    }
}

impl Handler<BuildDrt> for ChainManager {
    type Result = ResponseActFuture<Self, Hash, failure::Error>;

    fn handle(&mut self, msg: BuildDrt, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Box::new(actix::fut::err(
                ChainManagerError::NotSynced {
                    current_state: self.sm_state,
                }
                .into(),
            ));
        }
        if let Err(e) = validate_rad_request(&msg.dro.data_request) {
            return Box::new(actix::fut::err(e));
        }
        let timestamp = u64::try_from(get_timestamp()).unwrap();
        match transaction_factory::build_drt(
            msg.dro,
            msg.fee,
            &mut self.chain_state.own_utxos,
            self.own_pkh.unwrap(),
            &self.chain_state.unspent_outputs_pool,
            timestamp,
            self.tx_pending_timeout,
        ) {
            Err(e) => {
                log::error!("Error when building data request transaction: {}", e);
                Box::new(actix::fut::err(e.into()))
            }
            Ok(drt) => {
                log::debug!("Created drt:\n{:?}", drt);
                let fut = transaction_factory::sign_transaction(&drt, drt.inputs.len())
                    .into_actor(self)
                    .then(|s, act, ctx| match s {
                        Ok(signatures) => {
                            let transaction =
                                Transaction::DataRequest(DRTransaction::new(drt, signatures));
                            let tx_hash = transaction.hash();
                            Box::new(
                                act.handle(AddTransaction { transaction }, ctx)
                                    .map(move |_, _, _| tx_hash),
                            )
                        }
                        Err(e) => {
                            log::error!("Failed to sign data request transaction: {}", e);

                            let res: Box<
                                dyn ActorFuture<
                                    Item = Hash,
                                    Error = failure::Error,
                                    Actor = ChainManager,
                                >,
                            > = Box::new(actix::fut::err(e));
                            res
                        }
                    });

                Box::new(fut)
            }
        }
    }
}

impl Handler<GetState> for ChainManager {
    type Result = <GetState as Message>::Result;

    fn handle(&mut self, _msg: GetState, _ctx: &mut Self::Context) -> Self::Result {
        Ok(self.sm_state)
    }
}

impl Handler<GetDataRequestReport> for ChainManager {
    type Result = ResponseFuture<DataRequestInfo, failure::Error>;

    fn handle(&mut self, msg: GetDataRequestReport, _ctx: &mut Self::Context) -> Self::Result {
        let dr_pointer = msg.dr_pointer;

        // First, try to get it from memory
        if let Some(dr_info) = self
            .chain_state
            .data_request_pool
            .data_request_pool
            .get(&dr_pointer)
            .map(|dr_state| dr_state.info.clone())
        {
            Box::new(futures::finished(dr_info))
        } else {
            let dr_pointer_string = format!("DR-REPORT-{}", dr_pointer);
            // Otherwise, try to get it from storage
            let fut = storage_mngr::get::<_, DataRequestReport>(&dr_pointer_string).and_then(
                move |dr_report| match dr_report {
                    Some(x) => futures::finished(DataRequestInfo::from(x)),
                    None => futures::failed(DataRequestNotFound { hash: dr_pointer }.into()),
                },
            );

            Box::new(fut)
        }
    }
}

impl Handler<GetBalance> for ChainManager {
    type Result = Result<u64, failure::Error>;

    fn handle(&mut self, GetBalance { pkh }: GetBalance, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        Ok(transaction_factory::get_total_balance(
            &self.chain_state.unspent_outputs_pool,
            pkh,
        ))
    }
}

impl Handler<GetUtxoInfo> for ChainManager {
    type Result = Result<UtxoInfo, failure::Error>;

    fn handle(
        &mut self,
        GetUtxoInfo { pkh }: GetUtxoInfo,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let chain_info = self.chain_state.chain_info.as_ref().unwrap();
        let block_number_limit = self
            .chain_state
            .block_number()
            .saturating_sub(chain_info.consensus_constants.collateral_age);

        let pkh = if self.own_pkh == Some(pkh) {
            None
        } else {
            Some(pkh)
        };

        Ok(get_utxo_info(
            pkh,
            &self.chain_state.own_utxos,
            &self.chain_state.unspent_outputs_pool,
            chain_info.consensus_constants.collateral_minimum,
            block_number_limit,
        ))
    }
}

impl Handler<GetReputation> for ChainManager {
    type Result = Result<(Reputation, bool), failure::Error>;

    fn handle(
        &mut self,
        GetReputation { pkh }: GetReputation,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let rep_eng = match self.chain_state.reputation_engine.as_ref() {
            Some(x) => x,
            None => return Err(ChainManagerError::ChainNotReady.into()),
        };

        Ok((rep_eng.trs().get(&pkh), rep_eng.ars().contains(&pkh)))
    }
}

impl Handler<GetReputationAll> for ChainManager {
    type Result = Result<HashMap<PublicKeyHash, (Reputation, bool)>, failure::Error>;

    fn handle(&mut self, _msg: GetReputationAll, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let rep_eng = match self.chain_state.reputation_engine.as_ref() {
            Some(x) => x,
            None => return Err(ChainManagerError::ChainNotReady.into()),
        };

        Ok(rep_eng
            .trs()
            .identities()
            .map(|(k, v)| (*k, (*v, rep_eng.ars().contains(k))))
            .collect())
    }
}

impl Handler<GetReputationStatus> for ChainManager {
    type Result = Result<GetReputationStatusResult, failure::Error>;

    fn handle(&mut self, _msg: GetReputationStatus, _ctx: &mut Self::Context) -> Self::Result {
        if self.sm_state != StateMachine::Synced {
            return Err(ChainManagerError::NotSynced {
                current_state: self.sm_state,
            }
            .into());
        }

        let rep_eng = match self.chain_state.reputation_engine.as_ref() {
            Some(x) => x,
            None => return Err(ChainManagerError::ChainNotReady.into()),
        };

        let num_active_identities = u32::try_from(rep_eng.ars().active_identities_number())?;
        let total_active_reputation = rep_eng.total_active_reputation();

        Ok(GetReputationStatusResult {
            num_active_identities,
            total_active_reputation,
        })
    }
}

impl Handler<TryMineBlock> for ChainManager {
    type Result = ();

    fn handle(&mut self, _msg: TryMineBlock, ctx: &mut Self::Context) -> Self::Result {
        self.try_mine_block(ctx);
    }
}

impl Handler<AddCommitReveal> for ChainManager {
    type Result = ResponseActFuture<Self, (), failure::Error>;

    fn handle(
        &mut self,
        AddCommitReveal {
            commit_transaction,
            reveal_transaction,
        }: AddCommitReveal,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        let dr_pointer = commit_transaction.body.dr_pointer;
        // Hold reveal transaction under "waiting_for_reveal" field of data requests pool
        self.chain_state
            .data_request_pool
            .insert_reveal(dr_pointer, reveal_transaction);

        // Send AddTransaction message to self
        // And broadcast it to all of peers
        Box::new(
            self.handle(
                AddTransaction {
                    transaction: Transaction::Commit(commit_transaction),
                },
                ctx,
            )
            .map_err(|e, _, _| {
                log::warn!("Failed to add commit transaction: {}", e);
                e
            }),
        )
    }
}

impl Handler<GetMemoryTransaction> for ChainManager {
    type Result = Result<Transaction, ()>;

    fn handle(&mut self, msg: GetMemoryTransaction, _ctx: &mut Self::Context) -> Self::Result {
        self.transactions_pool.get(&msg.hash).ok_or(())
    }
}

impl Handler<GetMempool> for ChainManager {
    type Result = Result<GetMempoolResult, failure::Error>;

    fn handle(&mut self, _msg: GetMempool, _ctx: &mut Self::Context) -> Self::Result {
        let res = GetMempoolResult {
            value_transfer: self.transactions_pool.vt_iter().map(|t| t.hash()).collect(),
            data_request: self.transactions_pool.dr_iter().map(|t| t.hash()).collect(),
        };

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_beacons_consensus_less_peers_than_outbound() {
        let beacon1 = LastBeacon {
            highest_block_checkpoint: CheckpointBeacon {
                checkpoint: 1,
                hash_prev_block: "6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b"
                    .parse()
                    .unwrap(),
            },
            highest_superblock_checkpoint: CheckpointBeacon {
                checkpoint: 0,
                hash_prev_block: Hash::default(),
            },
        };
        let beacon2 = LastBeacon {
            highest_block_checkpoint: CheckpointBeacon {
                checkpoint: 1,
                hash_prev_block: "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35"
                    .parse()
                    .unwrap(),
            },
            highest_superblock_checkpoint: CheckpointBeacon {
                checkpoint: 0,
                hash_prev_block: Hash::default(),
            },
        };

        // 0 peers
        let peers_beacons = PeersBeacons {
            pb: vec![],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.block_consensus(60), None);

        // 1 peer
        let peers_beacons = PeersBeacons {
            pb: vec![("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone()))],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.block_consensus(60), None);

        // 2 peers
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.block_consensus(60), None);

        // 3 peers and 2 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon2.clone())),
            ],
            outbound_limit: Some(4),
        };
        // Note that the consensus % includes the missing peers,
        // so in this case it is 2/4 (50%), not 2/3 (66%), so there is no consensus for 60%
        assert_eq!(peers_beacons.block_consensus(60), None);

        // 3 peers and 3 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon1.clone())),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.block_consensus(60),
            Some(beacon1.highest_block_checkpoint)
        );

        // 4 peers and 2 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon2.clone())),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon2.clone())),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(peers_beacons.block_consensus(60), None);

        // 4 peers and 3 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon2)),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.block_consensus(60),
            Some(beacon1.highest_block_checkpoint)
        );

        // 4 peers and 4 agree
        let peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon1.clone())),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.block_consensus(60),
            Some(beacon1.highest_block_checkpoint)
        );
    }

    #[test]
    fn test_unregister_peers() {
        let beacon1 = LastBeacon {
            highest_block_checkpoint: CheckpointBeacon {
                checkpoint: 1,
                hash_prev_block: "6b86b273ff34fce19d6b804eff5a3f5747ada4eaa22f1d49c01e52ddb7875b4b"
                    .parse()
                    .unwrap(),
            },
            highest_superblock_checkpoint: CheckpointBeacon {
                checkpoint: 0,
                hash_prev_block: Hash::default(),
            },
        };
        let beacon2 = LastBeacon {
            highest_block_checkpoint: CheckpointBeacon {
                checkpoint: 1,
                hash_prev_block: "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab35"
                    .parse()
                    .unwrap(),
            },
            highest_superblock_checkpoint: CheckpointBeacon {
                checkpoint: 0,
                hash_prev_block: Hash::default(),
            },
        };

        // 0 peers
        let mut peers_beacons = PeersBeacons {
            pb: vec![],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon1.highest_block_checkpoint),
            []
        );

        // 1 peer in consensus
        peers_beacons = PeersBeacons {
            pb: vec![("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone()))],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon1.highest_block_checkpoint),
            []
        );

        // 1 peer out of consensus
        peers_beacons = PeersBeacons {
            pb: vec![("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone()))],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon2.highest_block_checkpoint),
            ["127.0.0.1:10001".parse().unwrap()]
        );

        peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10003".parse().unwrap(), Some(beacon2.clone())),
                ("127.0.0.1:10004".parse().unwrap(), Some(beacon2.clone())),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon2.highest_block_checkpoint),
            [
                "127.0.0.1:10001".parse().unwrap(),
                "127.0.0.1:10002".parse().unwrap()
            ]
        );

        peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10002".parse().unwrap(), Some(beacon1.clone())),
                ("127.0.0.1:10003".parse().unwrap(), None),
                ("127.0.0.1:10004".parse().unwrap(), None),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon1.highest_block_checkpoint),
            [
                "127.0.0.1:10003".parse().unwrap(),
                "127.0.0.1:10004".parse().unwrap()
            ]
        );

        peers_beacons = PeersBeacons {
            pb: vec![
                ("127.0.0.1:10001".parse().unwrap(), None),
                ("127.0.0.1:10002".parse().unwrap(), None),
                ("127.0.0.1:10003".parse().unwrap(), None),
                ("127.0.0.1:10004".parse().unwrap(), None),
            ],
            outbound_limit: Some(4),
        };
        assert_eq!(
            peers_beacons.decide_peers_to_unregister(beacon1.highest_block_checkpoint),
            [
                "127.0.0.1:10001".parse().unwrap(),
                "127.0.0.1:10002".parse().unwrap(),
                "127.0.0.1:10003".parse().unwrap(),
                "127.0.0.1:10004".parse().unwrap()
            ]
        );
    }

    #[test]
    fn test_split_blocks_batch() {
        use witnet_data_structures::chain::block_example;
        let b = |checkpoint| {
            let mut block = block_example();
            block.block_header.beacon.checkpoint = checkpoint;

            block
        };

        let mut sync_target = SyncTarget {
            block: Default::default(),
            superblock: Default::default(),
        };
        let superblock_period = 10;

        assert_eq!(
            split_blocks_batch_at_target(vec![], &sync_target, superblock_period),
            (vec![], None, None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0)], &sync_target, superblock_period),
            (vec![], Some(vec![b(0)]), None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(8)], &sync_target, superblock_period),
            (vec![], Some(vec![b(0), b(8)]), None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(9)], &sync_target, superblock_period),
            (vec![], Some(vec![b(0), b(9)]), Some(vec![]), None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(10)], &sync_target, superblock_period),
            (vec![], Some(vec![b(0)]), Some(vec![b(10)]), None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(10), b(19)], &sync_target, superblock_period),
            (
                vec![],
                Some(vec![b(0)]),
                Some(vec![b(10), b(19)]),
                Some(vec![])
            )
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(10), b(20)], &sync_target, superblock_period),
            (
                vec![],
                Some(vec![b(0)]),
                Some(vec![b(10)]),
                Some(vec![b(20)])
            )
        );

        sync_target.superblock.checkpoint = 1;
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0)], &sync_target, superblock_period),
            (vec![b(0)], None, None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(8)], &sync_target, superblock_period),
            (vec![b(0), b(8)], None, None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(9)], &sync_target, superblock_period),
            (vec![b(0), b(9)], Some(vec![]), None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(10)], &sync_target, superblock_period),
            (vec![b(0)], Some(vec![b(10)]), None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(8), b(11)], &sync_target, superblock_period),
            (vec![b(0), b(8)], Some(vec![b(11)]), None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(
                vec![b(0), b(9), b(10), b(18)],
                &sync_target,
                superblock_period
            ),
            (vec![b(0), b(9)], Some(vec![b(10), b(18)]), None, None)
        );
        assert_eq!(
            split_blocks_batch_at_target(
                vec![b(0), b(9), b(10), b(19)],
                &sync_target,
                superblock_period
            ),
            (
                vec![b(0), b(9)],
                Some(vec![b(10), b(19)]),
                Some(vec![]),
                None
            ),
        );
        assert_eq!(
            split_blocks_batch_at_target(vec![b(0), b(10), b(20)], &sync_target, superblock_period),
            (vec![b(0)], Some(vec![b(10)]), Some(vec![b(20)]), None),
        );
        assert_eq!(
            split_blocks_batch_at_target(
                vec![b(0), b(9), b(10), b(19), b(20), b(21)],
                &sync_target,
                superblock_period
            ),
            (
                vec![b(0), b(9)],
                Some(vec![b(10), b(19)]),
                Some(vec![b(20), b(21)]),
                None
            ),
        );

        sync_target.superblock.checkpoint = 2;
        assert_eq!(
            split_blocks_batch_at_target(vec![b(100)], &sync_target, superblock_period),
            (vec![], Some(vec![]), Some(vec![]), Some(vec![b(100)]))
        );
    }
}
