// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Substrate
use sc_consensus_grandpa::AuthorityId as GrandpaId;
use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
use sp_consensus_babe::AuthorityId as BabeId;
use sp_consensus_beefy::ecdsa_crypto::AuthorityId as BeefyId;
use sp_core::storage::Storage;
use sp_keyring::Sr25519Keyring as Keyring;

// Polkadot
use polkadot_primitives::{AssignmentId, ValidatorId};

// Cumulus
use emulated_integration_tests_common::{
	accounts, build_genesis_storage, get_host_config, validators,
};
use parachains_common::Balance;
use rococo_runtime_constants::currency::UNITS as ROC;

pub const ED: Balance = rococo_runtime_constants::currency::EXISTENTIAL_DEPOSIT;
const ENDOWMENT: u128 = 1_000_000 * ROC;

fn session_keys(
	babe: BabeId,
	grandpa: GrandpaId,
	para_validator: ValidatorId,
	para_assignment: AssignmentId,
	authority_discovery: AuthorityDiscoveryId,
	beefy: BeefyId,
) -> rococo_runtime::SessionKeys {
	rococo_runtime::SessionKeys {
		babe,
		grandpa,
		para_validator,
		para_assignment,
		authority_discovery,
		beefy,
	}
}

pub fn genesis() -> Storage {
	let genesis_config = rococo_runtime::RuntimeGenesisConfig {
		system: rococo_runtime::SystemConfig::default(),
		balances: rococo_runtime::BalancesConfig {
			balances: accounts::init_balances().iter().map(|k| (k.clone(), ENDOWMENT)).collect(),
			..Default::default()
		},
		session: rococo_runtime::SessionConfig {
			keys: validators::initial_authorities()
				.iter()
				.map(|x| {
					(
						x.0.clone(),
						x.0.clone(),
						session_keys(
							x.2.clone(),
							x.3.clone(),
							x.4.clone(),
							x.5.clone(),
							x.6.clone(),
							x.7.clone(),
						),
					)
				})
				.collect::<Vec<_>>(),
			..Default::default()
		},
		babe: rococo_runtime::BabeConfig {
			authorities: Default::default(),
			epoch_config: rococo_runtime::BABE_GENESIS_EPOCH_CONFIG,
			..Default::default()
		},
		sudo: rococo_runtime::SudoConfig { key: Some(Keyring::Alice.to_account_id()) },
		configuration: rococo_runtime::ConfigurationConfig { config: get_host_config() },
		registrar: rococo_runtime::RegistrarConfig {
			next_free_para_id: polkadot_primitives::LOWEST_PUBLIC_ID,
			..Default::default()
		},
		..Default::default()
	};

	build_genesis_storage(&genesis_config, rococo_runtime::WASM_BINARY.unwrap())
}
