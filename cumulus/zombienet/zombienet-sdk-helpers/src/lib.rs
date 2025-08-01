// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use codec::{Compact, Decode};
use cumulus_primitives_core::{relay_chain, rpsr_digest::RPSR_CONSENSUS_ID};
use futures::stream::StreamExt;
use polkadot_primitives::{vstaging::CandidateReceiptV2, Id as ParaId};
use std::{cmp::max, collections::HashMap, ops::Range};
use tokio::{
	join,
	time::{sleep, Duration},
};
use zombienet_sdk::subxt::{
	self,
	blocks::Block,
	config::{signed_extensions::CheckMortalityParams, substrate::DigestItem, ExtrinsicParams},
	dynamic::Value,
	events::Events,
	ext::scale_value::value,
	tx::{signer::Signer, DynamicPayload, TxStatus},
	utils::H256,
	Config, OnlineClient, PolkadotConfig,
};

// Maximum number of blocks to wait for a session change.
// If it does not arrive for whatever reason, we should not wait forever.
const WAIT_MAX_BLOCKS_FOR_SESSION: u32 = 50;

/// Create a batch call to assign cores to a parachain.
pub fn create_assign_core_call(core_and_para: &[(u32, u32)]) -> DynamicPayload {
	let mut assign_cores = vec![];
	for (core, para_id) in core_and_para.iter() {
		assign_cores.push(value! {
			Coretime(assign_core { core : *core, begin: 0, assignment: ((Task(*para_id), 57600)), end_hint: None() })
		});
	}

	zombienet_sdk::subxt::tx::dynamic(
		"Sudo",
		"sudo",
		vec![value! {
			Utility(batch { calls: assign_cores })
		}],
	)
}

/// Find an event in subxt `Events` and attempt to decode the fields fo the event.
fn find_event_and_decode_fields<T: Decode>(
	events: &Events<PolkadotConfig>,
	pallet: &str,
	variant: &str,
) -> Result<Vec<T>, anyhow::Error> {
	let mut result = vec![];
	for event in events.iter() {
		let event = event?;
		if event.pallet_name() == pallet && event.variant_name() == variant {
			let field_bytes = event.field_bytes().to_vec();
			result.push(T::decode(&mut &field_bytes[..])?);
		}
	}
	Ok(result)
}

// Helper function for asserting the throughput of parachains, after the first session change.
//
// The throughput is measured as total number of backed candidates in a window of relay chain
// blocks. Relay chain blocks with session changes are generally ignores.
pub async fn assert_para_throughput(
	relay_client: &OnlineClient<PolkadotConfig>,
	stop_after: u32,
	expected_candidate_ranges: HashMap<ParaId, Range<u32>>,
) -> Result<(), anyhow::Error> {
	let mut blocks_sub = relay_client.blocks().subscribe_finalized().await?;
	let mut candidate_count: HashMap<ParaId, u32> = HashMap::new();
	let mut current_block_count = 0;

	let valid_para_ids: Vec<ParaId> = expected_candidate_ranges.keys().cloned().collect();

	// Wait for the first session, block production on the parachain will start after that.
	wait_for_first_session_change(&mut blocks_sub).await?;

	while let Some(block) = blocks_sub.next().await {
		let block = block?;
		log::debug!("Finalized relay chain block {}", block.number());
		let events = block.events().await?;
		let is_session_change = events.iter().any(|event| {
			event.as_ref().is_ok_and(|event| {
				event.pallet_name() == "Session" && event.variant_name() == "NewSession"
			})
		});

		// Do not count blocks with session changes, no backed blocks there.
		if is_session_change {
			continue;
		}

		current_block_count += 1;

		let receipts = find_event_and_decode_fields::<CandidateReceiptV2<H256>>(
			&events,
			"ParaInclusion",
			"CandidateBacked",
		)?;

		for receipt in receipts {
			let para_id = receipt.descriptor.para_id();
			log::debug!("Block backed for para_id {para_id}");
			if !valid_para_ids.contains(&para_id) {
				return Err(anyhow!("Invalid ParaId detected: {}", para_id));
			};
			*(candidate_count.entry(para_id).or_default()) += 1;
		}

		if current_block_count == stop_after {
			break;
		}
	}

	log::info!(
		"Reached {stop_after} finalized relay chain blocks that contain backed candidates. The per-parachain distribution is: {:#?}",
		candidate_count.iter().map(|(para_id, count)| format!("{para_id} has {count} backed candidates")).collect::<Vec<_>>()
	);

	for (para_id, expected_candidate_range) in expected_candidate_ranges {
		let actual = candidate_count
			.get(&para_id)
			.ok_or_else(|| anyhow!("ParaId did not have any backed candidates"))?;

		if !expected_candidate_range.contains(actual) {
			return Err(anyhow!(
				"Candidate count {actual} not within range {expected_candidate_range:?}"
			))
		}
	}

	Ok(())
}

/// Wait for the first block with a session change.
///
/// The session change is detected by inspecting the events in the block.
pub async fn wait_for_first_session_change(
	blocks_sub: &mut zombienet_sdk::subxt::backend::StreamOfResults<
		Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	>,
) -> Result<(), anyhow::Error> {
	wait_for_nth_session_change(blocks_sub, 1).await
}

/// Wait for the first block with the Nth session change.
///
/// The session change is detected by inspecting the events in the block.
pub async fn wait_for_nth_session_change(
	blocks_sub: &mut zombienet_sdk::subxt::backend::StreamOfResults<
		Block<PolkadotConfig, OnlineClient<PolkadotConfig>>,
	>,
	mut sessions_to_wait: u32,
) -> Result<(), anyhow::Error> {
	let mut waited_block_num = 0;
	while let Some(block) = blocks_sub.next().await {
		let block = block?;
		log::debug!("Finalized relay chain block {}", block.number());
		let events = block.events().await?;
		let is_session_change = events.iter().any(|event| {
			event.as_ref().is_ok_and(|event| {
				event.pallet_name() == "Session" && event.variant_name() == "NewSession"
			})
		});

		if is_session_change {
			sessions_to_wait -= 1;
			if sessions_to_wait == 0 {
				return Ok(());
			}

			waited_block_num = 0;
		} else {
			if waited_block_num >= WAIT_MAX_BLOCKS_FOR_SESSION {
				return Err(anyhow::format_err!("Waited for {WAIT_MAX_BLOCKS_FOR_SESSION}, a new session should have been arrived by now."));
			}

			waited_block_num += 1;
		}
	}
	Ok(())
}

// Helper function that asserts the maximum finality lag.
pub async fn assert_finality_lag(
	client: &OnlineClient<PolkadotConfig>,
	maximum_lag: u32,
) -> Result<(), anyhow::Error> {
	let mut best_stream = client.blocks().subscribe_best().await?;
	let mut fut_stream = client.blocks().subscribe_finalized().await?;
	let (Some(Ok(best)), Some(Ok(finalized))) = join!(best_stream.next(), fut_stream.next()) else {
		return Err(anyhow::format_err!("Unable to fetch best an finalized block!"));
	};
	let finality_lag = best.number() - finalized.number();

	log::info!(
		"Finality lagged by {finality_lag} blocks, maximum expected was {maximum_lag} blocks"
	);

	assert!(finality_lag <= maximum_lag, "Expected finality to lag by a maximum of {maximum_lag} blocks, but was lagging by {finality_lag} blocks.");
	Ok(())
}

/// Assert that finality has not stalled.
pub async fn assert_blocks_are_being_finalized(
	client: &OnlineClient<PolkadotConfig>,
) -> Result<(), anyhow::Error> {
	let sleep_duration = Duration::from_secs(12);
	let mut finalized_blocks = client.blocks().subscribe_finalized().await?;
	let first_measurement = finalized_blocks
		.next()
		.await
		.ok_or(anyhow::anyhow!("Can't get finalized block from stream"))??
		.number();
	sleep(sleep_duration).await;
	let second_measurement = finalized_blocks
		.next()
		.await
		.ok_or(anyhow::anyhow!("Can't get finalized block from stream"))??
		.number();

	log::info!(
		"Finalized {} blocks within {sleep_duration:?}",
		second_measurement - first_measurement
	);
	assert!(second_measurement > first_measurement);

	Ok(())
}

/// Asserts that parachain blocks have the correct relay parent offset.
///
/// # Arguments
///
/// * `relay_client` - Client connected to a relay chain node
/// * `para_client` - Client connected to a parachain node
/// * `offset` - Expected minimum offset between relay parent and highest seen relay block
/// * `block_limit` - Number of parachain blocks to verify before completing
pub async fn assert_relay_parent_offset(
	relay_client: &OnlineClient<PolkadotConfig>,
	para_client: &OnlineClient<PolkadotConfig>,
	offset: u32,
	block_limit: u32,
) -> Result<(), anyhow::Error> {
	let mut relay_block_stream = relay_client.blocks().subscribe_all().await?;

	// First parachain header #0 does not contains RSPR digest item.
	let mut para_block_stream = para_client.blocks().subscribe_all().await?.skip(1);
	let mut highest_relay_block_seen = 0;
	let mut num_para_blocks_seen = 0;
	loop {
		tokio::select! {
			Some(Ok(relay_block)) = relay_block_stream.next() => {
				highest_relay_block_seen = max(relay_block.number(), highest_relay_block_seen);
				if highest_relay_block_seen > 15 && num_para_blocks_seen == 0 {
					return Err(anyhow!("No parachain blocks produced!"))
				}
			},
			Some(Ok(para_block)) = para_block_stream.next() => {
				let logs = &para_block.header().digest.logs;

				let Some((_, relay_parent_number)): Option<(H256, u32)> = logs.iter().find_map(extract_relay_parent_storage_root) else {
					return Err(anyhow!("No RPSR digest found in header #{}", para_block.number()));
				};
				log::debug!("Parachain block #{} was built on relay parent #{relay_parent_number}, highest seen was {highest_relay_block_seen}", para_block.number());
				assert!(highest_relay_block_seen < offset || relay_parent_number <= highest_relay_block_seen.saturating_sub(offset), "Relay parent is not at the correct offset! relay_parent: #{relay_parent_number} highest_seen_relay_block: #{highest_relay_block_seen}");
				num_para_blocks_seen += 1;
				if num_para_blocks_seen >= block_limit {
					log::info!("Successfully verified relay parent offset of {offset} for {num_para_blocks_seen} parachain blocks.");
					break;
				}
			}
		}
	}
	Ok(())
}

/// Extract relay parent information from the digest logs.
fn extract_relay_parent_storage_root(
	digest: &DigestItem,
) -> Option<(relay_chain::Hash, relay_chain::BlockNumber)> {
	match digest {
		DigestItem::Consensus(id, val) if id == &RPSR_CONSENSUS_ID => {
			let (h, n): (relay_chain::Hash, Compact<relay_chain::BlockNumber>) =
				Decode::decode(&mut &val[..]).ok()?;

			Some((h, n.0))
		},
		_ => None,
	}
}

/// Submits the given `call` as transaction and waits for it successful finalization.
///
/// The transaction is send as immortal transaction.
pub async fn submit_extrinsic_and_wait_for_finalization_success<S: Signer<PolkadotConfig>>(
	client: &OnlineClient<PolkadotConfig>,
	call: &DynamicPayload,
	signer: &S,
) -> Result<(), anyhow::Error> {
	let mut extensions: <<PolkadotConfig as Config>::ExtrinsicParams as ExtrinsicParams<
		PolkadotConfig,
	>>::Params = Default::default();

	extensions.4 = CheckMortalityParams::<PolkadotConfig>::immortal();

	let mut tx = client
		.tx()
		.create_signed(call, signer, extensions)
		.await?
		.submit_and_watch()
		.await?;

	// Below we use the low level API to replicate the `wait_for_in_block` behaviour
	// which was removed in subxt 0.33.0. See https://github.com/paritytech/subxt/pull/1237.
	while let Some(status) = tx.next().await {
		let status = status?;
		match &status {
			TxStatus::InBestBlock(tx_in_block) | TxStatus::InFinalizedBlock(tx_in_block) => {
				let _result = tx_in_block.wait_for_success().await?;
				let block_status =
					if status.as_finalized().is_some() { "Finalized" } else { "Best" };
				log::info!("[{}] In block: {:#?}", block_status, tx_in_block.block_hash());
			},
			TxStatus::Error { message } |
			TxStatus::Invalid { message } |
			TxStatus::Dropped { message } => {
				return Err(anyhow::format_err!("Error submitting tx: {message}"));
			},
			_ => continue,
		}
	}
	Ok(())
}

/// Submits the given `call` as transaction and waits `timeout_secs` for it successful finalization.
///
/// If the transaction does not reach the finalized state in `timeout_secs` an error is returned.
/// The transaction is send as immortal transaction.
pub async fn submit_extrinsic_and_wait_for_finalization_success_with_timeout<
	S: Signer<PolkadotConfig>,
>(
	client: &OnlineClient<PolkadotConfig>,
	call: &DynamicPayload,
	signer: &S,
	timeout_secs: impl Into<u64>,
) -> Result<(), anyhow::Error> {
	let secs = timeout_secs.into();
	let res = tokio::time::timeout(
		Duration::from_secs(secs),
		submit_extrinsic_and_wait_for_finalization_success(client, call, signer),
	)
	.await;

	match res {
		Ok(Ok(_)) => Ok(()),
		Ok(Err(e)) => Err(anyhow!("Error waiting for metric: {}", e)),
		// timeout
		Err(_) => Err(anyhow!("Timeout ({secs}), waiting for extrinsic finalization")),
	}
}

/// Asserts that the given `para_id` is registered at the relay chain.
pub async fn assert_para_is_registered(
	relay_client: &OnlineClient<PolkadotConfig>,
	para_id: ParaId,
	blocks_to_wait: u32,
) -> Result<(), anyhow::Error> {
	let mut blocks_sub = relay_client.blocks().subscribe_all().await?;
	let para_id: u32 = para_id.into();

	let keys: Vec<Value> = vec![];
	let query = subxt::dynamic::storage("Paras", "Parachains", keys);

	let mut blocks_cnt = 0;
	while let Some(block) = blocks_sub.next().await {
		let block = block?;
		log::debug!("Relay block #{}, checking if para_id {para_id} is registered", block.number(),);
		let parachains = block.storage().fetch(&query).await?;

		let parachains: Vec<u32> = match parachains {
			Some(parachains) => parachains.as_type()?,
			None => vec![],
		};

		log::debug!("Registered para_ids: {:?}", parachains);

		if parachains.iter().any(|p| para_id.eq(p)) {
			log::debug!("para_id {para_id} registered");
			return Ok(());
		}
		if blocks_cnt >= blocks_to_wait {
			return Err(anyhow!(
				"Parachain {para_id} not registered within {blocks_to_wait} blocks"
			));
		}
		blocks_cnt += 1;
	}

	Err(anyhow!("No more blocks to check"))
}
