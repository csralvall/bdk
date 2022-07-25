// Bitcoin Dev Kit
// Written in 2020 by Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020-2021 Bitcoin Dev Kit Developers
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Coin selection
//!
//! This module provides the trait [`CoinSelectionAlgorithm`] that can be implemented to
//! define custom coin selection algorithms.
//!
//! You can specify a custom coin selection algorithm through the [`coin_selection`] method on
//! [`TxBuilder`]. [`DefaultCoinSelectionAlgorithm`] aliases the coin selection algorithm that will
//! be used if it is not explicitly set.
//!
//! [`TxBuilder`]: super::tx_builder::TxBuilder
//! [`coin_selection`]: super::tx_builder::TxBuilder::coin_selection
//!
//! ## Example
//!
//! ```
//! # use std::str::FromStr;
//! # use bitcoin::*;
//! # use bdk::wallet::{self, coin_selection::*};
//! # use bdk::database::Database;
//! # use bdk::*;
//! # const TXIN_BASE_WEIGHT: usize = (32 + 4 + 4 + 1) * 4;
//! #[derive(Debug)]
//! struct AlwaysSpendEverything;
//!
//! impl<D: Database> CoinSelectionAlgorithm<D> for AlwaysSpendEverything {
//!     fn coin_select(
//!         &self,
//!         database: &D,
//!         required_utxos: Vec<WeightedUtxo>,
//!         optional_utxos: Vec<WeightedUtxo>,
//!         fee_rate: FeeRate,
//!         amount_needed: u64,
//!         fee_amount: u64,
//!     ) -> Result<CoinSelectionResult, bdk::Error> {
//!         let mut selected_amount = 0;
//!         let mut additional_weight = 0;
//!         let all_utxos_selected = required_utxos
//!             .into_iter()
//!             .chain(optional_utxos)
//!             .scan(
//!                 (&mut selected_amount, &mut additional_weight),
//!                 |(selected_amount, additional_weight), weighted_utxo| {
//!                     **selected_amount += weighted_utxo.utxo.txout().value;
//!                     **additional_weight += TXIN_BASE_WEIGHT + weighted_utxo.satisfaction_weight;
//!                     Some(weighted_utxo.utxo)
//!                 },
//!             )
//!             .collect::<Vec<_>>();
//!         let additional_fees = fee_rate.fee_wu(additional_weight);
//!         let amount_needed_with_fees = (fee_amount + additional_fees) + amount_needed;
//!         if amount_needed_with_fees > selected_amount {
//!             return Err(bdk::Error::InsufficientFunds {
//!                 needed: amount_needed_with_fees,
//!                 available: selected_amount,
//!             });
//!         }
//!
//!         Ok(CoinSelectionResult {
//!             selected: all_utxos_selected,
//!             fee_amount: fee_amount + additional_fees,
//!         })
//!     }
//! }
//!
//! # let wallet = doctest_wallet!();
//! // create wallet, sync, ...
//!
//! let to_address = Address::from_str("2N4eQYCbKUHCCTUjBJeHcJp9ok6J2GZsTDt").unwrap();
//! let (psbt, details) = {
//!     let mut builder = wallet.build_tx().coin_selection(AlwaysSpendEverything);
//!     builder.add_recipient(to_address.script_pubkey(), 50_000);
//!     builder.finish()?
//! };
//!
//! // inspect, sign, broadcast, ...
//!
//! # Ok::<(), bdk::Error>(())
//! ```

use crate::types::{FeeRate, WeightedScript};
use crate::wallet::utils::IsDust;
use crate::{database::Database, WeightedUtxo};
use crate::{error::Error, Utxo};

use bitcoin::consensus::encode::serialize;
use bitcoin::Script;

use rand::seq::SliceRandom;
#[cfg(not(test))]
use rand::thread_rng;
#[cfg(test)]
use rand::{rngs::StdRng, SeedableRng};
use std::collections::HashMap;
use std::convert::TryInto;

/// Default coin selection algorithm used by [`TxBuilder`](super::tx_builder::TxBuilder) if not
/// overridden
#[cfg(not(test))]
pub type DefaultCoinSelectionAlgorithm = BranchAndBoundCoinSelection;
#[cfg(test)]
pub type DefaultCoinSelectionAlgorithm = LargestFirstCoinSelection; // make the tests more predictable
/// Algorithm to use in case of error in used coin selection algorithm
// If everything fails, simple random selection should work
pub type FallbackCoinSelectionAlgorithm = SingleRandomDrawCoinSelection;

// Base weight of a Txin, not counting the weight needed for satisfying it.
// prev_txid (32 bytes) + prev_vout (4 bytes) + sequence (4 bytes) + script_len (1 bytes)
pub(crate) const TXIN_BASE_WEIGHT: usize = (32 + 4 + 4 + 1) * 4;

#[derive(Debug)]
/// Remaining amount after performing coin selection
pub enum Excess {
    /// It's not possible to create spendable output from excess using the current drain output
    NoChange {
        /// Threshold to consider amount as dust for this particular change script_pubkey
        dust_threshold: u64,
        /// Exceeding amount of current selection over outgoing value and fee costs
        remaining_amount: u64,
        /// The calculated fee for the drain TxOut with the selected script_pubkey
        change_fee: u64,
    },
    /// It's possible to create spendable output from excess using the current drain output
    Change {
        /// Effective amount available to create change after deducting the change output fee
        amount: u64,
        /// The deducted change output fee
        fee: u64,
    },
}

/// Result of a successful coin selection
#[derive(Debug)]
pub struct CoinSelectionResult {
    /// List of outputs selected for use as inputs
    pub selected: Vec<Utxo>,
    /// Total fee amount in satoshi
    pub fee_amount: u64,
    /// Waste value of current coin selection
    pub waste: Waste,
    /// Remaining amount after deducing fees and outgoing outputs
    pub excess: Excess,
}

impl CoinSelectionResult {
    /// The total value of the inputs selected.
    pub fn selected_amount(&self) -> u64 {
        self.selected.iter().map(|u| u.txout().value).sum()
    }

    /// The total value of the inputs selected from the local wallet.
    pub fn local_selected_amount(&self) -> u64 {
        self.selected
            .iter()
            .filter_map(|u| match u {
                Utxo::Local(_) => Some(u.txout().value),
                _ => None,
            })
            .sum()
    }
}

/// Metric introduced to measure the performance of different coin selection algorithms.
///
/// This implementation considers "waste" the sum of two values:
/// * Timing cost
/// * Creation cost
/// > waste = timing_cost + creation_cost
///
/// **Timing cost** is the cost associated with the current fee rate and some long term fee rate used
/// as a threshold to consolidate UTXOs.
/// > timing_cost = txin_size * current_fee_rate - txin_size * long_term_fee_rate
///
/// Timing cost can be negative if the `current_fee_rate` is cheaper than the `long_term_fee_rate`,
/// or zero if they are equal.
///
/// **Creation cost** is the cost associated with the surplus of coins beyond the transaction amount
/// and transaction fees. It can appear in the form of a change output or in the form of excess
/// fees paid to the miner.
///
/// Change cost is derived from the cost of adding the extra output to the transaction and spending
/// that output in the future.
/// > cost_of_change = current_fee_rate * change_output_size + long_term_feerate * change_spend_size
///
/// Excess happens when there is no change, and the surplus of coins is spend as part of the fees
/// to the miner:
/// > excess = tx_total_value - tx_fees - target
///
/// Where _target_ is the amount needed to pay for the fees (minus input fees) and to fulfill the
/// output values of the transaction.
/// > target = sum(tx_outputs) + fee(tx_outputs) + fee(fixed_tx_parts)
///
/// Creation cost can be zero if there is a perfect match as result of the coin selection
/// algorithm.
///
/// So, waste can be zero if creation and timing cost are zero. Or can be negative, if timing cost
/// is negative and the creation cost is low enough (less than the absolute value of timing
/// cost).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Waste(pub i64);
// REVIEW: Add change_output field inside Waste struct?

const LONG_TERM_FEE_RATE: FeeRate = FeeRate::from_sat_per_vb(5.0);

impl Waste {
    /// Calculate the amount of waste for the given coin selection
    ///
    /// - `selected`: the selected output groups
    /// - `drain_script_weight`: spending satisfaction weight of drain script. If script_pubkey
    ///                          belongs to a foreign descriptor, it's satisfaction weight is zero.
    /// - `excess`: the final condition of the exceeding amount of transaction. It is NoChange
    ///             if it's not possible to create change, and Change otherwise.
    /// - `target_amount`: threshold in satoshis used to select UTXOs. It includes the sum of recipient
    ///             outputs, the fees for creating the recipient outputs and the fees for fixed
    ///             transaction parts
    /// - `fee_rate`: fee rate to use
    pub fn calculate(
        selected: &[OutputGroup],
        drain_script_weight: usize,
        excess: &Excess,
    ) -> Result<Waste, Error> {
        // Always consider the cost of spending an input now vs in the future.
        // If fee_rate < LONG_TERM_FEE_RATE, timing cost can be negative
        let timing_cost: i64 = selected.iter().fold(0, |acc, utxo| {
            let fee: i64 = utxo.fee as i64;
            let long_term_fee: i64 = LONG_TERM_FEE_RATE
                .fee_wu(TXIN_BASE_WEIGHT + utxo.weighted_utxo.satisfaction_weight)
                as i64;

            acc + fee - long_term_fee
        });

        // excess < change_output_size x fee_rate + dust_value
        // REVIEW: https://blog.rust-lang.org/2015/04/17/Enums-match-mutation-and-moves.html
        // REVIEW: MATCH ERGONOMICS
        let creation_cost = match *excess {
            Excess::NoChange {
                remaining_amount, ..
            } => remaining_amount,
            Excess::Change { fee, .. } => {
                let change_as_input_fee =
                    LONG_TERM_FEE_RATE.fee_wu(TXIN_BASE_WEIGHT + drain_script_weight);
                fee + change_as_input_fee
            }
        };

        Ok(Waste(timing_cost + creation_cost as i64))
    }
}

/// Trait for generalized coin selection algorithms
///
/// This trait can be implemented to make the [`Wallet`](super::Wallet) use a customized coin
/// selection algorithm when it creates transactions.
///
/// For an example see [this module](crate::wallet::coin_selection)'s documentation.
pub trait CoinSelectionAlgorithm: std::fmt::Debug {
    /// Perform the coin selection
    ///
    /// - `optional_utxos`: the remaining available utxos to satisfy `target_amount` with their
    ///                     weight cost
    /// - `fee_rate`: fee rate to use
    /// - `target_amount`: the outgoing amount in satoshis and the fees already
    ///                    accumulated from added outputs and transaction’s header.
    /// - `available_value`: the total effective value of all the optional utxos
    fn coin_select(
        &self,
        optional_utxos: Vec<OutputGroup>,
        fee_rate: FeeRate,
        target_amount: u64,
        available_value: i64,
    ) -> Result<(Vec<OutputGroup>, u64), Error>;
}

/// Simple and dumb coin selection
///
/// This coin selection algorithm sorts the available UTXOs by value and then picks them starting
/// from the largest ones until the required amount is reached.
#[derive(Debug, Default, Clone, Copy)]
pub struct LargestFirstCoinSelection;

impl CoinSelectionAlgorithm for LargestFirstCoinSelection {
    fn coin_select(
        &self,
        mut optional_utxos: Vec<OutputGroup>,
        _fee_rate: FeeRate,
        target_amount: u64,
        _available_value: i64,
    ) -> Result<(Vec<OutputGroup>, u64), Error> {
        log::debug!("target_amount = `{}`", target_amount,);

        // We put the "required UTXOs" first and make sure the optional UTXOs are sorted,
        // initially smallest to largest, before being reversed with `.rev()`.
        let utxos = {
            optional_utxos.sort_unstable_by_key(|og| og.effective_value);
            optional_utxos.into_iter().rev()
        };

        select_sorted_utxos(utxos, target_amount)
    }
}

/// OldestFirstCoinSelection always picks the utxo with the smallest blockheight to add to the selected coins next
///
/// This coin selection algorithm sorts the available UTXOs by blockheight and then picks them starting
/// from the oldest ones until the required amount is reached.
#[derive(Debug, Default, Clone, Copy)]
pub struct OldestFirstCoinSelection<D: Database> {
    database: D,
}

impl<D> CoinSelectionAlgorithm for OldestFirstCoinSelection<D>
where
    D: Database + std::fmt::Debug + std::default::Default,
{
    fn coin_select(
        &self,
        mut optional_utxos: Vec<OutputGroup>,
        _fee_rate: FeeRate,
        target_amount: u64,
        _available_value: i64,
    ) -> Result<(Vec<OutputGroup>, u64), Error> {
        // query db and create a blockheight lookup table
        let blockheights = optional_utxos
            .iter()
            .map(|og| og.weighted_utxo.utxo.outpoint().txid)
            // fold is used so we can skip db query for txid that already exist in hashmap acc
            .fold(Ok(HashMap::new()), |bh_result_acc, txid| {
                bh_result_acc.and_then(|mut bh_acc| {
                    if bh_acc.contains_key(&txid) {
                        Ok(bh_acc)
                    } else {
                        self.database.get_tx(&txid, false).map(|details| {
                            bh_acc.insert(
                                txid,
                                details.and_then(|d| d.confirmation_time.map(|ct| ct.height)),
                            );
                            bh_acc
                        })
                    }
                })
            })?;

        // We put the "required UTXOs" first and make sure the optional UTXOs are sorted from
        // oldest to newest according to blocktime
        // For utxo that doesn't exist in DB, they will have lowest priority to be selected
        let utxos = {
            optional_utxos.sort_unstable_by_key(|og| {
                match blockheights.get(&og.weighted_utxo.utxo.outpoint().txid) {
                    Some(Some(blockheight)) => blockheight,
                    _ => &u32::MAX,
                }
            });

            optional_utxos.into_iter()
        };

        select_sorted_utxos(utxos, target_amount)
    }
}

fn select_sorted_utxos(
    utxos: impl Iterator<Item = OutputGroup>,
    target_amount: u64,
) -> Result<(Vec<OutputGroup>, u64), Error> {
    let mut selected_amount = 0;
    let mut fee_amount = 0;
    let selected = utxos
        .scan(
            (&mut selected_amount, &mut fee_amount),
            |(selected_amount, fee_amount), output_group| {
                if **selected_amount < target_amount + **fee_amount {
                    **fee_amount += output_group.fee;
                    **selected_amount += output_group.weighted_utxo.utxo.txout().value;

                    log::debug!(
                        "Selected {}, updated fee_amount = `{}`",
                        output_group.weighted_utxo.utxo.outpoint(),
                        fee_amount
                    );

                    Some(output_group)
                } else {
                    None
                }
            },
        )
        .collect::<Vec<_>>();

    Ok((selected, fee_amount))
}

/// Decide if change can be created
///
/// - `remaining_amount`: the amount in which the selected coins exceed the target amount
/// - `fee_rate`: required fee rate for the current selection
/// - `drain_script`: script to consider change creation
pub fn decide_change(remaining_amount: u64, fee_rate: FeeRate, drain_script: &Script) -> Excess {
    // drain_output_len = size(len(script_pubkey)) + len(script_pubkey) + size(output_value)
    let drain_output_len = serialize(drain_script).len() + 8usize;
    let change_fee = fee_rate.fee_vb(drain_output_len);
    let drain_val = remaining_amount.saturating_sub(change_fee);

    if drain_val.is_dust(drain_script) {
        let dust_threshold = drain_script.dust_value().as_sat();
        Excess::NoChange {
            dust_threshold,
            change_fee,
            remaining_amount,
        }
    } else {
        Excess::Change {
            amount: drain_val,
            fee: change_fee,
        }
    }
}

/// Perform the coin selection
///
/// - `required_utxos`: the utxos that must be spent regardless of `target_amount` with their
///                     weight cost
/// - `optional_utxos`: the remaining available utxos to satisfy `target_amount` with their
///                     weight cost
/// - `fee_rate`: fee rate to use
/// - `target_amount`: the outgoing amount in satoshis and the fees already
///                    accumulated from added outputs and transaction’s header.
pub fn get_selection<Cs: CoinSelectionAlgorithm>(
    algorithm: Cs,
    required_utxos: Vec<WeightedUtxo>,
    optional_utxos: Vec<WeightedUtxo>,
    fee_rate: FeeRate,
    target_amount: u64,
    weighted_drain_script: &WeightedScript,
) -> Result<CoinSelectionResult, Error> {
    // ####################################################################
    // ######################### PREPROCESSING ############################
    // ####################################################################

    // Mapping every (UTXO, usize) to an output group
    let mut required_utxos: Vec<OutputGroup> = required_utxos
        .into_iter()
        .map(|u| OutputGroup::new(u, fee_rate))
        .collect();

    // Mapping every (UTXO, usize) to an output group.
    let optional_utxos: Vec<OutputGroup> = optional_utxos
        .into_iter()
        .map(|u| OutputGroup::new(u, fee_rate))
        .collect();

    let req_values = required_utxos.iter().fold((0, 0), |(eff_value, fees), x| {
        (eff_value + x.effective_value, fees + x.fee)
    });

    let opt_values = optional_utxos.iter().fold((0, 0), |(eff_value, fees), x| {
        (eff_value + x.effective_value, fees + x.fee)
    });

    let expected = (opt_values.0 + req_values.0).try_into().map_err(|_| {
        Error::Generic("Sum of UTXO spendable values does not fit into u64".to_string())
    })?;

    if expected < target_amount {
        return Err(Error::InsufficientFunds {
            needed: target_amount,
            available: expected,
        });
    }

    let target_amount_i64 = target_amount
        .try_into()
        .expect("Bitcoin amount to fit into i64");

    // ####################################################################
    // ######################### COIN SELECTION ###########################
    // ####################################################################

    let selection = if req_values.0 > target_amount_i64 {
        // req_values.1 = required_utxos_fee_amount
        // req_values.0 = required_utxos_effective_value
        (required_utxos, req_values.0, req_values.1)
    } else {
        // from now on, target_amount can only be positive
        let target_amount = (target_amount_i64 - req_values.0) as u64;
        let mut opt_selection = match algorithm.coin_select(
            optional_utxos.clone(),
            fee_rate,
            target_amount,
            opt_values.0,
        ) {
            Ok(selection) => selection,
            Err(_err) => {
                let fallback_algorithm = FallbackCoinSelectionAlgorithm {};
                fallback_algorithm.coin_select(
                    optional_utxos,
                    fee_rate,
                    target_amount,
                    opt_values.0,
                )?
            }
        };
        let opt_effective_value: i64 = opt_selection.0.iter().map(|og| og.effective_value).sum();
        opt_selection.0.append(&mut required_utxos);
        (
            // opt_selected_utxos ++ required_utxos
            opt_selection.0,
            // opt_selected_effective_value + required_utxos_effective_value
            opt_effective_value + req_values.0,
            // opt_selected_fee_amount + required_utxos_fee_amount
            opt_selection.1 + req_values.1,
        )
    };

    // ####################################################################
    // ############################ GET EXCESS ############################
    // ####################################################################

    // if coin_select finish, it means it found a valid coin selection. The effective value of that
    // coin selection should be greater than target_amount (a u64) so it's safe to assume it's an
    // u64
    let selection_effective_value = selection.1 as u64;

    // remaining_amount = target_amount - (selection_effective_value + selection_fees)
    let remaining_amount = target_amount - (selection_effective_value + selection.2);

    let excess = decide_change(
        remaining_amount,
        fee_rate,
        &weighted_drain_script.script_pubkey,
    );

    // ####################################################################
    // ############################# GET WASTE ############################
    // ####################################################################

    let waste = Waste::calculate(
        &selection.0,
        weighted_drain_script.satisfaction_weight,
        &excess,
    )?;

    // ####################################################################
    // ######################### GET WEIGHTED_UTXOS #######################
    // ####################################################################

    let selected = selection
        .0
        .into_iter()
        .map(|x| x.weighted_utxo.utxo)
        .collect::<Vec<_>>();

    // ####################################################################
    // ##################### BUILD COIN SELECTION RESULT ##################
    // ####################################################################

    Ok(CoinSelectionResult {
        selected,
        fee_amount: selection.2,
        waste,
        excess,
    })
}

#[derive(Debug, Clone)]
// Adds fee information to an UTXO.
/// OutputGroup stores together the fee and the effective value of each
/// [WeightedUtxo](types::WeightedUtxo)
pub struct OutputGroup {
    // TODO: weighted_utxo: Vec<WeightedUtxo>
    weighted_utxo: WeightedUtxo,
    // Amount of fees for spending a certain utxo, calculated using a certain FeeRate
    fee: u64,
    // The effective value of the UTXO, i.e., the utxo value minus the fee for spending it
    effective_value: i64,
}

impl OutputGroup {
    fn new(weighted_utxo: WeightedUtxo, fee_rate: FeeRate) -> Self {
        let fee = fee_rate.fee_wu(TXIN_BASE_WEIGHT + weighted_utxo.satisfaction_weight);
        let effective_value = weighted_utxo.utxo.txout().value as i64 - fee as i64;
        OutputGroup {
            weighted_utxo,
            fee,
            effective_value,
        }
    }
}

/// Branch and bound coin selection
///
/// Code adapted from Bitcoin Core's implementation and from Mark Erhardt Master's Thesis: <http://murch.one/wp-content/uploads/2016/11/erhardt2016coinselection.pdf>
#[derive(Debug)]
pub struct BranchAndBoundCoinSelection {
    size_of_change: u64,
}

impl Default for BranchAndBoundCoinSelection {
    fn default() -> Self {
        Self {
            // P2WPKH cost of change -> value (8 bytes) + script len (1 bytes) + script (22 bytes)
            size_of_change: 8 + 1 + 22,
        }
    }
}

impl BranchAndBoundCoinSelection {
    /// Create new instance with target size for change output
    pub fn new(size_of_change: u64) -> Self {
        Self { size_of_change }
    }
}

const BNB_TOTAL_TRIES: usize = 100_000;

impl CoinSelectionAlgorithm for BranchAndBoundCoinSelection {
    // TODO: make this more Rust-onic :)
    // (And perhaps refactor with less arguments?)
    fn coin_select(
        &self,
        mut optional_utxos: Vec<OutputGroup>,
        fee_rate: FeeRate,
        // curr_value is already considered in the discount done to target_amount
        //mut curr_value: i64,
        // curr_value has been discounted to target_amount
        target_amount: u64,
        // REVIEW: How to avoid this extra parameter in trait method signature
        // without incurre in performance issues?
        // Clearly it's better to lost 4 bytes for the implemented algorithm once than risking to
        // compute the available_value again for a large number of utxos.
        mut available_value: i64,
    ) -> Result<(Vec<OutputGroup>, u64), Error> {
        // convert target amount ot i64 to use in comparisons and assignments
        let target_amount = target_amount
            .try_into()
            .expect("Bitcoin amount to fit into i64");

        // the value of the current selection (former curr_value)
        let mut selected_value = 0;

        let cost_of_change = self.size_of_change as f32 * fee_rate.as_sat_vb();

        // current_selection[i] will contain true if we are using optional_utxos[i],
        // false otherwise. Note that current_selection.len() could be less than
        // optional_utxos.len(), it just means that we still haven't decided if we should keep
        // certain optional_utxos or not.
        let mut current_selection: Vec<bool> = Vec::with_capacity(optional_utxos.len());

        // Sort the utxo_pool
        optional_utxos.sort_unstable_by_key(|a| a.effective_value);
        optional_utxos.reverse();

        // Contains the best selection we found
        let mut best_selection = Vec::new();
        let mut best_selection_value = None;

        // Depth First search loop for choosing the UTXOs
        for _ in 0..BNB_TOTAL_TRIES {
            // Conditions for starting a backtrack
            let mut backtrack = false;
            // Cannot possibly reach target with the amount remaining in the available_value,
            // or the selected value is out of range.
            // Go back and try other branch
            if selected_value + available_value < target_amount
                || selected_value > target_amount + cost_of_change as i64
            {
                backtrack = true;
            } else if selected_value >= target_amount {
                // Selected value is within range, there's no point in going forward. Start
                // backtracking
                backtrack = true;

                // If we found a solution better than the previous one, or if there wasn't previous
                // solution, update the best solution
                if best_selection_value.is_none() || selected_value < best_selection_value.unwrap()
                {
                    best_selection = current_selection.clone();
                    best_selection_value = Some(selected_value);
                }

                // If we found a perfect match, break here
                if selected_value == target_amount {
                    break;
                }
            }

            // Backtracking, moving backwards
            if backtrack {
                // Walk backwards to find the last included UTXO that still needs to have its omission branch traversed.
                while let Some(false) = current_selection.last() {
                    current_selection.pop();
                    available_value += optional_utxos[current_selection.len()].effective_value;
                }

                if current_selection.last_mut().is_none() {
                    // We have walked back to the first utxo and no branch is untraversed. All solutions searched
                    // If best selection is empty, then there's no exact match
                    if best_selection.is_empty() {
                        return Err(Error::BnBNoExactMatch);
                    }
                    break;
                }

                if let Some(c) = current_selection.last_mut() {
                    // Output was included on previous iterations, try excluding now.
                    *c = false;
                }

                let utxo = &optional_utxos[current_selection.len() - 1];
                selected_value -= utxo.effective_value;
            } else {
                // Moving forwards, continuing down this branch
                let utxo = &optional_utxos[current_selection.len()];

                // Remove this utxo from the available_value utxo amount
                available_value -= utxo.effective_value;

                // Inclusion branch first (Largest First Exploration)
                current_selection.push(true);
                selected_value += utxo.effective_value;
            }
        }

        // Check for solution
        if best_selection.is_empty() {
            return Err(Error::BnBTotalTriesExceeded);
        }

        // Set output set
        let selected_utxos = optional_utxos
            .into_iter()
            .zip(best_selection)
            .filter_map(|(optional, is_in_best)| if is_in_best { Some(optional) } else { None })
            .collect::<Vec<OutputGroup>>();

        let fee_amount = selected_utxos.iter().map(|u| u.fee).sum::<u64>();

        Ok((selected_utxos, fee_amount))
    }
}

/// Single Random Draw coin selection
#[derive(Debug)]
pub struct SingleRandomDrawCoinSelection;

impl CoinSelectionAlgorithm for SingleRandomDrawCoinSelection {
    fn coin_select(
        &self,
        mut optional_utxos: Vec<OutputGroup>,
        _fee_rate: FeeRate,
        target_amount: u64,
        _available_value: i64,
    ) -> Result<(Vec<OutputGroup>, u64), Error> {
        #[cfg(not(test))]
        optional_utxos.shuffle(&mut thread_rng());
        #[cfg(test)]
        {
            let seed = [0; 32];
            let mut rng: StdRng = SeedableRng::from_seed(seed);
            optional_utxos.shuffle(&mut rng);
        }

        // convert target amount ot i64 to use in comparisons and assignments
        let target_amount = target_amount
            .try_into()
            .expect("Bitcoin amount to fit into i64");

        let selected_utxos = optional_utxos
            .into_iter()
            .scan(0, |acc_value, utxo| {
                if *acc_value >= target_amount {
                    None
                } else {
                    *acc_value += utxo.effective_value;
                    Some(utxo)
                }
            })
            .collect::<Vec<_>>();

        let fee_amount = selected_utxos.iter().map(|u| u.fee).sum::<u64>();

        Ok((selected_utxos, fee_amount))
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use bitcoin::{OutPoint, Script, TxOut};

    use super::*;
    use crate::database::{BatchOperations, MemoryDatabase};
    use crate::types::*;
    use crate::wallet::Vbytes;

    use rand::rngs::StdRng;
    use rand::seq::SliceRandom;
    use rand::{Rng, SeedableRng};

    const P2WPKH_WITNESS_SIZE: usize = 73 + 33 + 2;

    const FEE_AMOUNT: u64 = 50;

    fn utxo(value: u64, index: u32) -> WeightedUtxo {
        assert!(index < 10);
        let outpoint = OutPoint::from_str(&format!(
            "000000000000000000000000000000000000000000000000000000000000000{}:0",
            index
        ))
        .unwrap();
        WeightedUtxo {
            satisfaction_weight: P2WPKH_WITNESS_SIZE,
            utxo: Utxo::Local(LocalUtxo {
                outpoint,
                txout: TxOut {
                    value,
                    script_pubkey: Script::new(),
                },
                keychain: KeychainKind::External,
                is_spent: false,
            }),
        }
    }

    fn get_test_utxos() -> Vec<WeightedUtxo> {
        vec![
            utxo(100_000, 0),
            utxo(FEE_AMOUNT as u64 - 40, 1),
            utxo(200_000, 2),
        ]
    }

    fn setup_database_and_get_oldest_first_test_utxos<D: Database>(
        database: &mut D,
    ) -> Vec<WeightedUtxo> {
        // ensure utxos are from different tx
        let utxo1 = utxo(120_000, 1);
        let utxo2 = utxo(80_000, 2);
        let utxo3 = utxo(300_000, 3);

        // add tx to DB so utxos are sorted by blocktime asc
        // utxos will be selected by the following order
        // utxo1(blockheight 1) -> utxo2(blockheight 2), utxo3 (blockheight 3)
        // timestamp are all set as the same to ensure that only block height is used in sorting
        let utxo1_tx_details = TransactionDetails {
            transaction: None,
            txid: utxo1.utxo.outpoint().txid,
            received: 1,
            sent: 0,
            fee: None,
            confirmation_time: Some(BlockTime {
                height: 1,
                timestamp: 1231006505,
            }),
        };

        let utxo2_tx_details = TransactionDetails {
            transaction: None,
            txid: utxo2.utxo.outpoint().txid,
            received: 1,
            sent: 0,
            fee: None,
            confirmation_time: Some(BlockTime {
                height: 2,
                timestamp: 1231006505,
            }),
        };

        let utxo3_tx_details = TransactionDetails {
            transaction: None,
            txid: utxo3.utxo.outpoint().txid,
            received: 1,
            sent: 0,
            fee: None,
            confirmation_time: Some(BlockTime {
                height: 3,
                timestamp: 1231006505,
            }),
        };

        database.set_tx(&utxo1_tx_details).unwrap();
        database.set_tx(&utxo2_tx_details).unwrap();
        database.set_tx(&utxo3_tx_details).unwrap();

        vec![utxo1, utxo2, utxo3]
    }

    fn generate_random_utxos(rng: &mut StdRng, utxos_number: usize) -> Vec<WeightedUtxo> {
        let mut res = Vec::new();
        for _ in 0..utxos_number {
            res.push(WeightedUtxo {
                satisfaction_weight: P2WPKH_WITNESS_SIZE,
                utxo: Utxo::Local(LocalUtxo {
                    outpoint: OutPoint::from_str(
                        "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:0",
                    )
                    .unwrap(),
                    txout: TxOut {
                        value: rng.gen_range(0, 200000000),
                        script_pubkey: Script::new(),
                    },
                    keychain: KeychainKind::External,
                    is_spent: false,
                }),
            });
        }
        res
    }

    fn generate_same_value_utxos(utxos_value: u64, utxos_number: usize) -> Vec<WeightedUtxo> {
        let utxo = WeightedUtxo {
            satisfaction_weight: P2WPKH_WITNESS_SIZE,
            utxo: Utxo::Local(LocalUtxo {
                outpoint: OutPoint::from_str(
                    "ebd9813ecebc57ff8f30797de7c205e3c7498ca950ea4341ee51a685ff2fa30a:0",
                )
                .unwrap(),
                txout: TxOut {
                    value: utxos_value,
                    script_pubkey: Script::new(),
                },
                keychain: KeychainKind::External,
                is_spent: false,
            }),
        };
        vec![utxo; utxos_number]
    }

    fn sum_random_utxos(mut rng: &mut StdRng, utxos: &mut Vec<WeightedUtxo>) -> u64 {
        let utxos_picked_len = rng.gen_range(2, utxos.len() / 2);
        utxos.shuffle(&mut rng);
        utxos[..utxos_picked_len]
            .iter()
            .map(|u| u.utxo.txout().value)
            .sum()
    }

    #[test]
    fn test_largest_first_coin_selection_success() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        let result = LargestFirstCoinSelection::default()
            .coin_select(
                &database,
                utxos,
                vec![],
                FeeRate::from_sat_per_vb(1.0),
                250_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), 300_010);
        assert_eq!(result.fee_amount, 254)
    }

    #[test]
    fn test_largest_first_coin_selection_use_all() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        let result = LargestFirstCoinSelection::default()
            .coin_select(
                &database,
                utxos,
                vec![],
                FeeRate::from_sat_per_vb(1.0),
                20_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), 300_010);
        assert_eq!(result.fee_amount, 254);
    }

    #[test]
    fn test_largest_first_coin_selection_use_only_necessary() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        let result = LargestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                20_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected_amount(), 200_000);
        assert_eq!(result.fee_amount, 118);
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_largest_first_coin_selection_insufficient_funds() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        LargestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                500_000,
                FEE_AMOUNT,
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_largest_first_coin_selection_insufficient_funds_high_fees() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        LargestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1000.0),
                250_000,
                FEE_AMOUNT,
            )
            .unwrap();
    }

    #[test]
    fn test_oldest_first_coin_selection_success() {
        let mut database = MemoryDatabase::default();
        let utxos = setup_database_and_get_oldest_first_test_utxos(&mut database);

        let result = OldestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                180_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 2);
        assert_eq!(result.selected_amount(), 200_000);
        assert_eq!(result.fee_amount, 186)
    }

    #[test]
    fn test_oldest_first_coin_selection_utxo_not_in_db_will_be_selected_last() {
        // ensure utxos are from different tx
        let utxo1 = utxo(120_000, 1);
        let utxo2 = utxo(80_000, 2);
        let utxo3 = utxo(300_000, 3);

        let mut database = MemoryDatabase::default();

        // add tx to DB so utxos are sorted by blocktime asc
        // utxos will be selected by the following order
        // utxo1(blockheight 1) -> utxo2(blockheight 2), utxo3 (not exist in DB)
        // timestamp are all set as the same to ensure that only block height is used in sorting
        let utxo1_tx_details = TransactionDetails {
            transaction: None,
            txid: utxo1.utxo.outpoint().txid,
            received: 1,
            sent: 0,
            fee: None,
            confirmation_time: Some(BlockTime {
                height: 1,
                timestamp: 1231006505,
            }),
        };

        let utxo2_tx_details = TransactionDetails {
            transaction: None,
            txid: utxo2.utxo.outpoint().txid,
            received: 1,
            sent: 0,
            fee: None,
            confirmation_time: Some(BlockTime {
                height: 2,
                timestamp: 1231006505,
            }),
        };

        database.set_tx(&utxo1_tx_details).unwrap();
        database.set_tx(&utxo2_tx_details).unwrap();

        let result = OldestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                vec![utxo3, utxo1, utxo2],
                FeeRate::from_sat_per_vb(1.0),
                180_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 2);
        assert_eq!(result.selected_amount(), 200_000);
        assert_eq!(result.fee_amount, 186)
    }

    #[test]
    fn test_oldest_first_coin_selection_use_all() {
        let mut database = MemoryDatabase::default();
        let utxos = setup_database_and_get_oldest_first_test_utxos(&mut database);

        let result = OldestFirstCoinSelection::default()
            .coin_select(
                &database,
                utxos,
                vec![],
                FeeRate::from_sat_per_vb(1.0),
                20_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), 500_000);
        assert_eq!(result.fee_amount, 254);
    }

    #[test]
    fn test_oldest_first_coin_selection_use_only_necessary() {
        let mut database = MemoryDatabase::default();
        let utxos = setup_database_and_get_oldest_first_test_utxos(&mut database);

        let result = OldestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                20_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected_amount(), 120_000);
        assert_eq!(result.fee_amount, 118);
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_oldest_first_coin_selection_insufficient_funds() {
        let mut database = MemoryDatabase::default();
        let utxos = setup_database_and_get_oldest_first_test_utxos(&mut database);

        OldestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                600_000,
                FEE_AMOUNT,
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_oldest_first_coin_selection_insufficient_funds_high_fees() {
        let mut database = MemoryDatabase::default();
        let utxos = setup_database_and_get_oldest_first_test_utxos(&mut database);

        let amount_needed: u64 =
            utxos.iter().map(|wu| wu.utxo.txout().value).sum::<u64>() - (FEE_AMOUNT + 50);

        OldestFirstCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1000.0),
                amount_needed,
                FEE_AMOUNT,
            )
            .unwrap();
    }

    #[test]
    fn test_bnb_coin_selection_success() {
        // In this case bnb won't find a suitable match and single random draw will
        // select three outputs
        let utxos = generate_same_value_utxos(100_000, 20);

        let database = MemoryDatabase::default();

        let result = BranchAndBoundCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                250_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), 300_000);
        assert_eq!(result.fee_amount, 254);
    }

    #[test]
    fn test_bnb_coin_selection_required_are_enough() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        let result = BranchAndBoundCoinSelection::default()
            .coin_select(
                &database,
                utxos.clone(),
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                20_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), 300_010);
        assert_eq!(result.fee_amount, 254);
    }

    #[test]
    fn test_bnb_coin_selection_optional_are_enough() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        let result = BranchAndBoundCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                299756,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), 300010);
        assert_eq!(result.fee_amount, 254);
    }

    #[test]
    fn test_bnb_coin_selection_required_not_enough() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        let required = vec![utxos[0].clone()];
        let mut optional = utxos[1..].to_vec();
        optional.push(utxo(500_000, 3));

        // Defensive assertions, for sanity and in case someone changes the test utxos vector.
        let amount: u64 = required.iter().map(|u| u.utxo.txout().value).sum();
        assert_eq!(amount, 100_000);
        let amount: u64 = optional.iter().map(|u| u.utxo.txout().value).sum();
        assert!(amount > 150_000);

        let result = BranchAndBoundCoinSelection::default()
            .coin_select(
                &database,
                required,
                optional,
                FeeRate::from_sat_per_vb(1.0),
                150_000,
                FEE_AMOUNT,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 3);
        assert_eq!(result.selected_amount(), 300_010);
        assert!((result.fee_amount as f32 - 254.0).abs() < f32::EPSILON);
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_bnb_coin_selection_insufficient_funds() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        BranchAndBoundCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                500_000,
                FEE_AMOUNT,
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_bnb_coin_selection_insufficient_funds_high_fees() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        BranchAndBoundCoinSelection::default()
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1000.0),
                250_000,
                FEE_AMOUNT,
            )
            .unwrap();
    }

    #[test]
    fn test_bnb_coin_selection_check_fee_rate() {
        let utxos = get_test_utxos();
        let database = MemoryDatabase::default();

        let result = BranchAndBoundCoinSelection::new(0)
            .coin_select(
                &database,
                vec![],
                utxos,
                FeeRate::from_sat_per_vb(1.0),
                99932, // first utxo's effective value
                0,
            )
            .unwrap();

        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected_amount(), 100_000);
        let input_size = (TXIN_BASE_WEIGHT + P2WPKH_WITNESS_SIZE).vbytes();
        let epsilon = 0.5;
        assert!((1.0 - (result.fee_amount as f32 / input_size as f32)).abs() < epsilon);
    }

    #[test]
    fn test_bnb_coin_selection_exact_match() {
        let seed = [0; 32];
        let mut rng: StdRng = SeedableRng::from_seed(seed);
        let database = MemoryDatabase::default();

        for _i in 0..200 {
            let mut optional_utxos = generate_random_utxos(&mut rng, 16);
            let target_amount = sum_random_utxos(&mut rng, &mut optional_utxos);
            let result = BranchAndBoundCoinSelection::new(0)
                .coin_select(
                    &database,
                    vec![],
                    optional_utxos,
                    FeeRate::from_sat_per_vb(0.0),
                    target_amount,
                    0,
                )
                .unwrap();
            assert_eq!(result.selected_amount(), target_amount);
        }
    }

    #[test]
    #[should_panic(expected = "BnBNoExactMatch")]
    fn test_bnb_function_no_exact_match() {
        let fee_rate = FeeRate::from_sat_per_vb(10.0);
        let utxos: Vec<OutputGroup> = get_test_utxos()
            .into_iter()
            .map(|u| OutputGroup::new(u, fee_rate))
            .collect();

        let curr_available_value = utxos.iter().fold(0, |acc, x| acc + x.effective_value);

        let size_of_change = 31;
        let cost_of_change = size_of_change as f32 * fee_rate.as_sat_vb();
        BranchAndBoundCoinSelection::new(size_of_change)
            .bnb(
                vec![],
                utxos,
                0,
                curr_available_value,
                20_000,
                FEE_AMOUNT,
                cost_of_change,
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "BnBTotalTriesExceeded")]
    fn test_bnb_function_tries_exceeded() {
        let fee_rate = FeeRate::from_sat_per_vb(10.0);
        let utxos: Vec<OutputGroup> = generate_same_value_utxos(100_000, 100_000)
            .into_iter()
            .map(|u| OutputGroup::new(u, fee_rate))
            .collect();

        let curr_available_value = utxos.iter().fold(0, |acc, x| acc + x.effective_value);

        let size_of_change = 31;
        let cost_of_change = size_of_change as f32 * fee_rate.as_sat_vb();

        BranchAndBoundCoinSelection::new(size_of_change)
            .bnb(
                vec![],
                utxos,
                0,
                curr_available_value,
                20_000,
                FEE_AMOUNT,
                cost_of_change,
            )
            .unwrap();
    }

    // The match won't be exact but still in the range
    #[test]
    fn test_bnb_function_almost_exact_match_with_fees() {
        let fee_rate = FeeRate::from_sat_per_vb(1.0);
        let size_of_change = 31;
        let cost_of_change = size_of_change as f32 * fee_rate.as_sat_vb();

        let utxos: Vec<_> = generate_same_value_utxos(50_000, 10)
            .into_iter()
            .map(|u| OutputGroup::new(u, fee_rate))
            .collect();

        let curr_value = 0;

        let curr_available_value = utxos.iter().fold(0, |acc, x| acc + x.effective_value);

        // 2*(value of 1 utxo)  - 2*(1 utxo fees with 1.0sat/vbyte fee rate) -
        // cost_of_change + 5.
        let target_amount = 2 * 50_000 - 2 * 67 - cost_of_change.ceil() as i64 + 5;

        let result = BranchAndBoundCoinSelection::new(size_of_change)
            .bnb(
                vec![],
                utxos,
                curr_value,
                curr_available_value,
                target_amount,
                FEE_AMOUNT,
                cost_of_change,
            )
            .unwrap();
        assert_eq!(result.selected_amount(), 100_000);
        assert_eq!(result.fee_amount, 186);
    }

    // TODO: bnb() function should be optimized, and this test should be done with more utxos
    #[test]
    fn test_bnb_function_exact_match_more_utxos() {
        let seed = [0; 32];
        let mut rng: StdRng = SeedableRng::from_seed(seed);
        let fee_rate = FeeRate::from_sat_per_vb(0.0);

        for _ in 0..200 {
            let optional_utxos: Vec<_> = generate_random_utxos(&mut rng, 40)
                .into_iter()
                .map(|u| OutputGroup::new(u, fee_rate))
                .collect();

            let curr_value = 0;

            let curr_available_value = optional_utxos
                .iter()
                .fold(0, |acc, x| acc + x.effective_value);

            let target_amount =
                optional_utxos[3].effective_value + optional_utxos[23].effective_value;

            let result = BranchAndBoundCoinSelection::new(0)
                .bnb(
                    vec![],
                    optional_utxos,
                    curr_value,
                    curr_available_value,
                    target_amount,
                    0,
                    0.0,
                )
                .unwrap();
            assert_eq!(result.selected_amount(), target_amount as u64);
        }
    }

    #[test]
    fn test_single_random_draw_function_success() {
        let seed = [0; 32];
        let mut rng: StdRng = SeedableRng::from_seed(seed);
        let mut utxos = generate_random_utxos(&mut rng, 300);
        let target_amount = sum_random_utxos(&mut rng, &mut utxos);

        let fee_rate = FeeRate::from_sat_per_vb(1.0);
        let utxos: Vec<OutputGroup> = utxos
            .into_iter()
            .map(|u| OutputGroup::new(u, fee_rate))
            .collect();

        let result = BranchAndBoundCoinSelection::default().single_random_draw(
            vec![],
            utxos,
            0,
            target_amount as i64,
            FEE_AMOUNT,
        );

        assert!(result.selected_amount() > target_amount);
        assert_eq!(result.fee_amount, (50 + result.selected.len() * 68) as u64);
    }
}
