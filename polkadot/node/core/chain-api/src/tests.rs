// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::*;

use codec::Encode;
use futures::{channel::oneshot, future::BoxFuture};
use std::collections::BTreeMap;

use polkadot_node_primitives::BlockWeight;
use polkadot_node_subsystem_test_helpers::{make_subsystem_context, TestSubsystemContextHandle};
use polkadot_node_subsystem_types::ChainApiBackend;
use polkadot_primitives::{Block, BlockNumber, Hash, Header};
use sp_blockchain::Info as BlockInfo;
use sp_core::testing::TaskExecutor;

#[derive(Clone)]
struct TestClient {
	blocks: BTreeMap<Hash, BlockNumber>,
	block_weights: BTreeMap<Hash, BlockWeight>,
	finalized_blocks: BTreeMap<BlockNumber, Hash>,
	headers: BTreeMap<Hash, Header>,
}

const GENESIS: Hash = Hash::repeat_byte(0xAA);
const ONE: Hash = Hash::repeat_byte(0x01);
const TWO: Hash = Hash::repeat_byte(0x02);
const THREE: Hash = Hash::repeat_byte(0x03);
const FOUR: Hash = Hash::repeat_byte(0x04);
const ERROR_PATH: Hash = Hash::repeat_byte(0xFF);

fn default_header() -> Header {
	Header {
		parent_hash: Hash::zero(),
		number: 100500,
		state_root: Hash::zero(),
		extrinsics_root: Hash::zero(),
		digest: Default::default(),
	}
}

impl Default for TestClient {
	fn default() -> Self {
		Self {
			blocks: maplit::btreemap! {
				GENESIS => 0,
				ONE => 1,
				TWO => 2,
				THREE => 3,
				FOUR => 4,
			},
			block_weights: maplit::btreemap! {
				ONE => 0,
				TWO => 1,
				THREE => 1,
				FOUR => 2,
			},
			finalized_blocks: maplit::btreemap! {
				1 => ONE,
				3 => THREE,
			},
			headers: maplit::btreemap! {
				GENESIS => Header {
					parent_hash: Hash::zero(), // Dummy parent with zero hash.
					number: 0,
					..default_header()
				},
				ONE => Header {
					parent_hash: GENESIS,
					number: 1,
					..default_header()
				},
				TWO => Header {
					parent_hash: ONE,
					number: 2,
					..default_header()
				},
				THREE => Header {
					parent_hash: TWO,
					number: 3,
					..default_header()
				},
				FOUR => Header {
					parent_hash: THREE,
					number: 4,
					..default_header()
				},
				ERROR_PATH => Header {
					..default_header()
				}
			},
		}
	}
}

fn last_key_value<K: Clone, V: Clone>(map: &BTreeMap<K, V>) -> (K, V) {
	assert!(!map.is_empty());
	map.iter().last().map(|(k, v)| (k.clone(), v.clone())).unwrap()
}

impl sp_blockchain::HeaderBackend<Block> for TestClient {
	fn info(&self) -> BlockInfo<Block> {
		let genesis_hash = self.blocks.iter().next().map(|(h, _)| *h).unwrap();
		let (best_hash, best_number) = last_key_value(&self.blocks);
		let (finalized_number, finalized_hash) = last_key_value(&self.finalized_blocks);

		BlockInfo {
			best_hash,
			best_number,
			genesis_hash,
			finalized_hash,
			finalized_number,
			number_leaves: 0,
			finalized_state: None,
			block_gap: None,
		}
	}
	fn number(&self, hash: Hash) -> sp_blockchain::Result<Option<BlockNumber>> {
		Ok(self.blocks.get(&hash).copied())
	}
	fn hash(&self, number: BlockNumber) -> sp_blockchain::Result<Option<Hash>> {
		Ok(self.finalized_blocks.get(&number).copied())
	}
	fn header(&self, hash: Hash) -> sp_blockchain::Result<Option<Header>> {
		if hash.is_zero() {
			Err(sp_blockchain::Error::Backend("Zero hashes are illegal!".into()))
		} else {
			Ok(self.headers.get(&hash).cloned())
		}
	}
	fn status(&self, _hash: Hash) -> sp_blockchain::Result<sp_blockchain::BlockStatus> {
		unimplemented!()
	}
}

fn test_harness(
	test: impl FnOnce(
		Arc<TestClient>,
		TestSubsystemContextHandle<ChainApiMessage>,
	) -> BoxFuture<'static, ()>,
) {
	let (ctx, ctx_handle) = make_subsystem_context(TaskExecutor::new());
	let client = Arc::new(TestClient::default());

	let subsystem = ChainApiSubsystem::new(client.clone(), Metrics(None));
	let chain_api_task = run(ctx, subsystem).map(|x| x.unwrap());
	let test_task = test(client, ctx_handle);

	futures::executor::block_on(future::join(chain_api_task, test_task));
}

impl AuxStore for TestClient {
	fn insert_aux<
		'a,
		'b: 'a,
		'c: 'a,
		I: IntoIterator<Item = &'a (&'c [u8], &'c [u8])>,
		D: IntoIterator<Item = &'a &'b [u8]>,
	>(
		&self,
		_insert: I,
		_delete: D,
	) -> sp_blockchain::Result<()> {
		unimplemented!()
	}

	fn get_aux(&self, key: &[u8]) -> sp_blockchain::Result<Option<Vec<u8>>> {
		Ok(self
			.block_weights
			.iter()
			.find(|(hash, _)| sc_consensus_babe::aux_schema::block_weight_key(hash) == key)
			.map(|(_, weight)| weight.encode()))
	}
}

#[test]
fn request_block_number() {
	test_harness(|client, mut sender| {
		async move {
			let zero = Hash::zero();
			let test_cases = [
				(TWO, client.number(TWO).await.unwrap()),
				(zero, client.number(zero).await.unwrap()), // not here
			];
			for (hash, expected) in &test_cases {
				let (tx, rx) = oneshot::channel();

				sender
					.send(FromOrchestra::Communication {
						msg: ChainApiMessage::BlockNumber(*hash, tx),
					})
					.await;

				assert_eq!(rx.await.unwrap().unwrap(), *expected);
			}

			sender.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		}
		.boxed()
	})
}

#[test]
fn request_block_header() {
	test_harness(|client, mut sender| {
		async move {
			const NOT_HERE: Hash = Hash::repeat_byte(0x5);
			let test_cases = [
				(TWO, client.header(TWO).await.unwrap()),
				(NOT_HERE, client.header(NOT_HERE).await.unwrap()),
			];
			for (hash, expected) in &test_cases {
				let (tx, rx) = oneshot::channel();

				sender
					.send(FromOrchestra::Communication {
						msg: ChainApiMessage::BlockHeader(*hash, tx),
					})
					.await;

				assert_eq!(rx.await.unwrap().unwrap(), *expected);
			}

			sender.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		}
		.boxed()
	})
}

#[test]
fn request_block_weight() {
	test_harness(|client, mut sender| {
		async move {
			const NOT_HERE: Hash = Hash::repeat_byte(0x5);
			let test_cases = [
				(TWO, sc_consensus_babe::block_weight(&*client, TWO).unwrap()),
				(FOUR, sc_consensus_babe::block_weight(&*client, FOUR).unwrap()),
				(NOT_HERE, sc_consensus_babe::block_weight(&*client, NOT_HERE).unwrap()),
			];
			for (hash, expected) in &test_cases {
				let (tx, rx) = oneshot::channel();

				sender
					.send(FromOrchestra::Communication {
						msg: ChainApiMessage::BlockWeight(*hash, tx),
					})
					.await;

				assert_eq!(rx.await.unwrap().unwrap(), *expected);
			}

			sender.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		}
		.boxed()
	})
}

#[test]
fn request_finalized_hash() {
	test_harness(|client, mut sender| {
		async move {
			let test_cases = [
				(1, client.hash(1).await.unwrap()), // not here
				(2, client.hash(2).await.unwrap()),
			];
			for (number, expected) in &test_cases {
				let (tx, rx) = oneshot::channel();

				sender
					.send(FromOrchestra::Communication {
						msg: ChainApiMessage::FinalizedBlockHash(*number, tx),
					})
					.await;

				assert_eq!(rx.await.unwrap().unwrap(), *expected);
			}

			sender.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		}
		.boxed()
	})
}

#[test]
fn request_last_finalized_number() {
	test_harness(|client, mut sender| {
		async move {
			let (tx, rx) = oneshot::channel();

			let expected = client.info().await.unwrap().finalized_number;
			sender
				.send(FromOrchestra::Communication {
					msg: ChainApiMessage::FinalizedBlockNumber(tx),
				})
				.await;

			assert_eq!(rx.await.unwrap().unwrap(), expected);

			sender.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		}
		.boxed()
	})
}

#[test]
fn request_ancestors() {
	test_harness(|_client, mut sender| {
		async move {
			let (tx, rx) = oneshot::channel();
			sender
				.send(FromOrchestra::Communication {
					msg: ChainApiMessage::Ancestors { hash: THREE, k: 4, response_channel: tx },
				})
				.await;
			assert_eq!(rx.await.unwrap().unwrap(), vec![TWO, ONE, GENESIS]);

			// Limit the number of ancestors.
			let (tx, rx) = oneshot::channel();
			sender
				.send(FromOrchestra::Communication {
					msg: ChainApiMessage::Ancestors { hash: TWO, k: 1, response_channel: tx },
				})
				.await;
			assert_eq!(rx.await.unwrap().unwrap(), vec![ONE]);

			// Ancestor of block #1 is returned.
			let (tx, rx) = oneshot::channel();
			sender
				.send(FromOrchestra::Communication {
					msg: ChainApiMessage::Ancestors { hash: ONE, k: 10, response_channel: tx },
				})
				.await;
			assert_eq!(rx.await.unwrap().unwrap(), vec![GENESIS]);

			// No ancestors of genesis block.
			let (tx, rx) = oneshot::channel();
			sender
				.send(FromOrchestra::Communication {
					msg: ChainApiMessage::Ancestors { hash: GENESIS, k: 10, response_channel: tx },
				})
				.await;
			assert_eq!(rx.await.unwrap().unwrap(), Vec::new());

			let (tx, rx) = oneshot::channel();
			sender
				.send(FromOrchestra::Communication {
					msg: ChainApiMessage::Ancestors {
						hash: ERROR_PATH,
						k: 2,
						response_channel: tx,
					},
				})
				.await;
			assert!(rx.await.unwrap().is_err());

			sender.send(FromOrchestra::Signal(OverseerSignal::Conclude)).await;
		}
		.boxed()
	})
}
