// Copyright 2019-2022 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

mod deal;
mod policy;
mod state;
mod types;

pub use self::deal::*;
use self::policy::*;
pub use self::state::*;
pub use self::types::*;
use crate::{
    power, request_miner_control_addrs, reward,
    verifreg::{Method as VerifregMethod, RestoreBytesParams, UseBytesParams},
    ActorDowncast, DealID, BURNT_FUNDS_ACTOR_ADDR, CALLER_TYPES_SIGNABLE, CRON_ACTOR_ADDR,
    MINER_ACTOR_CODE_ID, REWARD_ACTOR_ADDR, STORAGE_POWER_ACTOR_ADDR, SYSTEM_ACTOR_ADDR,
    VERIFIED_REGISTRY_ACTOR_ADDR,
};
use address::Address;
use ahash::AHashMap;
use bitfield::BitField;
use cid::Prefix;
use clock::{ChainEpoch, EPOCH_UNDEFINED};
use encoding::{to_vec, Cbor};
use fil_types::deadlines::QuantSpec;
use fil_types::{PieceInfo, StoragePower};
use ipld_blockstore::BlockStore;
use log::info;
use num_bigint::BigInt;
use num_derive::FromPrimitive;
use num_traits::{FromPrimitive, Signed, Zero};
use runtime::{ActorCode, Runtime};
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use vm::{
    actor_error, ActorError, ExitCode, MethodNum, Serialized, TokenAmount, METHOD_CONSTRUCTOR,
    METHOD_SEND,
};

// * Updated to specs-actors commit: e195950ba98adb8ce362030356bf4a3809b7ec77 (v2.3.2)

/// Market actor methods available
#[derive(FromPrimitive)]
#[repr(u64)]
pub enum Method {
    Constructor = METHOD_CONSTRUCTOR,
    AddBalance = 2,
    WithdrawBalance = 3,
    PublishStorageDeals = 4,
    VerifyDealsForActivation = 5,
    ActivateDeals = 6,
    OnMinerSectorsTerminate = 7,
    ComputeDataCommitment = 8,
    CronTick = 9,
}

/// Market Actor
pub struct Actor;
impl Actor {
    pub fn constructor<BS, RT>(rt: &mut RT) -> Result<(), ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_is(std::iter::once(&*SYSTEM_ACTOR_ADDR))?;

        let st = State::new(rt.store()).map_err(|e| {
            e.downcast_default(ExitCode::ErrIllegalState, "Failed to create market state")
        })?;
        rt.create(&st)?;
        Ok(())
    }

    /// Deposits the received value into the balance held in escrow.
    fn add_balance<BS, RT>(rt: &mut RT, provider_or_client: Address) -> Result<(), ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        let msg_value = rt.message().value_received().clone();

        if msg_value <= TokenAmount::from(0) {
            return Err(actor_error!(
                ErrIllegalArgument,
                "balance to add must be greater than zero was: {}",
                msg_value
            ));
        }

        // only signing parties can add balance for client AND provider.
        rt.validate_immediate_caller_type(CALLER_TYPES_SIGNABLE.iter())?;

        let (nominal, _, _) = escrow_address(rt, &provider_or_client)?;

        rt.transaction(|st: &mut State, rt| {
            let mut msm = st.mutator(rt.store());
            msm.with_escrow_table(Permission::Write)
                .with_locked_table(Permission::Write)
                .build()
                .map_err(|e| {
                    e.downcast_default(ExitCode::ErrIllegalState, "failed to load state")
                })?;

            msm.escrow_table
                .as_mut()
                .unwrap()
                .add(&nominal, &msg_value)
                .map_err(|e| {
                    e.downcast_default(
                        ExitCode::ErrIllegalState,
                        "failed to add balance to escrow table",
                    )
                })?;

            msm.commit_state().map_err(|e| {
                e.downcast_default(ExitCode::ErrIllegalState, "failed to flush state")
            })?;

            Ok(())
        })?;

        Ok(())
    }

    /// Attempt to withdraw the specified amount from the balance held in escrow.
    /// If less than the specified amount is available, yields the entire available balance.
    fn withdraw_balance<BS, RT>(
        rt: &mut RT,
        params: WithdrawBalanceParams,
    ) -> Result<WithdrawBalanceReturn, ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        if params.amount < TokenAmount::from(0) {
            return Err(actor_error!(
                ErrIllegalArgument,
                "negative amount: {}",
                params.amount
            ));
        }

        let (nominal, recipient, approved) = escrow_address(rt, &params.provider_or_client)?;
        // for providers -> only corresponding owner or worker can withdraw
        // for clients -> only the client i.e the recipient can withdraw
        rt.validate_immediate_caller_is(&approved)?;

        let amount_extracted = rt.transaction(|st: &mut State, rt| {
            let mut msm = st.mutator(rt.store());
            msm.with_escrow_table(Permission::Write)
                .with_locked_table(Permission::Write)
                .build()
                .map_err(|e| {
                    e.downcast_default(ExitCode::ErrIllegalState, "failed to load state")
                })?;

            // The withdrawable amount might be slightly less than nominal
            // depending on whether or not all relevant entries have been processed
            // by cron
            let min_balance = msm
                .locked_table
                .as_ref()
                .unwrap()
                .get(&nominal)
                .map_err(|e| {
                    e.downcast_default(ExitCode::ErrIllegalState, "failed to get locked balance")
                })?;

            let ex = msm
                .escrow_table
                .as_mut()
                .unwrap()
                .subtract_with_minimum(&nominal, &params.amount, &min_balance)
                .map_err(|e| {
                    e.downcast_default(
                        ExitCode::ErrIllegalState,
                        "failed to subtract from escrow table",
                    )
                })?;

            msm.commit_state().map_err(|e| {
                e.downcast_default(ExitCode::ErrIllegalState, "failed to flush state")
            })?;

            Ok(ex)
        })?;

        rt.send(
            recipient,
            METHOD_SEND,
            Serialized::default(),
            amount_extracted.clone(),
        )?;
        Ok(WithdrawBalanceReturn {
            amount_withdrawn: amount_extracted,
        })
    }

    /// Publish a new set of storage deals (not yet included in a sector).
    fn publish_storage_deals<BS, RT>(
        rt: &mut RT,
        mut params: PublishStorageDealsParams,
    ) -> Result<PublishStorageDealsReturn, ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        // Deal message must have a From field identical to the provider of all the deals.
        // This allows us to retain and verify only the client's signature in each deal proposal itself.
        rt.validate_immediate_caller_type(CALLER_TYPES_SIGNABLE.iter())?;
        if params.deals.is_empty() {
            return Err(actor_error!(ErrIllegalArgument, "Empty deals parameter"));
        }

        // All deals should have the same provider so get worker once
        let provider_raw = params.deals[0].proposal.provider;
        let provider = rt.resolve_address(&provider_raw)?.ok_or_else(|| {
            actor_error!(
                ErrNotFound,
                "failed to resolve provider address {}",
                provider_raw
            )
        })?;

        let code_id = rt.get_actor_code_cid(&provider)?.ok_or_else(|| {
            actor_error!(ErrIllegalArgument, "no code ID for address {}", provider)
        })?;
        if code_id != *MINER_ACTOR_CODE_ID {
            return Err(actor_error!(
                ErrIllegalArgument,
                "deal provider is not a storage miner actor"
            ));
        }

        let (_, worker, controllers) = request_miner_control_addrs(rt, provider)?;
        let caller = rt.message().caller();
        let mut caller_ok = caller == &worker;
        for controller in controllers.iter() {
            if caller_ok {
                break;
            }
            caller_ok = caller == controller;
        }
        if !caller_ok {
            return Err(actor_error!(
                ErrForbidden,
                "caller {} is not worker or control address of provider {}",
                caller,
                provider
            ));
        }

        let baseline_power = request_current_baseline_power(rt)?;
        let (network_raw_power, _) = request_current_network_power(rt)?;

        // Drop invalid deals
        let mut proposal_cid_lookup = HashSet::new();
        let mut valid_proposal_cids = Vec::new();
        let mut valid_deals = Vec::with_capacity(params.deals.len());
        let mut total_client_lockup: HashMap<Address, TokenAmount> = HashMap::new();
        let mut total_provider_lockup = TokenAmount::zero();

        let mut valid_input_bf = BitField::default();
        let mut state: State = rt.state::<State>()?;

        let store = rt.store();
        let mut msm = state.mutator(store);
        msm.with_pending_proposals(Permission::ReadOnly)
            .with_escrow_table(Permission::ReadOnly)
            .with_locked_table(Permission::ReadOnly)
            .build()
            .map_err(|e| e.downcast_default(ExitCode::ErrIllegalState, "failed to load msm"))?;

        // the for loop too uses rt immutably and also needs to mutably access it.
        // so we will need to isolate them.
        struct Deal<'a> {
            to: Address,
            method: u64,
            params: Serialized,
            value: TokenAmount,
            di: usize,
            deal: &'a mut deal::ClientDealProposal,
        }
        let mut method_queue: Vec<Deal> = vec![];

        for (di, deal) in params.deals.iter_mut().enumerate() {
            // drop malformed deals
            if let Err(e) = validate_deal(rt, deal, &network_raw_power, &baseline_power) {
                info!("invalid deal {}: {}", di, e);
                continue;
            }

            if deal.proposal.provider != provider && deal.proposal.provider != provider_raw {
                info!(
                    "invalid deal {}: cannot publish deals from multiple providers in one batch",
                    di
                );
                continue;
            }
            let client = match rt.resolve_address(&deal.proposal.client) {
                Ok(Some(client)) => client,
                _ => {
                    info!(
                        "invalid deal {}: failed to resolve proposal.client address {} for deal",
                        di, deal.proposal.client
                    );
                    continue;
                }
            };

            // drop deals with insufficient lock up to cover costs
            total_client_lockup
                .entry(client)
                .or_insert_with(TokenAmount::zero);

            if let Some(lockup) = total_client_lockup.get_mut(&client) {
                *lockup += deal.proposal.client_balance_requirement();
            }
            // safe to unwrap here as we just set it right before if it doesnt exist
            let client_balance_ok = msm
                .balance_covered(client, total_client_lockup.get(&client).unwrap())
                .map_err(|e| {
                    e.downcast_default(
                        ExitCode::ErrIllegalState,
                        "failed to check client balance coverage",
                    )
                })?;

            if !client_balance_ok {
                info!(
                    "invalid deal: {}: insufficient client funds to cover proposal cost",
                    di
                );
                continue;
            }
            total_provider_lockup += &deal.proposal.provider_collateral;
            let provider_balance_ok = msm
                .balance_covered(provider, &total_provider_lockup)
                .map_err(|e| {
                    e.downcast_default(
                        ExitCode::ErrIllegalState,
                        "failed to check provider balance coverage",
                    )
                })?;

            if !provider_balance_ok {
                info!(
                    "invalid deal: {}: insufficient provider funds to cover proposal cost",
                    di
                );
                continue;
            }

            // drop duplicate deals
            // Normalise provider and client addresses in the proposal stored on chain.
            // Must happen after signature verification and before taking cid.

            deal.proposal.provider = provider;
            deal.proposal.client = client;
            // TODO resolved_addr add
            let pcid = deal.proposal.cid().map_err(|e| {
                ActorError::from(e).wrap(format!("failed to take cid of proposal {}", di))
            })?;

            // check proposalCids for duplication within message batch
            // check state PendingProposals for duplication across messages
            let duplicate_in_state = msm
                .pending_deals
                .as_ref()
                .unwrap()
                .has(&pcid.to_bytes())
                .map_err(|e| {
                    e.downcast_default(
                        ExitCode::ErrIllegalState,
                        "failed to check for existence of deal proposal",
                    )
                })?;
            let duplicate_in_message = proposal_cid_lookup.contains(&pcid);
            if duplicate_in_state || duplicate_in_message {
                info!(
                    "invalid deal {}: cannot publish duplicate deal proposal",
                    di
                );
                continue;
            }

            // check VerifiedClient allowed cap and deduct PieceSize from cap
            // drop deals with a DealSize that cannot be fully covered by VerifiedClient's available DataCap
            if deal.proposal.verified_deal {
                // So instead of mutably calling, we queue the rt.send() params as a struct, see Deal struct
                method_queue.push(Deal {
                    to: *VERIFIED_REGISTRY_ACTOR_ADDR,
                    method: crate::verifreg::Method::UseBytes as u64,
                    params: Serialized::serialize(UseBytesParams {
                        address: client,
                        deal_size: BigInt::from(deal.proposal.piece_size.0),
                    })?,
                    value: TokenAmount::zero(),
                    di,
                    deal,
                });
            }

            proposal_cid_lookup.insert(pcid);
        }

        // we then loop over the method_queue and call rt.send()
        for i in method_queue {
            let Deal {
                to,
                method,
                params,
                value,
                di,
                deal,
            } = i;
            let ret = rt.send(to, method, params, value);

            if let Err(e) = ret {
                info!(
                    "invalid deal {}: failed to acquire datacap exitcode: {}",
                    di, e
                );
                continue;
            }

            let pcid = deal.proposal.cid().map_err(|e| {
                ActorError::from(e).wrap(format!("failed to take cid of proposal {}", di))
            })?;

            // update valid deal state
            valid_proposal_cids.push(pcid);
            valid_deals.push(deal.clone());
            valid_input_bf.set(di);
        }

        let valid_deal_count = valid_input_bf.len();
        if valid_deals.len() != valid_proposal_cids.len() {
            return Err(actor_error!(
                ErrIllegalState,
                "{} valid deals but {} valid proposal cids",
                valid_deals.len(),
                valid_proposal_cids.len()
            ));
        }
        if valid_deal_count != valid_deals.len() {
            return Err(actor_error!(
                ErrIllegalState,
                "{} valid deals but valid_deal_count {}",
                valid_deals.len(),
                valid_deal_count
            ));
        }
        if valid_deal_count == 0 {
            return Err(actor_error!(
                ErrIllegalArgument,
                "All deal proposals invalid"
            ));
        }

        let mut new_deal_ids = Vec::new();

        rt.transaction(|st: &mut State, rt| {
            let mut msm = st.mutator(rt.store());
            msm.with_pending_proposals(Permission::Write)
                .with_deal_proposals(Permission::Write)
                .with_deals_by_epoch(Permission::Write)
                .with_escrow_table(Permission::Write)
                .with_locked_table(Permission::Write)
                .build()
                .map_err(|e| {
                    e.downcast_default(ExitCode::ErrIllegalState, "failed to load state")
                })?;

            // All storage dealProposals will be added in an atomic transaction; this operation will be unrolled if any of them fails.
            // This should only fail on programmer error because all expected invalid conditions should be filtered in the first set of checks.

            // TODO: Check if we neeed to make the previous look non mut
            for (vid, valid_deal) in valid_deals.iter().enumerate() {
                msm.lock_client_and_provider_balances(&valid_deal.proposal)?;

                let id = msm.generate_storage_deal_id();

                let pcid = valid_proposal_cids[vid];

                msm.pending_deals
                    .as_mut()
                    .unwrap()
                    .put(pcid.to_bytes().into())
                    .map_err(|e| {
                        e.downcast_default(ExitCode::ErrIllegalState, "failed to set pending deal")
                    })?;
                msm.deal_proposals
                    .as_mut()
                    .unwrap()
                    .set(id as usize, valid_deal.proposal.clone())
                    .map_err(|e| {
                        e.downcast_default(ExitCode::ErrIllegalState, "failed to set deal")
                    })?;

                // We randomize the first epoch for when the deal will be processed so an attacker isn't able to
                // schedule too many deals for the same tick.
                let process_epoch = gen_rand_next_epoch(valid_deal.proposal.start_epoch, id);

                msm.deals_by_epoch
                    .as_mut()
                    .unwrap()
                    .put(process_epoch, id)
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            "failed to set deal ops by epoch",
                        )
                    })?;

                new_deal_ids.push(id);
            }

            msm.commit_state().map_err(|e| {
                e.downcast_default(ExitCode::ErrIllegalState, "failed to flush state")
            })?;
            Ok(())
        })?;

        Ok(PublishStorageDealsReturn {
            ids: new_deal_ids,
            valid_deals: valid_input_bf,
        })
    }

    /// Verify that a given set of storage deals is valid for a sector currently being PreCommitted
    /// and return DealWeight of the set of storage deals given.
    /// The weight is defined as the sum, over all deals in the set, of the product of deal size
    /// and duration.
    fn verify_deals_for_activation<BS, RT>(
        rt: &mut RT,
        params: VerifyDealsForActivationParams,
    ) -> Result<VerifyDealsForActivationReturn, ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_type(std::iter::once(&*MINER_ACTOR_CODE_ID))?;
        let miner_addr = *rt.message().caller();
        let curr_epoch = rt.curr_epoch();

        let st: State = rt.state()?;
        let proposals = DealArray::load(&st.proposals, rt.store()).map_err(|e| {
            e.downcast_default(ExitCode::ErrIllegalState, "failed to load deal proposals")
        })?;

        let mut weights = Vec::with_capacity(params.sectors.len());
        for sector in params.sectors.iter() {
            let (deal_weight, verified_deal_weight, deal_space) = validate_and_compute_deal_weight(
                &proposals,
                &sector.deal_ids,
                &miner_addr,
                sector.sector_expiry,
                curr_epoch,
            )
            .map_err(|e| {
                e.downcast_default(
                    ExitCode::ErrIllegalState,
                    "failed to validate deal proposals for activation",
                )
            })?;
            weights.push(SectorWeights {
                deal_space,
                deal_weight,
                verified_deal_weight,
            });
        }

        Ok(VerifyDealsForActivationReturn { sectors: weights })
    }

    /// Verify that a given set of storage deals is valid for a sector currently being ProveCommitted,
    /// update the market's internal state accordingly.
    fn activate_deals<BS, RT>(rt: &mut RT, params: ActivateDealsParams) -> Result<(), ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_type(std::iter::once(&*MINER_ACTOR_CODE_ID))?;
        let miner_addr = *rt.message().caller();
        let curr_epoch = rt.curr_epoch();

        // Update deal states
        rt.transaction(|st: &mut State, rt| {
            validate_deals_for_activation(
                st,
                rt.store(),
                &params.deal_ids,
                &miner_addr,
                params.sector_expiry,
                curr_epoch,
            )
            .map_err(|e| {
                e.downcast_default(
                    ExitCode::ErrIllegalState,
                    "failed to validate deal proposals for activation",
                )
            })?;

            let mut msm = st.mutator(rt.store());
            msm.with_deal_states(Permission::Write)
                .with_pending_proposals(Permission::ReadOnly)
                .with_deal_proposals(Permission::ReadOnly)
                .build()
                .map_err(|e| {
                    e.downcast_default(ExitCode::ErrIllegalState, "failed to load state")
                })?;

            for deal_id in params.deal_ids {
                // This construction could be replaced with a single "update deal state"
                // state method, possibly batched over all deal ids at once.
                let s = msm
                    .deal_states
                    .as_ref()
                    .unwrap()
                    .get(deal_id as usize)
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to get state for deal_id ({})", deal_id),
                        )
                    })?;
                if s.is_some() {
                    return Err(actor_error!(
                        ErrIllegalArgument,
                        "deal {} already included in another sector",
                        deal_id
                    ));
                }

                let proposal = msm
                    .deal_proposals
                    .as_ref()
                    .unwrap()
                    .get(deal_id as usize)
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to get deal_id ({})", deal_id),
                        )
                    })?
                    .ok_or_else(|| actor_error!(ErrNotFound, "no such deal_id: {}", deal_id))?;

                let propc = proposal
                    .cid()
                    .map_err(|e| ActorError::from(e).wrap("failed to calculate proposal Cid"))?;

                let has = msm
                    .pending_deals
                    .as_ref()
                    .unwrap()
                    .has(&propc.to_bytes())
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to get pending proposal ({})", propc),
                        )
                    })?;

                if !has {
                    return Err(actor_error!(
                        ErrIllegalState,
                        "tried to activate deal that was not in the pending set ({})",
                        propc
                    ));
                }

                msm.deal_states
                    .as_mut()
                    .unwrap()
                    .set(
                        deal_id as usize,
                        DealState {
                            sector_start_epoch: curr_epoch,
                            last_updated_epoch: EPOCH_UNDEFINED,
                            slash_epoch: EPOCH_UNDEFINED,
                        },
                    )
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to set deal state {}", deal_id),
                        )
                    })?;
            }

            msm.commit_state().map_err(|e| {
                e.downcast_default(ExitCode::ErrIllegalState, "failed to flush state")
            })?;
            Ok(())
        })?;

        Ok(())
    }

    /// Terminate a set of deals in response to their containing sector being terminated.
    /// Slash provider collateral, refund client collateral, and refund partial unpaid escrow
    /// amount to client.
    fn on_miner_sectors_terminate<BS, RT>(
        rt: &mut RT,
        params: OnMinerSectorsTerminateParams,
    ) -> Result<(), ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_type(std::iter::once(&*MINER_ACTOR_CODE_ID))?;
        let miner_addr = *rt.message().caller();

        rt.transaction(|st: &mut State, rt| {
            let mut msm = st.mutator(rt.store());
            msm.with_deal_states(Permission::Write)
                .with_deal_proposals(Permission::ReadOnly)
                .build()
                .map_err(|e| {
                    e.downcast_default(ExitCode::ErrIllegalState, "failed to load state")
                })?;

            for id in params.deal_ids {
                let deal = msm
                    .deal_proposals
                    .as_ref()
                    .unwrap()
                    .get(id as usize)
                    .map_err(|e| {
                        e.downcast_default(ExitCode::ErrIllegalState, "failed to get deal proposal")
                    })?;
                // The deal may have expired and been deleted before the sector is terminated.
                // Nothing to do, but continue execution for the other deals.
                if deal.is_none() {
                    info!("couldn't find deal {}", id);
                    continue;
                }
                let deal = deal.unwrap();

                if deal.provider != miner_addr {
                    return Err(actor_error!(
                        ErrIllegalState,
                        "caller {} is not the provider {} of deal {}",
                        miner_addr,
                        deal.provider,
                        id
                    ));
                }

                // do not slash expired deals
                if deal.end_epoch <= params.epoch {
                    info!("deal {} expired, not slashing", id);
                    continue;
                }

                let mut state: DealState = *msm
                    .deal_states
                    .as_ref()
                    .unwrap()
                    .get(id as usize)
                    .map_err(|e| {
                        e.downcast_default(ExitCode::ErrIllegalState, "failed to get deal state")
                    })?
                    // A deal with a proposal but no state is not activated, but then it should not be
                    // part of a sector that is terminating.
                    .ok_or_else(|| actor_error!(ErrIllegalArgument, "no state for deal {}", id))?;

                // If a deal is already slashed, don't need to do anything
                if state.slash_epoch != EPOCH_UNDEFINED {
                    info!("deal {}, already slashed", id);
                    continue;
                }

                // mark the deal for slashing here. Actual releasing of locked funds for the client
                // and slashing of provider collateral happens in cron_tick.
                state.slash_epoch = params.epoch;

                msm.deal_states
                    .as_mut()
                    .unwrap()
                    .set(id as usize, state)
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to set deal state ({})", id),
                        )
                    })?;
            }

            msm.commit_state().map_err(|e| {
                e.downcast_default(ExitCode::ErrIllegalState, "failed to flush state")
            })?;
            Ok(())
        })?;
        Ok(())
    }

    fn compute_data_commitment<BS, RT>(
        rt: &mut RT,
        params: ComputeDataCommitmentParams,
    ) -> Result<ComputeDataCommitmentReturn, ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_type(std::iter::once(&*MINER_ACTOR_CODE_ID))?;

        let st: State = rt.state()?;

        let proposals = DealArray::load(&st.proposals, rt.store()).map_err(|e| {
            e.downcast_default(ExitCode::ErrIllegalState, "failed to load deal proposals")
        })?;
        let mut commds = Vec::with_capacity(params.inputs.len());
        for comm_input in params.inputs.iter() {
            let mut pieces: Vec<PieceInfo> = Vec::with_capacity(comm_input.deal_ids.len());
            for deal_id in &comm_input.deal_ids {
                let deal = proposals
                    .get(*deal_id as usize)
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to get deal_id ({})", deal_id),
                        )
                    })?
                    .ok_or_else(|| {
                        actor_error!(ErrNotFound, "proposal doesn't exist ({})", deal_id)
                    })?;
                pieces.push(PieceInfo {
                    cid: deal.piece_cid,
                    size: deal.piece_size,
                });
            }
            let commd = rt
                .compute_unsealed_sector_cid(comm_input.sector_type, &pieces)
                .map_err(|e| {
                    e.downcast_default(
                        ExitCode::ErrIllegalArgument,
                        "failed to compute unsealed sector CID",
                    )
                })?;
            commds.push(commd);
        }

        Ok(ComputeDataCommitmentReturn { commds })
    }

    fn cron_tick<BS, RT>(rt: &mut RT) -> Result<(), ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        rt.validate_immediate_caller_is(std::iter::once(&*CRON_ACTOR_ADDR))?;

        let mut amount_slashed = BigInt::zero();
        let curr_epoch = rt.curr_epoch();
        let mut timed_out_verified_deals: Vec<DealProposal> = Vec::new();

        rt.transaction(|st: &mut State, rt| {
            let last_cron = st.last_cron;
            let mut updates_needed: AHashMap<ChainEpoch, Vec<DealID>> = AHashMap::new();
            let mut msm = st.mutator(rt.store());
            msm.with_deal_states(Permission::Write)
                .with_locked_table(Permission::Write)
                .with_escrow_table(Permission::Write)
                .with_deals_by_epoch(Permission::Write)
                .with_deal_proposals(Permission::Write)
                .with_pending_proposals(Permission::Write)
                .build()
                .map_err(|e| {
                    e.downcast_default(ExitCode::ErrIllegalState, "failed to load state")
                })?;

            for i in (last_cron + 1)..=rt.curr_epoch() {
                // TODO specs-actors modifies msm as it's iterated through, which is memory unsafe
                // for now the deal ids are being collected and then iterated on, which could
                // cause a potential inconsistency in exit code returned if a deal_id fails
                // to be pulled from storage where it wouldn't be triggered otherwise.
                // Workaround a better solution (seperating msm or fixing go impl)
                let mut deal_ids = Vec::new();
                msm.deals_by_epoch
                    .as_ref()
                    .unwrap()
                    .for_each(i, |deal_id| {
                        deal_ids.push(deal_id);
                        Ok(())
                    })
                    .map_err(|e| {
                        e.downcast_default(ExitCode::ErrIllegalState, "failed to set deal state")
                    })?;

                for deal_id in deal_ids {
                    let deal = msm
                        .deal_proposals
                        .as_ref()
                        .unwrap()
                        .get(deal_id as usize)
                        .map_err(|e| {
                            e.downcast_default(
                                ExitCode::ErrIllegalState,
                                format!("failed to get deal_id ({})", deal_id),
                            )
                        })?
                        .ok_or_else(|| {
                            actor_error!(ErrNotFound, "proposal doesn't exist ({})", deal_id)
                        })?
                        .clone();

                    let dcid = deal.cid().map_err(|e| {
                        ActorError::from(e)
                            .wrap(format!("failed to calculate cid for proposal {}", deal_id))
                    })?;

                    let state = msm
                        .deal_states
                        .as_ref()
                        .unwrap()
                        .get(deal_id as usize)
                        .map_err(|e| {
                            e.downcast_default(
                                ExitCode::ErrIllegalState,
                                "failed to get deal state",
                            )
                        })?
                        .cloned();

                    // deal has been published but not activated yet -> terminate it
                    // as it has timed out
                    if state.is_none() {
                        // Not yet appeared in proven sector; check for timeout.
                        if curr_epoch < deal.start_epoch {
                            return Err(actor_error!(
                                ErrIllegalState,
                                "deal {} processed before start epoch {}",
                                deal_id,
                                deal.start_epoch
                            ));
                        }

                        let slashed = msm.process_deal_init_timed_out(&deal)?;
                        if !slashed.is_zero() {
                            amount_slashed += slashed;
                        }
                        if deal.verified_deal {
                            timed_out_verified_deals.push(deal);
                        }

                        // Delete the proposal (but not state, which doesn't exist).
                        let deleted = msm
                            .deal_proposals
                            .as_mut()
                            .unwrap()
                            .delete(deal_id as usize)
                            .map_err(|e| {
                                e.downcast_default(
                                    ExitCode::ErrIllegalState,
                                    format!("failed to delete deal proposal {}", deal_id),
                                )
                            })?;
                        if deleted.is_none() {
                            return Err(actor_error!(
                                ErrIllegalState,
                                format!(
                                    "failed to delete deal {} proposal {}: does not exist",
                                    deal_id, dcid
                                )
                            ));
                        }
                        msm.pending_deals
                            .as_mut()
                            .unwrap()
                            .delete(&dcid.to_bytes())
                            .map_err(|e| {
                                e.downcast_default(
                                    ExitCode::ErrIllegalState,
                                    format!("failed to delete pending proposal {}", deal_id),
                                )
                            })?
                            .ok_or_else(|| {
                                actor_error!(
                                    ErrIllegalState,
                                    "failed to delete pending proposal: does not exist"
                                )
                            })?;

                        continue;
                    }
                    let mut state = state.unwrap();

                    if state.last_updated_epoch == EPOCH_UNDEFINED {
                        msm.pending_deals
                            .as_mut()
                            .unwrap()
                            .delete(&dcid.to_bytes())
                            .map_err(|e| {
                                e.downcast_default(
                                    ExitCode::ErrIllegalState,
                                    format!("failed to delete pending proposal {}", dcid),
                                )
                            })?
                            .ok_or_else(|| {
                                actor_error!(
                                    ErrIllegalState,
                                    "failed to delete pending proposal: does not exist"
                                )
                            })?;
                    }

                    let (slash_amount, next_epoch, remove_deal) =
                        msm.update_pending_deal_state(&state, &deal, curr_epoch)?;
                    if slash_amount.is_negative() {
                        return Err(actor_error!(
                            ErrIllegalState,
                            format!(
                                "computed negative slash amount {} for deal {}",
                                slash_amount, deal_id
                            )
                        ));
                    }

                    if remove_deal {
                        if next_epoch != EPOCH_UNDEFINED {
                            return Err(actor_error!(
                                ErrIllegalState,
                                format!(
                                    "removed deal {} should have no scheduled epoch (got {})",
                                    deal_id, next_epoch
                                )
                            ));
                        }

                        amount_slashed += slash_amount;

                        // Delete proposal and state simultaneously.
                        let deleted = msm
                            .deal_states
                            .as_mut()
                            .unwrap()
                            .delete(deal_id as usize)
                            .map_err(|e| {
                                e.downcast_default(
                                    ExitCode::ErrIllegalState,
                                    "failed to delete deal state",
                                )
                            })?;
                        if deleted.is_none() {
                            return Err(actor_error!(
                                ErrIllegalState,
                                "failed to delete deal state: does not exist"
                            ));
                        }

                        let deleted = msm
                            .deal_proposals
                            .as_mut()
                            .unwrap()
                            .delete(deal_id as usize)
                            .map_err(|e| {
                                e.downcast_default(
                                    ExitCode::ErrIllegalState,
                                    "failed to delete deal proposal",
                                )
                            })?;
                        if deleted.is_none() {
                            return Err(actor_error!(
                                ErrIllegalState,
                                "failed to delete deal proposal: does not exist"
                            ));
                        }
                    } else {
                        if next_epoch <= rt.curr_epoch() {
                            return Err(actor_error!(
                                ErrIllegalState,
                                "continuing deal {} next epoch {} should be in the future",
                                deal_id,
                                next_epoch
                            ));
                        }
                        if !slash_amount.is_zero() {
                            return Err(actor_error!(
                                ErrIllegalState,
                                "continuing deal {} should not be slashed",
                                deal_id
                            ));
                        }

                        state.last_updated_epoch = curr_epoch;
                        msm.deal_states
                            .as_mut()
                            .unwrap()
                            .set(deal_id as usize, state)
                            .map_err(|e| {
                                e.downcast_default(
                                    ExitCode::ErrIllegalState,
                                    "failed to set deal state",
                                )
                            })?;

                        if let Some(ev) = updates_needed.get_mut(&next_epoch) {
                            ev.push(deal_id);
                        } else {
                            updates_needed.insert(next_epoch, vec![deal_id]);
                        }
                    }
                }
                msm.deals_by_epoch
                    .as_mut()
                    .unwrap()
                    .remove_all(i)
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to delete deal ops for epoch {}", i),
                        )
                    })?;
            }

            // Iterate changes in sorted order to ensure that loads/stores
            // are deterministic. Otherwise, we could end up charging an
            // inconsistent amount of gas.
            let mut changed_epochs: Vec<ChainEpoch> = updates_needed.keys().cloned().collect();
            changed_epochs.sort_unstable();

            for epoch in changed_epochs {
                msm.deals_by_epoch
                    .as_mut()
                    .unwrap()
                    .put_many(
                        epoch,
                        updates_needed.get(&epoch).expect("key checked to exist"),
                    )
                    .map_err(|e| {
                        e.downcast_default(
                            ExitCode::ErrIllegalState,
                            format!("failed to reinsert deal IDs for epoch {}", epoch),
                        )
                    })?;
            }

            msm.st.last_cron = rt.curr_epoch();

            msm.commit_state().map_err(|e| {
                e.downcast_default(ExitCode::ErrIllegalState, "failed to flush state")
            })?;
            Ok(())
        })?;

        for d in timed_out_verified_deals {
            let res = rt.send(
                *VERIFIED_REGISTRY_ACTOR_ADDR,
                VerifregMethod::RestoreBytes as u64,
                Serialized::serialize(RestoreBytesParams {
                    address: d.client,
                    deal_size: BigInt::from(d.piece_size.0),
                })?,
                TokenAmount::zero(),
            );
            if let Err(e) = res {
                log::error!(
                    "failed to send RestoreBytes call to the verifreg actor for timed \
                    out verified deal, client: {}, deal_size: {}, provider: {}, got code: {:?}. {}",
                    d.client,
                    d.piece_size.0,
                    d.provider,
                    e.exit_code(),
                    e.msg()
                );
            }
        }

        if !amount_slashed.is_zero() {
            rt.send(
                *BURNT_FUNDS_ACTOR_ADDR,
                METHOD_SEND,
                Serialized::default(),
                amount_slashed,
            )?;
        }
        Ok(())
    }
}

/// Validates a collection of deal dealProposals for activation, and returns their combined weight,
/// split into regular deal weight and verified deal weight.
pub fn validate_deals_for_activation<BS>(
    st: &State,
    store: &BS,
    deal_ids: &[DealID],
    miner_addr: &Address,
    sector_expiry: ChainEpoch,
    curr_epoch: ChainEpoch,
) -> Result<(BigInt, BigInt, u64), Box<dyn StdError>>
where
    BS: BlockStore,
{
    let proposals = DealArray::load(&st.proposals, store)?;

    validate_and_compute_deal_weight(&proposals, deal_ids, miner_addr, sector_expiry, curr_epoch)
}

pub fn validate_and_compute_deal_weight<BS>(
    proposals: &DealArray<BS>,
    deal_ids: &[DealID],
    miner_addr: &Address,
    sector_expiry: ChainEpoch,
    sector_activation: ChainEpoch,
) -> Result<(BigInt, BigInt, u64), Box<dyn StdError>>
where
    BS: BlockStore,
{
    let mut seen_deal_ids = HashSet::new();
    let mut total_deal_space = 0;
    let mut total_deal_space_time = BigInt::zero();
    let mut total_verified_space_time = BigInt::zero();
    for deal_id in deal_ids {
        if !seen_deal_ids.insert(deal_id) {
            return Err(actor_error!(
                ErrIllegalArgument,
                "deal id {} present multiple times",
                deal_id
            )
            .into());
        }
        let proposal = proposals
            .get(*deal_id as usize)?
            .ok_or_else(|| actor_error!(ErrNotFound, "no such deal {}", deal_id))?;

        validate_deal_can_activate(proposal, miner_addr, sector_expiry, sector_activation)
            .map_err(|e| e.wrap(&format!("cannot activate deal {}", deal_id)))?;

        total_deal_space += proposal.piece_size.0;
        let deal_space_time = deal_weight(proposal);
        if proposal.verified_deal {
            total_verified_space_time += deal_space_time;
        } else {
            total_deal_space_time += deal_space_time;
        }
    }

    Ok((
        total_deal_space_time,
        total_verified_space_time,
        total_deal_space,
    ))
}

fn gen_rand_next_epoch(start_epoch: ChainEpoch, deal_id: DealID) -> ChainEpoch {
    let offset = deal_id as i64 % DEAL_UPDATES_INTERVAL;
    let q = QuantSpec {
        unit: DEAL_UPDATES_INTERVAL,
        offset: 0,
    };
    let prev_day = q.quantize_down(start_epoch);
    if prev_day + offset >= start_epoch {
        return prev_day + offset;
    }
    let next_day = q.quantize_up(start_epoch);
    next_day + offset
}
////////////////////////////////////////////////////////////////////////////////
// Checks
////////////////////////////////////////////////////////////////////////////////
fn validate_deal_can_activate(
    proposal: &DealProposal,
    miner_addr: &Address,
    sector_expiration: ChainEpoch,
    curr_epoch: ChainEpoch,
) -> Result<(), ActorError> {
    if &proposal.provider != miner_addr {
        return Err(actor_error!(
            ErrForbidden,
            "proposal has provider {}, must be {}",
            proposal.provider,
            miner_addr
        ));
    };

    if curr_epoch > proposal.start_epoch {
        return Err(actor_error!(
            ErrIllegalArgument,
            "proposal start epoch {} has already elapsed at {}",
            proposal.start_epoch,
            curr_epoch
        ));
    };

    if proposal.end_epoch > sector_expiration {
        return Err(actor_error!(
            ErrIllegalArgument,
            "proposal expiration {} exceeds sector expiration {}",
            proposal.end_epoch,
            sector_expiration
        ));
    };

    Ok(())
}

fn validate_deal<BS, RT>(
    rt: &RT,
    deal: &ClientDealProposal,
    network_raw_power: &StoragePower,
    baseline_power: &StoragePower,
) -> Result<(), ActorError>
where
    BS: BlockStore,
    RT: Runtime<BS>,
{
    deal_proposal_is_internally_valid(rt, deal)?;

    let proposal = &deal.proposal;

    if proposal.label.len() > DEAL_MAX_LABEL_SIZE {
        return Err(actor_error!(
            ErrIllegalArgument,
            "deal label can be at most {} bytes, is {}",
            DEAL_MAX_LABEL_SIZE,
            proposal.label.len()
        ));
    }

    proposal
        .piece_size
        .validate()
        .map_err(|e| actor_error!(ErrIllegalArgument, "proposal piece size is invalid: {}", e))?;

    // * we are skipping the check for if Cid is defined, but this shouldn't be possible

    if Prefix::from(proposal.piece_cid) != PIECE_CID_PREFIX {
        return Err(actor_error!(
            ErrIllegalArgument,
            "proposal PieceCID undefined"
        ));
    }

    if proposal.end_epoch <= proposal.start_epoch {
        return Err(actor_error!(
            ErrIllegalArgument,
            "proposal end before proposal start"
        ));
    }

    if rt.curr_epoch() > proposal.start_epoch {
        return Err(actor_error!(
            ErrIllegalArgument,
            "Deal start epoch has already elapsed."
        ));
    };

    let (min_dur, max_dur) = deal_duration_bounds(proposal.piece_size);
    if proposal.duration() < min_dur || proposal.duration() > max_dur {
        return Err(actor_error!(
            ErrIllegalArgument,
            "Deal duration out of bounds."
        ));
    };

    let (min_price, max_price) =
        deal_price_per_epoch_bounds(proposal.piece_size, proposal.duration());
    if proposal.storage_price_per_epoch < min_price || &proposal.storage_price_per_epoch > max_price
    {
        return Err(actor_error!(
            ErrIllegalArgument,
            "Storage price out of bounds."
        ));
    };

    let (min_provider_collateral, max_provider_collateral) = deal_provider_collateral_bounds(
        proposal.piece_size,
        network_raw_power,
        baseline_power,
        &rt.total_fil_circ_supply()?,
    );
    if proposal.provider_collateral < min_provider_collateral
        || &proposal.provider_collateral > max_provider_collateral
    {
        return Err(actor_error!(
            ErrIllegalArgument,
            "Provider collateral out of bounds."
        ));
    };

    let (min_client_collateral, max_client_collateral) =
        deal_client_collateral_bounds(proposal.piece_size, proposal.duration());
    if proposal.client_collateral < min_client_collateral
        || &proposal.client_collateral > max_client_collateral
    {
        return Err(actor_error!(
            ErrIllegalArgument,
            "Client collateral out of bounds."
        ));
    };

    Ok(())
}

fn deal_proposal_is_internally_valid<BS, RT>(
    rt: &RT,
    proposal: &ClientDealProposal,
) -> Result<(), ActorError>
where
    BS: BlockStore,
    RT: Runtime<BS>,
{
    // Generate unsigned bytes
    let sv_bz = to_vec(&proposal.proposal)
        .map_err(|e| ActorError::from(e).wrap("failed to serialize DealProposal"))?;

    rt.verify_signature(
        &proposal.client_signature,
        &proposal.proposal.client,
        &sv_bz,
    )
    .map_err(|e| e.downcast_default(ExitCode::ErrIllegalArgument, "signature proposal invalid"))?;

    Ok(())
}

/// Resolves a provider or client address to the canonical form against which a balance should be held, and
/// the designated recipient address of withdrawals (which is the same, for simple account parties).
fn escrow_address<BS, RT>(
    rt: &mut RT,
    addr: &Address,
) -> Result<(Address, Address, Vec<Address>), ActorError>
where
    BS: BlockStore,
    RT: Runtime<BS>,
{
    // Resolve the provided address to the canonical form against which the balance is held.
    let nominal = rt
        .resolve_address(addr)?
        .ok_or_else(|| actor_error!(ErrIllegalArgument, "failed to resolve address {}", addr))?;

    let code_id = rt
        .get_actor_code_cid(&nominal)?
        .ok_or_else(|| actor_error!(ErrIllegalArgument, "no code for address {}", nominal))?;

    if code_id == *MINER_ACTOR_CODE_ID {
        // Storage miner actor entry; implied funds recipient is the associated owner address.
        let (owner_addr, worker_addr, _) = request_miner_control_addrs(rt, nominal)?;
        return Ok((nominal, owner_addr, vec![owner_addr, worker_addr]));
    }

    Ok((nominal, nominal, vec![nominal]))
}

/// Requests the current epoch target block reward from the reward actor.
fn request_current_baseline_power<BS, RT>(rt: &mut RT) -> Result<StoragePower, ActorError>
where
    BS: BlockStore,
    RT: Runtime<BS>,
{
    let rwret = rt.send(
        *REWARD_ACTOR_ADDR,
        reward::Method::ThisEpochReward as u64,
        Serialized::default(),
        0.into(),
    )?;
    let ret: reward::ThisEpochRewardReturn = rwret.deserialize()?;
    Ok(ret.this_epoch_baseline_power)
}

/// Requests the current network total power and pledge from the power actor.
/// Returns a tuple of (raw_power, qa_power).
fn request_current_network_power<BS, RT>(
    rt: &mut RT,
) -> Result<(StoragePower, StoragePower), ActorError>
where
    BS: BlockStore,
    RT: Runtime<BS>,
{
    let rwret = rt.send(
        *STORAGE_POWER_ACTOR_ADDR,
        power::Method::CurrentTotalPower as u64,
        Serialized::default(),
        0.into(),
    )?;
    let ret: power::CurrentTotalPowerReturn = rwret.deserialize()?;
    Ok((ret.raw_byte_power, ret.quality_adj_power))
}

impl ActorCode for Actor {
    fn invoke_method<BS, RT>(
        rt: &mut RT,
        method: MethodNum,
        params: &Serialized,
    ) -> Result<Serialized, ActorError>
    where
        BS: BlockStore,
        RT: Runtime<BS>,
    {
        match FromPrimitive::from_u64(method) {
            Some(Method::Constructor) => {
                Self::constructor(rt)?;
                Ok(Serialized::default())
            }
            Some(Method::AddBalance) => {
                Self::add_balance(rt, rt.deserialize_params(params)?)?;
                Ok(Serialized::default())
            }
            Some(Method::WithdrawBalance) => {
                let res = Self::withdraw_balance(rt, rt.deserialize_params(params)?)?;
                Ok(Serialized::serialize(res)?)
            }
            Some(Method::PublishStorageDeals) => {
                let res = Self::publish_storage_deals(rt, rt.deserialize_params(params)?)?;
                Ok(Serialized::serialize(res)?)
            }
            Some(Method::VerifyDealsForActivation) => {
                let res = Self::verify_deals_for_activation(rt, rt.deserialize_params(params)?)?;
                Ok(Serialized::serialize(res)?)
            }
            Some(Method::ActivateDeals) => {
                Self::activate_deals(rt, rt.deserialize_params(params)?)?;
                Ok(Serialized::default())
            }
            Some(Method::OnMinerSectorsTerminate) => {
                Self::on_miner_sectors_terminate(rt, rt.deserialize_params(params)?)?;
                Ok(Serialized::default())
            }
            Some(Method::ComputeDataCommitment) => {
                let res = Self::compute_data_commitment(rt, rt.deserialize_params(params)?)?;
                Ok(Serialized::serialize(res)?)
            }
            Some(Method::CronTick) => {
                Self::cron_tick(rt)?;
                Ok(Serialized::default())
            }
            None => Err(actor_error!(SysErrInvalidMethod, "Invalid method")),
        }
    }
}
