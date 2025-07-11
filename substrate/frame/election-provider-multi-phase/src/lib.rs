// This file is part of Substrate.

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

//! # Multi phase, offchain election provider pallet.
//!
//! Currently, this election-provider has two distinct phases (see [`Phase`]), **signed** and
//! **unsigned**.
//!
//! ## Phases
//!
//! The timeline of pallet is as follows. At each block,
//! [`frame_election_provider_support::ElectionDataProvider::next_election_prediction`] is used to
//! estimate the time remaining to the next call to
//! [`frame_election_provider_support::ElectionProvider::elect`]. Based on this, a phase is chosen.
//! The timeline is as follows.
//!
//! ```ignore
//!                                                                    elect()
//!                 +   <--T::SignedPhase-->  +  <--T::UnsignedPhase-->   +
//!   +-------------------------------------------------------------------+
//!    Phase::Off   +       Phase::Signed     +      Phase::Unsigned      +
//! ```
//!
//! Note that the unsigned phase starts [`pallet::Config::UnsignedPhase`] blocks before the
//! `next_election_prediction`, but only ends when a call to [`ElectionProvider::elect`] happens. If
//! no `elect` happens, the signed phase is extended.
//!
//! > Given this, it is rather important for the user of this pallet to ensure it always terminates
//! election via `elect` before requesting a new one.
//!
//! Each of the phases can be disabled by essentially setting their length to zero. If both phases
//! have length zero, then the pallet essentially runs only the fallback strategy, denoted by
//! [`Config::Fallback`].
//!
//! ### Signed Phase
//!
//! In the signed phase, solutions (of type [`RawSolution`]) are submitted and queued on chain. A
//! deposit is reserved, based on the size of the solution, for the cost of keeping this solution
//! on-chain for a number of blocks, and the potential weight of the solution upon being checked. A
//! maximum of `pallet::Config::SignedMaxSubmissions` solutions are stored. The queue is always
//! sorted based on score (worse to best).
//!
//! Upon arrival of a new solution:
//!
//! 1. If the queue is not full, it is stored in the appropriate sorted index.
//! 2. If the queue is full but the submitted solution is better than one of the queued ones, the
//!    worse solution is discarded, the bond of the outgoing solution is returned, and the new
//!    solution is stored in the correct index.
//! 3. If the queue is full and the solution is not an improvement compared to any of the queued
//!    ones, it is instantly rejected and no additional bond is reserved.
//!
//! A signed solution cannot be reversed, taken back, updated, or retracted. In other words, the
//! origin can not bail out in any way, if their solution is queued.
//!
//! Upon the end of the signed phase, the solutions are examined from best to worse (i.e. `pop()`ed
//! until drained). Each solution undergoes an expensive `Pallet::feasibility_check`, which ensures
//! the score claimed by this score was correct, and it is valid based on the election data (i.e.
//! votes and targets). At each step, if the current best solution passes the feasibility check,
//! it is considered to be the best one. The sender of the origin is rewarded, and the rest of the
//! queued solutions get their deposit back and are discarded, without being checked.
//!
//! The following example covers all of the cases at the end of the signed phase:
//!
//! ```ignore
//! Queue
//! +-------------------------------+
//! |Solution(score=20, valid=false)| +-->  Slashed
//! +-------------------------------+
//! |Solution(score=15, valid=true )| +-->  Rewarded, Saved
//! +-------------------------------+
//! |Solution(score=10, valid=true )| +-->  Discarded
//! +-------------------------------+
//! |Solution(score=05, valid=false)| +-->  Discarded
//! +-------------------------------+
//! |             None              |
//! +-------------------------------+
//! ```
//!
//! Note that both of the bottom solutions end up being discarded and get their deposit back,
//! despite one of them being *invalid*.
//!
//! ## Unsigned Phase
//!
//! The unsigned phase will always follow the signed phase, with the specified duration. In this
//! phase, only validator nodes can submit solutions. A validator node who has offchain workers
//! enabled will start to mine a solution in this phase and submits it back to the chain as an
//! unsigned transaction, thus the name _unsigned_ phase. This unsigned transaction can never be
//! valid if propagated, and it acts similar to an inherent.
//!
//! Validators will only submit solutions if the one that they have computed is strictly better than
//! the best queued one and will limit the weight of the solution to [`MinerConfig::MaxWeight`].
//!
//! The unsigned phase can be made passive depending on how the previous signed phase went, by
//! setting the first inner value of [`Phase`] to `false`. For now, the signed phase is always
//! active.
//!
//! ### Fallback
//!
//! If we reach the end of both phases (i.e. call to [`ElectionProvider::elect`] happens) and no
//! good solution is queued, then the fallback strategy [`pallet::Config::Fallback`] is used to
//! determine what needs to be done. The on-chain election is slow, and contains no balancing or
//! reduction post-processing. If [`pallet::Config::Fallback`] fails, the next phase
//! [`Phase::Emergency`] is enabled, which is a more *fail-safe* approach.
//!
//! ### Emergency Phase
//!
//! If, for any of the below reasons:
//!
//! 1. No **signed** or **unsigned** solution submitted, and no successful [`Config::Fallback`] is
//!    provided
//! 2. Any other unforeseen internal error
//!
//! A call to `T::ElectionProvider::elect` is made, and `Ok(_)` cannot be returned, then the pallet
//! proceeds to the [`Phase::Emergency`]. During this phase, any solution can be submitted from
//! [`Config::ForceOrigin`], without any checking, via [`Pallet::set_emergency_election_result`]
//! transaction. Hence, `[`Config::ForceOrigin`]` should only be set to a trusted origin, such as
//! the council or root. Once submitted, the forced solution is kept in [`QueuedSolution`] until the
//! next call to `T::ElectionProvider::elect`, where it is returned and [`Phase`] goes back to
//! `Off`.
//!
//! This implies that the user of this pallet (i.e. a staking pallet) should re-try calling
//! `T::ElectionProvider::elect` in case of error, until `OK(_)` is returned.
//!
//! To generate an emergency solution, one must only provide one argument: [`Supports`]. This is
//! essentially a collection of elected winners for the election, and voters who support them. The
//! supports can be generated by any means. In the simplest case, it could be manual. For example,
//! in the case of massive network failure or misbehavior, [`Config::ForceOrigin`] might decide to
//! select only a small number of emergency winners (which would greatly restrict the next validator
//! set, if this pallet is used with `pallet-staking`). If the failure is for other technical
//! reasons, then a simple and safe way to generate supports is using the staking-miner binary
//! provided in the Polkadot repository. This binary has a subcommand named `emergency-solution`
//! which is capable of connecting to a live network, and generating appropriate `supports` using a
//! standard algorithm, and outputting the `supports` in hex format, ready for submission. Note that
//! while this binary lives in the Polkadot repository, this particular subcommand of it can work
//! against any substrate-based chain.
//!
//! See the [`staking-miner`](https://github.com/paritytech/staking-miner-v2) docs for more
//! information.
//!
//! ## Feasible Solution (correct solution)
//!
//! All submissions must undergo a feasibility check. Signed solutions are checked one by one at the
//! end of the signed phase, and the unsigned solutions are checked on the spot. A feasible solution
//! is as follows:
//!
//! 0. **all** of the used indices must be correct.
//! 1. present *exactly* correct number of winners.
//! 2. any assignment is checked to match with [`RoundSnapshot::voters`].
//! 3. the claimed score is valid, based on the fixed point arithmetic accuracy.
//!
//! ## Accuracy
//!
//! The accuracy of the election is configured via [`SolutionAccuracyOf`] which is the accuracy that
//! the submitted solutions must adhere to.
//!
//! Note that the accuracy is of great importance. The offchain solution should be as small as
//! possible, reducing solutions size/weight.
//!
//! ## Error types
//!
//! This pallet provides a verbose error system to ease future debugging and debugging. The overall
//! hierarchy of errors is as follows:
//!
//! 1. [`pallet::Error`]: These are the errors that can be returned in the dispatchables of the
//!    pallet, either signed or unsigned. Since decomposition with nested enums is not possible
//!    here, they are prefixed with the logical sub-system to which they belong.
//! 2. [`ElectionError`]: These are the errors that can be generated while the pallet is doing
//!    something in automatic scenarios, such as `offchain_worker` or `on_initialize`. These errors
//!    are helpful for logging and are thus nested as:
//!    - [`ElectionError::Miner`]: wraps a [`unsigned::MinerError`].
//!    - [`ElectionError::Feasibility`]: wraps a [`FeasibilityError`].
//!    - [`ElectionError::Fallback`]: wraps a fallback error.
//!    - [`ElectionError::DataProvider`]: wraps a static str.
//!
//! Note that there could be an overlap between these sub-errors. For example, A
//! `SnapshotUnavailable` can happen in both miner and feasibility check phase.
//!
//!	## Multi-page election support
//!
//! The [`frame_election_provider_support::ElectionDataProvider`] and
//! [`frame_election_provider_support::ElectionProvider`] traits used by this pallet can support a
//! multi-page election.
//!
//! However, this pallet only supports single-page election and data
//! provider and all the relevant trait implementation and configurations reflect that assumption.
//!
//! If external callers request the election of a page index higher than 0, the election will fail
//! with [`ElectionError::MultiPageNotSupported`].
//!
//! ## Future Plans
//!
//! **Emergency-phase recovery script**: This script should be taken out of staking-miner in
//! polkadot and ideally live in `substrate/utils/frame/elections`.
//!
//! **Challenge Phase**. We plan on adding a third phase to the pallet, called the challenge phase.
//! This is a phase in which no further solutions are processed, and the current best solution might
//! be challenged by anyone (signed or unsigned). The main plan here is to enforce the solution to
//! be PJR. Checking PJR on-chain is quite expensive, yet proving that a solution is **not** PJR is
//! rather cheap. If a queued solution is successfully proven bad:
//!
//! 1. We must surely slash whoever submitted that solution (might be a challenge for unsigned
//!    solutions).
//! 2. We will fallback to the emergency strategy (likely extending the current era).
//!
//! **Bailing out**. The functionality of bailing out of a queued solution is nice. A miner can
//! submit a solution as soon as they _think_ it is high probability feasible, and do the checks
//! afterwards, and remove their solution (for a small cost of probably just transaction fees, or a
//! portion of the bond).
//!
//! **Conditionally open unsigned phase**: Currently, the unsigned phase is always opened. This is
//! useful because an honest validator will run substrate OCW code, which should be good enough to
//! trump a mediocre or malicious signed submission (assuming in the absence of honest signed bots).
//! If there are signed submissions, they can be checked against an absolute measure (e.g. PJR),
//! then we can only open the unsigned phase in extreme conditions (i.e. "no good signed solution
//! received") to spare some work for the active validators.
//!
//! **Allow smaller solutions and build up**: For now we only allow solutions that are exactly
//! [`DesiredTargets`], no more, no less. Over time, we can change this to a [min, max] where any
//! solution within this range is acceptable, where bigger solutions are prioritized.
//!
//! **Score based on (byte) size**: We should always prioritize small solutions over bigger ones, if
//! there is a tie. Even more harsh should be to enforce the bound of the `reduce` algorithm.
//!
//! **Take into account the encode/decode weight in benchmarks.** Currently, we only take into
//! account the weight of encode/decode in the `submit_unsigned` given its priority. Nonetheless,
//! all operations on the solution and the snapshot are worthy of taking this into account.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
use codec::{Decode, DecodeWithMemTracking, Encode};
use frame_election_provider_support::{
	bounds::{CountBound, ElectionBounds, SizeBound},
	BoundedSupports, BoundedSupportsOf, ElectionDataProvider, ElectionProvider,
	InstantElectionProvider, NposSolution, PageIndex,
};
use frame_support::{
	dispatch::DispatchClass,
	ensure,
	traits::{Currency, Get, OnUnbalanced, ReservableCurrency},
	weights::Weight,
	DefaultNoBound, EqNoBound, PartialEqNoBound,
};
use frame_system::{ensure_none, offchain::CreateBare, pallet_prelude::BlockNumberFor};
use scale_info::TypeInfo;
use sp_arithmetic::{
	traits::{CheckedAdd, Zero},
	UpperOf,
};
use sp_npos_elections::{ElectionScore, IdentifierT, Supports, VoteWeight};
use sp_runtime::{
	transaction_validity::{
		InvalidTransaction, TransactionPriority, TransactionSource, TransactionValidity,
		TransactionValidityError, ValidTransaction,
	},
	DispatchError, ModuleError, PerThing, Perbill, RuntimeDebug, SaturatedConversion,
};

#[cfg(feature = "try-runtime")]
use sp_runtime::TryRuntimeError;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(all(test, feature = "remote-mining"))]
mod remote_mining;
#[macro_use]
pub mod helpers;

/// This pallet only supports a single page election flow.
pub(crate) const SINGLE_PAGE: u32 = 0;
const LOG_TARGET: &str = "runtime::election-provider";

pub mod migrations;
pub mod signed;
pub mod unsigned;
pub mod weights;

pub use signed::{
	BalanceOf, GeometricDepositBase, NegativeImbalanceOf, PositiveImbalanceOf, SignedSubmission,
	SignedSubmissionOf, SignedSubmissions, SubmissionIndicesOf,
};
use unsigned::VoterOf;
pub use unsigned::{Miner, MinerConfig};
pub use weights::WeightInfo;

/// The solution type used by this crate.
pub type SolutionOf<T> = <T as MinerConfig>::Solution;
/// The voter index. Derived from [`SolutionOf`].
pub type SolutionVoterIndexOf<T> = <SolutionOf<T> as NposSolution>::VoterIndex;
/// The target index. Derived from [`SolutionOf`].
pub type SolutionTargetIndexOf<T> = <SolutionOf<T> as NposSolution>::TargetIndex;
/// The accuracy of the election, when submitted from offchain. Derived from [`SolutionOf`].
pub type SolutionAccuracyOf<T> =
	<SolutionOf<<T as crate::Config>::MinerConfig> as NposSolution>::Accuracy;
/// A ready solution parameterized with this pallet's miner config.
pub type ReadySolutionOf<T> = ReadySolution<
	<T as MinerConfig>::AccountId,
	<T as MinerConfig>::MaxWinners,
	<T as MinerConfig>::MaxBackersPerWinner,
>;
/// The fallback election type.
pub type FallbackErrorOf<T> = <<T as crate::Config>::Fallback as ElectionProvider>::Error;

/// Configuration for the benchmarks of the pallet.
pub trait BenchmarkingConfig {
	/// Range of voters.
	const VOTERS: [u32; 2];
	/// Range of targets.
	const TARGETS: [u32; 2];
	/// Range of active voters.
	const ACTIVE_VOTERS: [u32; 2];
	/// Range of desired targets.
	const DESIRED_TARGETS: [u32; 2];
	/// Maximum number of voters expected. This is used only for memory-benchmarking of snapshot.
	const SNAPSHOT_MAXIMUM_VOTERS: u32;
	/// Maximum number of voters expected. This is used only for memory-benchmarking of miner.
	const MINER_MAXIMUM_VOTERS: u32;
	/// Maximum number of targets expected. This is used only for memory-benchmarking.
	const MAXIMUM_TARGETS: u32;
}

/// Current phase of the pallet.
#[derive(PartialEq, Eq, Clone, Copy, Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo)]
pub enum Phase<Bn> {
	/// Nothing, the election is not happening.
	Off,
	/// Signed phase is open.
	Signed,
	/// Unsigned phase. First element is whether it is active or not, second the starting block
	/// number.
	///
	/// We do not yet check whether the unsigned phase is active or passive. The intent is for the
	/// blockchain to be able to declare: "I believe that there exists an adequate signed
	/// solution," advising validators not to bother running the unsigned offchain worker.
	///
	/// As validator nodes are free to edit their OCW code, they could simply ignore this advisory
	/// and always compute their own solution. However, by default, when the unsigned phase is
	/// passive, the offchain workers will not bother running.
	Unsigned((bool, Bn)),
	/// The emergency phase. This is enabled upon a failing call to `T::ElectionProvider::elect`.
	/// After that, the only way to leave this phase is through a successful
	/// `T::ElectionProvider::elect`.
	Emergency,
}

impl<Bn> Default for Phase<Bn> {
	fn default() -> Self {
		Phase::Off
	}
}

impl<Bn: PartialEq + Eq> Phase<Bn> {
	/// Whether the phase is emergency or not.
	pub fn is_emergency(&self) -> bool {
		matches!(self, Phase::Emergency)
	}

	/// Whether the phase is signed or not.
	pub fn is_signed(&self) -> bool {
		matches!(self, Phase::Signed)
	}

	/// Whether the phase is unsigned or not.
	pub fn is_unsigned(&self) -> bool {
		matches!(self, Phase::Unsigned(_))
	}

	/// Whether the phase is unsigned and open or not, with specific start.
	pub fn is_unsigned_open_at(&self, at: Bn) -> bool {
		matches!(self, Phase::Unsigned((true, real)) if *real == at)
	}

	/// Whether the phase is unsigned and open or not.
	pub fn is_unsigned_open(&self) -> bool {
		matches!(self, Phase::Unsigned((true, _)))
	}

	/// Whether the phase is off or not.
	pub fn is_off(&self) -> bool {
		matches!(self, Phase::Off)
	}
}

/// The type of `Computation` that provided this election data.
#[derive(PartialEq, Eq, Clone, Copy, Encode, Decode, DecodeWithMemTracking, Debug, TypeInfo)]
pub enum ElectionCompute {
	/// Election was computed on-chain.
	OnChain,
	/// Election was computed with a signed submission.
	Signed,
	/// Election was computed with an unsigned submission.
	Unsigned,
	/// Election was computed using the fallback
	Fallback,
	/// Election was computed with emergency status.
	Emergency,
}

impl Default for ElectionCompute {
	fn default() -> Self {
		ElectionCompute::OnChain
	}
}

/// A raw, unchecked solution.
///
/// This is what will get submitted to the chain.
///
/// Such a solution should never become effective in anyway before being checked by the
/// `Pallet::feasibility_check`.
#[derive(
	PartialEq,
	Eq,
	Clone,
	Encode,
	Decode,
	DecodeWithMemTracking,
	RuntimeDebug,
	PartialOrd,
	Ord,
	TypeInfo,
)]
pub struct RawSolution<S> {
	/// the solution itself.
	pub solution: S,
	/// The _claimed_ score of the solution.
	pub score: ElectionScore,
	/// The round at which this solution should be submitted.
	pub round: u32,
}

impl<C: Default> Default for RawSolution<C> {
	fn default() -> Self {
		// Round 0 is always invalid, only set this to 1.
		Self { round: 1, solution: Default::default(), score: Default::default() }
	}
}

/// A checked solution, ready to be enacted.
#[derive(
	PartialEqNoBound,
	EqNoBound,
	Clone,
	Encode,
	Decode,
	RuntimeDebug,
	DefaultNoBound,
	scale_info::TypeInfo,
)]
#[scale_info(skip_type_params(AccountId, MaxWinners, MaxBackersPerWinner))]
pub struct ReadySolution<AccountId, MaxWinners, MaxBackersPerWinner>
where
	AccountId: IdentifierT,
	MaxWinners: Get<u32>,
	MaxBackersPerWinner: Get<u32>,
{
	/// The final supports of the solution.
	///
	/// This is target-major vector, storing each winners, total backing, and each individual
	/// backer.
	pub supports: BoundedSupports<AccountId, MaxWinners, MaxBackersPerWinner>,
	/// The score of the solution.
	///
	/// This is needed to potentially challenge the solution.
	pub score: ElectionScore,
	/// How this election was computed.
	pub compute: ElectionCompute,
}

/// A snapshot of all the data that is needed for en entire round. They are provided by
/// [`ElectionDataProvider`] and are kept around until the round is finished.
///
/// These are stored together because they are often accessed together.
#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug, Default, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct RoundSnapshot<AccountId, VoterType> {
	/// All of the voters.
	pub voters: Vec<VoterType>,
	/// All of the targets.
	pub targets: Vec<AccountId>,
}

/// Encodes the length of a solution or a snapshot.
///
/// This is stored automatically on-chain, and it contains the **size of the entire snapshot**.
/// This is also used in dispatchables as weight witness data and should **only contain the size of
/// the presented solution**, not the entire snapshot.
#[derive(
	PartialEq, Eq, Clone, Copy, Encode, Decode, DecodeWithMemTracking, Debug, Default, TypeInfo,
)]
pub struct SolutionOrSnapshotSize {
	/// The length of voters.
	#[codec(compact)]
	pub voters: u32,
	/// The length of targets.
	#[codec(compact)]
	pub targets: u32,
}

/// Internal errors of the pallet.
///
/// Note that this is different from [`pallet::Error`].
#[derive(frame_support::DebugNoBound)]
#[cfg_attr(feature = "runtime-benchmarks", derive(strum::IntoStaticStr))]
pub enum ElectionError<T: Config> {
	/// An error happened in the feasibility check sub-system.
	Feasibility(FeasibilityError),
	/// An error in the miner (offchain) sub-system.
	Miner(unsigned::MinerError),
	/// An error happened in the data provider.
	DataProvider(&'static str),
	/// An error nested in the fallback.
	Fallback(FallbackErrorOf<T>),
	/// An error occurred when requesting an election result. The caller expects a multi-paged
	/// election, which this pallet does not support.
	MultiPageNotSupported,
	/// No solution has been queued.
	NothingQueued,
}

// NOTE: we have to do this manually because of the additional where clause needed on
// `FallbackErrorOf<T>`.
impl<T: Config> PartialEq for ElectionError<T>
where
	FallbackErrorOf<T>: PartialEq,
{
	fn eq(&self, other: &Self) -> bool {
		use ElectionError::*;
		match (self, other) {
			(Feasibility(x), Feasibility(y)) if x == y => true,
			(Miner(x), Miner(y)) if x == y => true,
			(DataProvider(x), DataProvider(y)) if x == y => true,
			(Fallback(x), Fallback(y)) if x == y => true,
			(MultiPageNotSupported, MultiPageNotSupported) => true,
			(NothingQueued, NothingQueued) => true,
			_ => false,
		}
	}
}

impl<T: Config> From<FeasibilityError> for ElectionError<T> {
	fn from(e: FeasibilityError) -> Self {
		ElectionError::Feasibility(e)
	}
}

impl<T: Config> From<unsigned::MinerError> for ElectionError<T> {
	fn from(e: unsigned::MinerError) -> Self {
		ElectionError::Miner(e)
	}
}

/// Errors that can happen in the feasibility check.
#[derive(Debug, Eq, PartialEq)]
#[cfg_attr(feature = "runtime-benchmarks", derive(strum::IntoStaticStr))]
pub enum FeasibilityError {
	/// Wrong number of winners presented.
	WrongWinnerCount,
	/// The snapshot is not available.
	///
	/// Kinda defensive: The pallet should technically never attempt to do a feasibility check when
	/// no snapshot is present.
	SnapshotUnavailable,
	/// Internal error from the election crate.
	NposElection(sp_npos_elections::Error),
	/// A vote is invalid.
	InvalidVote,
	/// A voter is invalid.
	InvalidVoter,
	/// The given score was invalid.
	InvalidScore,
	/// The provided round is incorrect.
	InvalidRound,
	/// Comparison against `MinimumUntrustedScore` failed.
	UntrustedScoreTooLow,
	/// Data Provider returned too many desired targets
	TooManyDesiredTargets,
	/// Conversion into bounded types failed.
	///
	/// Should never happen under correct configurations.
	BoundedConversionFailed,
}

impl From<sp_npos_elections::Error> for FeasibilityError {
	fn from(e: sp_npos_elections::Error) -> Self {
		FeasibilityError::NposElection(e)
	}
}

pub use pallet::*;
#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_election_provider_support::{InstantElectionProvider, NposSolver};
	use frame_support::{pallet_prelude::*, traits::EstimateCallFee};
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::Convert;

	#[pallet::config]
	pub trait Config: frame_system::Config + CreateBare<Call<Self>> {
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>
			+ TryInto<Event<Self>>;

		/// Currency type.
		type Currency: ReservableCurrency<Self::AccountId> + Currency<Self::AccountId>;

		/// Something that can predict the fee of a call. Used to sensibly distribute rewards.
		type EstimateCallFee: EstimateCallFee<Call<Self>, BalanceOf<Self>>;

		/// Duration of the unsigned phase.
		type UnsignedPhase: Get<BlockNumberFor<Self>>;
		/// Duration of the signed phase.
		type SignedPhase: Get<BlockNumberFor<Self>>;

		/// The minimum amount of improvement to the solution score that defines a solution as
		/// "better" in the Signed phase.
		#[pallet::constant]
		type BetterSignedThreshold: Get<Perbill>;

		/// The repeat threshold of the offchain worker.
		///
		/// For example, if it is 5, that means that at least 5 blocks will elapse between attempts
		/// to submit the worker's solution.
		#[pallet::constant]
		type OffchainRepeat: Get<BlockNumberFor<Self>>;

		/// The priority of the unsigned transaction submitted in the unsigned-phase
		#[pallet::constant]
		type MinerTxPriority: Get<TransactionPriority>;

		/// Configurations of the embedded miner.
		///
		/// Any external software implementing this can use the [`unsigned::Miner`] type provided,
		/// which can mine new solutions and trim them accordingly.
		type MinerConfig: crate::unsigned::MinerConfig<
			AccountId = Self::AccountId,
			MaxVotesPerVoter = <Self::DataProvider as ElectionDataProvider>::MaxVotesPerVoter,
			MaxWinners = Self::MaxWinners,
			MaxBackersPerWinner = Self::MaxBackersPerWinner,
		>;

		/// Maximum number of signed submissions that can be queued.
		///
		/// It is best to avoid adjusting this during an election, as it impacts downstream data
		/// structures. In particular, `SignedSubmissionIndices<T>` is bounded on this value. If you
		/// update this value during an election, you _must_ ensure that
		/// `SignedSubmissionIndices.len()` is less than or equal to the new value. Otherwise,
		/// attempts to submit new solutions may cause a runtime panic.
		#[pallet::constant]
		type SignedMaxSubmissions: Get<u32>;

		/// Maximum weight of a signed solution.
		///
		/// If [`Config::MinerConfig`] is being implemented to submit signed solutions (outside of
		/// this pallet), then [`MinerConfig::solution_weight`] is used to compare against
		/// this value.
		#[pallet::constant]
		type SignedMaxWeight: Get<Weight>;

		/// The maximum amount of unchecked solutions to refund the call fee for.
		#[pallet::constant]
		type SignedMaxRefunds: Get<u32>;

		/// Base reward for a signed solution
		#[pallet::constant]
		type SignedRewardBase: Get<BalanceOf<Self>>;

		/// Per-byte deposit for a signed solution.
		#[pallet::constant]
		type SignedDepositByte: Get<BalanceOf<Self>>;

		/// Per-weight deposit for a signed solution.
		#[pallet::constant]
		type SignedDepositWeight: Get<BalanceOf<Self>>;

		/// Maximum number of winners that an election supports.
		///
		/// Note: This must always be greater or equal to `T::DataProvider::desired_targets()`.
		#[pallet::constant]
		type MaxWinners: Get<u32>;

		/// Maximum number of voters that can support a winner in an election solution.
		///
		/// This is needed to ensure election computation is bounded.
		#[pallet::constant]
		type MaxBackersPerWinner: Get<u32>;

		/// Something that calculates the signed deposit base based on the signed submissions queue
		/// size.
		type SignedDepositBase: Convert<usize, BalanceOf<Self>>;

		/// The maximum number of electing voters and electable targets to put in the snapshot.
		type ElectionBounds: Get<ElectionBounds>;

		/// Handler for the slashed deposits.
		type SlashHandler: OnUnbalanced<NegativeImbalanceOf<Self>>;

		/// Handler for the rewards.
		type RewardHandler: OnUnbalanced<PositiveImbalanceOf<Self>>;

		/// Something that will provide the election data.
		type DataProvider: ElectionDataProvider<
			AccountId = Self::AccountId,
			BlockNumber = BlockNumberFor<Self>,
		>;

		/// Configuration for the fallback.
		type Fallback: InstantElectionProvider<
			AccountId = Self::AccountId,
			BlockNumber = BlockNumberFor<Self>,
			DataProvider = Self::DataProvider,
			MaxBackersPerWinner = Self::MaxBackersPerWinner,
			MaxWinnersPerPage = Self::MaxWinners,
		>;

		/// Configuration of the governance-only fallback.
		///
		/// As a side-note, it is recommend for test-nets to use `type ElectionProvider =
		/// BoundedExecution<_>` if the test-net is not expected to have thousands of nominators.
		type GovernanceFallback: InstantElectionProvider<
			AccountId = Self::AccountId,
			BlockNumber = BlockNumberFor<Self>,
			DataProvider = Self::DataProvider,
			MaxWinnersPerPage = Self::MaxWinners,
			MaxBackersPerWinner = Self::MaxBackersPerWinner,
		>;

		/// OCW election solution miner algorithm implementation.
		type Solver: NposSolver<AccountId = Self::AccountId>;

		/// Origin that can control this pallet. Note that any action taken by this origin (such)
		/// as providing an emergency solution is not checked. Thus, it must be a trusted origin.
		type ForceOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The configuration of benchmarking.
		type BenchmarkingConfig: BenchmarkingConfig;

		/// The weight of the pallet.
		type WeightInfo: WeightInfo;
	}

	// Expose miner configs over the metadata such that they can be re-implemented.
	#[pallet::extra_constants]
	impl<T: Config> Pallet<T> {
		#[pallet::constant_name(MinerMaxLength)]
		fn max_length() -> u32 {
			<T::MinerConfig as MinerConfig>::MaxLength::get()
		}

		#[pallet::constant_name(MinerMaxWeight)]
		fn max_weight() -> Weight {
			<T::MinerConfig as MinerConfig>::MaxWeight::get()
		}

		#[pallet::constant_name(MinerMaxVotesPerVoter)]
		fn max_votes_per_voter() -> u32 {
			<T::MinerConfig as MinerConfig>::MaxVotesPerVoter::get()
		}

		#[pallet::constant_name(MinerMaxWinners)]
		fn max_winners() -> u32 {
			<T::MinerConfig as MinerConfig>::MaxWinners::get()
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(now: BlockNumberFor<T>) -> Weight {
			let next_election = T::DataProvider::next_election_prediction(now).max(now);

			let signed_deadline = T::SignedPhase::get() + T::UnsignedPhase::get();
			let unsigned_deadline = T::UnsignedPhase::get();

			let remaining = next_election - now;
			let current_phase = CurrentPhase::<T>::get();

			log!(
				trace,
				"current phase {:?}, next election {:?}, queued? {:?}, metadata: {:?}",
				current_phase,
				next_election,
				QueuedSolution::<T>::get().map(|rs| (rs.supports.len(), rs.compute, rs.score)),
				SnapshotMetadata::<T>::get()
			);
			match current_phase {
				Phase::Off if remaining <= signed_deadline && remaining > unsigned_deadline => {
					// NOTE: if signed-phase length is zero, second part of the if-condition fails.
					match Self::create_snapshot() {
						Ok(_) => {
							Self::phase_transition(Phase::Signed);
							T::WeightInfo::on_initialize_open_signed()
						},
						Err(why) => {
							// Not much we can do about this at this point.
							log!(warn, "failed to open signed phase due to {:?}", why);
							T::WeightInfo::on_initialize_nothing()
						},
					}
				},
				Phase::Signed | Phase::Off
					if remaining <= unsigned_deadline && remaining > Zero::zero() =>
				{
					// our needs vary according to whether or not the unsigned phase follows a
					// signed phase
					let (need_snapshot, enabled) = if current_phase == Phase::Signed {
						// there was previously a signed phase: close the signed phase, no need for
						// snapshot.
						//
						// Notes:
						//
						//   - `Self::finalize_signed_phase()` also appears in `fn do_elect`. This
						//     is a guard against the case that `elect` is called prematurely. This
						//     adds a small amount of overhead, but that is unfortunately
						//     unavoidable.
						let _ = Self::finalize_signed_phase();
						// In the future we can consider disabling the unsigned phase if the signed
						// phase completes successfully, but for now we're enabling it
						// unconditionally as a defensive measure.
						(false, true)
					} else {
						// No signed phase: create a new snapshot, definitely `enable` the unsigned
						// phase.
						(true, true)
					};

					if need_snapshot {
						match Self::create_snapshot() {
							Ok(_) => {
								Self::phase_transition(Phase::Unsigned((enabled, now)));
								T::WeightInfo::on_initialize_open_unsigned()
							},
							Err(why) => {
								log!(warn, "failed to open unsigned phase due to {:?}", why);
								T::WeightInfo::on_initialize_nothing()
							},
						}
					} else {
						Self::phase_transition(Phase::Unsigned((enabled, now)));
						T::WeightInfo::on_initialize_open_unsigned()
					}
				},
				_ => T::WeightInfo::on_initialize_nothing(),
			}
		}

		fn offchain_worker(now: BlockNumberFor<T>) {
			use sp_runtime::offchain::storage_lock::{BlockAndTime, StorageLock};

			// Create a lock with the maximum deadline of number of blocks in the unsigned phase.
			// This should only come useful in an **abrupt** termination of execution, otherwise the
			// guard will be dropped upon successful execution.
			let mut lock =
				StorageLock::<BlockAndTime<frame_system::Pallet<T>>>::with_block_deadline(
					unsigned::OFFCHAIN_LOCK,
					T::UnsignedPhase::get().saturated_into(),
				);

			match lock.try_lock() {
				Ok(_guard) => {
					Self::do_synchronized_offchain_worker(now);
				},
				Err(deadline) => {
					log!(debug, "offchain worker lock not released, deadline is {:?}", deadline);
				},
			};
		}

		fn integrity_test() {
			use core::mem::size_of;
			// The index type of both voters and targets need to be smaller than that of usize (very
			// unlikely to be the case, but anyhow)..
			assert!(size_of::<SolutionVoterIndexOf<T::MinerConfig>>() <= size_of::<usize>());
			assert!(size_of::<SolutionTargetIndexOf<T::MinerConfig>>() <= size_of::<usize>());

			// ----------------------------
			// Based on the requirements of [`sp_npos_elections::Assignment::try_normalize`].
			let max_vote: usize = <SolutionOf<T::MinerConfig> as NposSolution>::LIMIT;

			// 2. Maximum sum of [SolutionAccuracy; 16] must fit into `UpperOf<OffchainAccuracy>`.
			let maximum_chain_accuracy: Vec<UpperOf<SolutionAccuracyOf<T>>> = (0..max_vote)
				.map(|_| {
					<UpperOf<SolutionAccuracyOf<T>>>::from(
						SolutionAccuracyOf::<T>::one().deconstruct(),
					)
				})
				.collect();
			let _: UpperOf<SolutionAccuracyOf<T>> = maximum_chain_accuracy
				.iter()
				.fold(Zero::zero(), |acc, x| acc.checked_add(x).unwrap());

			// We only accept data provider who's maximum votes per voter matches our
			// `T::Solution`'s `LIMIT`.
			//
			// NOTE that this pallet does not really need to enforce this in runtime. The
			// solution cannot represent any voters more than `LIMIT` anyhow.
			assert_eq!(
				<T::DataProvider as ElectionDataProvider>::MaxVotesPerVoter::get(),
				<SolutionOf<T::MinerConfig> as NposSolution>::LIMIT as u32,
			);

			// While it won't cause any failures, setting `SignedMaxRefunds` gt
			// `SignedMaxSubmissions` is a red flag that the developer does not understand how to
			// configure this pallet.
			assert!(T::SignedMaxSubmissions::get() >= T::SignedMaxRefunds::get());
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_n: BlockNumberFor<T>) -> Result<(), TryRuntimeError> {
			Self::do_try_state()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Submit a solution for the unsigned phase.
		///
		/// The dispatch origin fo this call must be __none__.
		///
		/// This submission is checked on the fly. Moreover, this unsigned solution is only
		/// validated when submitted to the pool from the **local** node. Effectively, this means
		/// that only active validators can submit this transaction when authoring a block (similar
		/// to an inherent).
		///
		/// To prevent any incorrect solution (and thus wasted time/weight), this transaction will
		/// panic if the solution submitted by the validator is invalid in any way, effectively
		/// putting their authoring reward at risk.
		///
		/// No deposit or reward is associated with this submission.
		#[pallet::call_index(0)]
		#[pallet::weight((
			T::WeightInfo::submit_unsigned(
				witness.voters,
				witness.targets,
				raw_solution.solution.voter_count() as u32,
				raw_solution.solution.unique_targets().len() as u32
			),
			DispatchClass::Operational,
		))]
		pub fn submit_unsigned(
			origin: OriginFor<T>,
			raw_solution: Box<RawSolution<SolutionOf<T::MinerConfig>>>,
			witness: SolutionOrSnapshotSize,
		) -> DispatchResult {
			ensure_none(origin)?;
			let error_message = "Invalid unsigned submission must produce invalid block and \
				 deprive validator from their authoring reward.";

			// Check score being an improvement, phase, and desired targets.
			Self::unsigned_pre_dispatch_checks(&raw_solution).expect(error_message);

			// Ensure witness was correct.
			let SolutionOrSnapshotSize { voters, targets } =
				SnapshotMetadata::<T>::get().expect(error_message);

			// NOTE: we are asserting, not `ensure`ing -- we want to panic here.
			assert!(voters as u32 == witness.voters, "{}", error_message);
			assert!(targets as u32 == witness.targets, "{}", error_message);

			let ready = Self::feasibility_check(*raw_solution, ElectionCompute::Unsigned)
				.expect(error_message);

			// Store the newly received solution.
			log!(debug, "queued unsigned solution with score {:?}", ready.score);
			let ejected_a_solution = QueuedSolution::<T>::exists();
			QueuedSolution::<T>::put(ready);
			Self::deposit_event(Event::SolutionStored {
				compute: ElectionCompute::Unsigned,
				origin: None,
				prev_ejected: ejected_a_solution,
			});

			Ok(())
		}

		/// Set a new value for `MinimumUntrustedScore`.
		///
		/// Dispatch origin must be aligned with `T::ForceOrigin`.
		///
		/// This check can be turned off by setting the value to `None`.
		#[pallet::call_index(1)]
		#[pallet::weight(T::DbWeight::get().writes(1))]
		pub fn set_minimum_untrusted_score(
			origin: OriginFor<T>,
			maybe_next_score: Option<ElectionScore>,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			MinimumUntrustedScore::<T>::set(maybe_next_score);
			Ok(())
		}

		/// Set a solution in the queue, to be handed out to the client of this pallet in the next
		/// call to `ElectionProvider::elect`.
		///
		/// This can only be set by `T::ForceOrigin`, and only when the phase is `Emergency`.
		///
		/// The solution is not checked for any feasibility and is assumed to be trustworthy, as any
		/// feasibility check itself can in principle cause the election process to fail (due to
		/// memory/weight constrains).
		#[pallet::call_index(2)]
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn set_emergency_election_result(
			origin: OriginFor<T>,
			supports: Supports<T::AccountId>,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			ensure!(CurrentPhase::<T>::get().is_emergency(), Error::<T>::CallNotAllowed);

			// bound supports with T::MaxWinners.
			let supports: BoundedSupportsOf<Self> =
				supports.try_into().map_err(|_| Error::<T>::TooManyWinners)?;

			// Note: we don't `rotate_round` at this point; the next call to
			// `ElectionProvider::elect` will succeed and take care of that.
			let solution = ReadySolution {
				supports,
				score: Default::default(),
				compute: ElectionCompute::Emergency,
			};

			Self::deposit_event(Event::SolutionStored {
				compute: ElectionCompute::Emergency,
				origin: None,
				prev_ejected: QueuedSolution::<T>::exists(),
			});

			QueuedSolution::<T>::put(solution);
			Ok(())
		}

		/// Submit a solution for the signed phase.
		///
		/// The dispatch origin fo this call must be __signed__.
		///
		/// The solution is potentially queued, based on the claimed score and processed at the end
		/// of the signed phase.
		///
		/// A deposit is reserved and recorded for the solution. Based on the outcome, the solution
		/// might be rewarded, slashed, or get all or a part of the deposit back.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::submit())]
		pub fn submit(
			origin: OriginFor<T>,
			raw_solution: Box<RawSolution<SolutionOf<T::MinerConfig>>>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			// ensure solution is timely.
			ensure!(CurrentPhase::<T>::get().is_signed(), Error::<T>::PreDispatchEarlySubmission);
			ensure!(raw_solution.round == Round::<T>::get(), Error::<T>::PreDispatchDifferentRound);

			// NOTE: this is the only case where having separate snapshot would have been better
			// because could do just decode_len. But we can create abstractions to do this.

			// build size. Note: this is not needed for weight calc, thus not input.
			// unlikely to ever return an error: if phase is signed, snapshot will exist.
			let size = SnapshotMetadata::<T>::get().ok_or(Error::<T>::MissingSnapshotMetadata)?;

			ensure!(
				Self::solution_weight_of(&raw_solution, size).all_lt(T::SignedMaxWeight::get()),
				Error::<T>::SignedTooMuchWeight,
			);

			// create the submission
			let deposit = Self::deposit_for(&raw_solution, size);
			let call_fee = {
				let call = Call::submit { raw_solution: raw_solution.clone() };
				T::EstimateCallFee::estimate_call_fee(&call, None::<Weight>.into())
			};

			let submission = SignedSubmission {
				who: who.clone(),
				deposit,
				raw_solution: *raw_solution,
				call_fee,
			};

			// insert the submission if the queue has space or it's better than the weakest
			// eject the weakest if the queue was full
			let mut signed_submissions = Self::signed_submissions();
			let maybe_removed = match signed_submissions.insert(submission) {
				// it's an error if we failed to insert a submission: this indicates the queue was
				// full but our solution had insufficient score to eject any solution
				signed::InsertResult::NotInserted => return Err(Error::<T>::SignedQueueFull.into()),
				signed::InsertResult::Inserted => None,
				signed::InsertResult::InsertedEjecting(weakest) => Some(weakest),
			};

			// collect deposit. Thereafter, the function cannot fail.
			T::Currency::reserve(&who, deposit).map_err(|_| Error::<T>::SignedCannotPayDeposit)?;

			let ejected_a_solution = maybe_removed.is_some();
			// if we had to remove the weakest solution, unreserve its deposit
			if let Some(removed) = maybe_removed {
				let _remainder = T::Currency::unreserve(&removed.who, removed.deposit);
				debug_assert!(_remainder.is_zero());
			}

			signed_submissions.put();
			Self::deposit_event(Event::SolutionStored {
				compute: ElectionCompute::Signed,
				origin: Some(who),
				prev_ejected: ejected_a_solution,
			});
			Ok(())
		}

		/// Trigger the governance fallback.
		///
		/// This can only be called when [`Phase::Emergency`] is enabled, as an alternative to
		/// calling [`Call::set_emergency_election_result`].
		#[pallet::call_index(4)]
		#[pallet::weight(T::DbWeight::get().reads_writes(1, 1))]
		pub fn governance_fallback(origin: OriginFor<T>) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			ensure!(CurrentPhase::<T>::get().is_emergency(), Error::<T>::CallNotAllowed);

			let RoundSnapshot { voters, targets } =
				Snapshot::<T>::get().ok_or(Error::<T>::MissingSnapshotMetadata)?;
			let desired_targets =
				DesiredTargets::<T>::get().ok_or(Error::<T>::MissingSnapshotMetadata)?;

			let supports = T::GovernanceFallback::instant_elect(voters, targets, desired_targets)
				.map_err(|e| {
				log!(error, "GovernanceFallback failed: {:?}", e);
				Error::<T>::FallbackFailed
			})?;

			let solution = ReadySolution {
				supports,
				score: Default::default(),
				compute: ElectionCompute::Fallback,
			};

			Self::deposit_event(Event::SolutionStored {
				compute: ElectionCompute::Fallback,
				origin: None,
				prev_ejected: QueuedSolution::<T>::exists(),
			});

			QueuedSolution::<T>::put(solution);
			Ok(())
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A solution was stored with the given compute.
		///
		/// The `origin` indicates the origin of the solution. If `origin` is `Some(AccountId)`,
		/// the stored solution was submitted in the signed phase by a miner with the `AccountId`.
		/// Otherwise, the solution was stored either during the unsigned phase or by
		/// `T::ForceOrigin`. The `bool` is `true` when a previous solution was ejected to make
		/// room for this one.
		SolutionStored {
			compute: ElectionCompute,
			origin: Option<T::AccountId>,
			prev_ejected: bool,
		},
		/// The election has been finalized, with the given computation and score.
		ElectionFinalized { compute: ElectionCompute, score: ElectionScore },
		/// An election failed.
		///
		/// Not much can be said about which computes failed in the process.
		ElectionFailed,
		/// An account has been rewarded for their signed submission being finalized.
		Rewarded { account: <T as frame_system::Config>::AccountId, value: BalanceOf<T> },
		/// An account has been slashed for submitting an invalid signed submission.
		Slashed { account: <T as frame_system::Config>::AccountId, value: BalanceOf<T> },
		/// There was a phase transition in a given round.
		PhaseTransitioned {
			from: Phase<BlockNumberFor<T>>,
			to: Phase<BlockNumberFor<T>>,
			round: u32,
		},
	}

	/// Error of the pallet that can be returned in response to dispatches.
	#[pallet::error]
	pub enum Error<T> {
		/// Submission was too early.
		PreDispatchEarlySubmission,
		/// Wrong number of winners presented.
		PreDispatchWrongWinnerCount,
		/// Submission was too weak, score-wise.
		PreDispatchWeakSubmission,
		/// The queue was full, and the solution was not better than any of the existing ones.
		SignedQueueFull,
		/// The origin failed to pay the deposit.
		SignedCannotPayDeposit,
		/// Witness data to dispatchable is invalid.
		SignedInvalidWitness,
		/// The signed submission consumes too much weight
		SignedTooMuchWeight,
		/// OCW submitted solution for wrong round
		OcwCallWrongEra,
		/// Snapshot metadata should exist but didn't.
		MissingSnapshotMetadata,
		/// `Self::insert_submission` returned an invalid index.
		InvalidSubmissionIndex,
		/// The call is not allowed at this point.
		CallNotAllowed,
		/// The fallback failed
		FallbackFailed,
		/// Some bound not met
		BoundNotMet,
		/// Submitted solution has too many winners
		TooManyWinners,
		/// Submission was prepared for a different round.
		PreDispatchDifferentRound,
	}

	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;
		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			if let Call::submit_unsigned { raw_solution, .. } = call {
				// Discard solution not coming from the local OCW.
				match source {
					TransactionSource::Local | TransactionSource::InBlock => { /* allowed */ },
					_ => return InvalidTransaction::Call.into(),
				}

				Self::unsigned_pre_dispatch_checks(raw_solution)
					.inspect_err(|err| {
						log!(debug, "unsigned transaction validation failed due to {:?}", err);
					})
					.map_err(dispatch_error_to_invalid)?;

				ValidTransaction::with_tag_prefix("OffchainElection")
					// The higher the score.minimal_stake, the better a solution is.
					.priority(
						T::MinerTxPriority::get()
							.saturating_add(raw_solution.score.minimal_stake.saturated_into()),
					)
					// Used to deduplicate unsigned solutions: each validator should produce one
					// solution per round at most, and solutions are not propagate.
					.and_provides(raw_solution.round)
					// Transaction should stay in the pool for the duration of the unsigned phase.
					.longevity(T::UnsignedPhase::get().saturated_into::<u64>())
					// We don't propagate this. This can never be validated at a remote node.
					.propagate(false)
					.build()
			} else {
				InvalidTransaction::Call.into()
			}
		}

		fn pre_dispatch(call: &Self::Call) -> Result<(), TransactionValidityError> {
			if let Call::submit_unsigned { raw_solution, .. } = call {
				Self::unsigned_pre_dispatch_checks(raw_solution)
					.map_err(dispatch_error_to_invalid)
					.map_err(Into::into)
			} else {
				Err(InvalidTransaction::Call.into())
			}
		}
	}

	#[pallet::type_value]
	pub fn DefaultForRound() -> u32 {
		1
	}

	/// Internal counter for the number of rounds.
	///
	/// This is useful for de-duplication of transactions submitted to the pool, and general
	/// diagnostics of the pallet.
	///
	/// This is merely incremented once per every time that an upstream `elect` is called.
	#[pallet::storage]
	pub type Round<T: Config> = StorageValue<_, u32, ValueQuery, DefaultForRound>;

	/// Current phase.
	#[pallet::storage]
	pub type CurrentPhase<T: Config> = StorageValue<_, Phase<BlockNumberFor<T>>, ValueQuery>;

	/// Current best solution, signed or unsigned, queued to be returned upon `elect`.
	///
	/// Always sorted by score.
	#[pallet::storage]
	pub type QueuedSolution<T: Config> = StorageValue<_, ReadySolutionOf<T::MinerConfig>>;

	/// Snapshot data of the round.
	///
	/// This is created at the beginning of the signed phase and cleared upon calling `elect`.
	/// Note: This storage type must only be mutated through [`SnapshotWrapper`].
	#[pallet::storage]
	pub type Snapshot<T: Config> = StorageValue<_, RoundSnapshot<T::AccountId, VoterOf<T>>>;

	/// Desired number of targets to elect for this round.
	///
	/// Only exists when [`Snapshot`] is present.
	/// Note: This storage type must only be mutated through [`SnapshotWrapper`].
	#[pallet::storage]
	pub type DesiredTargets<T> = StorageValue<_, u32>;

	/// The metadata of the [`RoundSnapshot`]
	///
	/// Only exists when [`Snapshot`] is present.
	/// Note: This storage type must only be mutated through [`SnapshotWrapper`].
	#[pallet::storage]
	pub type SnapshotMetadata<T: Config> = StorageValue<_, SolutionOrSnapshotSize>;

	// The following storage items collectively comprise `SignedSubmissions<T>`, and should never be
	// accessed independently. Instead, get `Self::signed_submissions()`, modify it as desired, and
	// then do `signed_submissions.put()` when you're done with it.

	/// The next index to be assigned to an incoming signed submission.
	///
	/// Every accepted submission is assigned a unique index; that index is bound to that particular
	/// submission for the duration of the election. On election finalization, the next index is
	/// reset to 0.
	///
	/// We can't just use `SignedSubmissionIndices.len()`, because that's a bounded set; past its
	/// capacity, it will simply saturate. We can't just iterate over `SignedSubmissionsMap`,
	/// because iteration is slow. Instead, we store the value here.
	#[pallet::storage]
	pub type SignedSubmissionNextIndex<T: Config> = StorageValue<_, u32, ValueQuery>;

	/// A sorted, bounded vector of `(score, block_number, index)`, where each `index` points to a
	/// value in `SignedSubmissions`.
	///
	/// We never need to process more than a single signed submission at a time. Signed submissions
	/// can be quite large, so we're willing to pay the cost of multiple database accesses to access
	/// them one at a time instead of reading and decoding all of them at once.
	#[pallet::storage]
	pub type SignedSubmissionIndices<T: Config> =
		StorageValue<_, SubmissionIndicesOf<T>, ValueQuery>;

	/// Unchecked, signed solutions.
	///
	/// Together with `SubmissionIndices`, this stores a bounded set of `SignedSubmissions` while
	/// allowing us to keep only a single one in memory at a time.
	///
	/// Twox note: the key of the map is an auto-incrementing index which users cannot inspect or
	/// affect; we shouldn't need a cryptographically secure hasher.
	#[pallet::storage]
	pub type SignedSubmissionsMap<T: Config> =
		StorageMap<_, Twox64Concat, u32, SignedSubmissionOf<T>, OptionQuery>;

	// `SignedSubmissions` items end here.

	/// The minimum score that each 'untrusted' solution must attain in order to be considered
	/// feasible.
	///
	/// Can be set via `set_minimum_untrusted_score`.
	#[pallet::storage]
	pub type MinimumUntrustedScore<T: Config> = StorageValue<_, ElectionScore>;

	/// The in-code storage version.
	///
	/// v1: https://github.com/paritytech/substrate/pull/12237/
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

	#[pallet::pallet]
	#[pallet::without_storage_info]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);
}

/// This wrapper is created for handling the synchronization of [`Snapshot`], [`SnapshotMetadata`]
/// and [`DesiredTargets`] storage items.
pub struct SnapshotWrapper<T>(core::marker::PhantomData<T>);

impl<T: Config> SnapshotWrapper<T> {
	/// Kill all snapshot related storage items at the same time.
	pub fn kill() {
		Snapshot::<T>::kill();
		SnapshotMetadata::<T>::kill();
		DesiredTargets::<T>::kill();
	}
	/// Set all snapshot related storage items at the same time.
	pub fn set(metadata: SolutionOrSnapshotSize, desired_targets: u32, buffer: &[u8]) {
		SnapshotMetadata::<T>::put(metadata);
		DesiredTargets::<T>::put(desired_targets);
		sp_io::storage::set(&Snapshot::<T>::hashed_key(), &buffer);
	}

	/// Check if all of the storage items exist at the same time or all of the storage items do not
	/// exist.
	#[cfg(feature = "try-runtime")]
	pub fn is_consistent() -> bool {
		let snapshots = [
			Snapshot::<T>::exists(),
			SnapshotMetadata::<T>::exists(),
			DesiredTargets::<T>::exists(),
		];

		// All should either exist or not exist
		snapshots.iter().skip(1).all(|v| snapshots[0] == *v)
	}
}

impl<T: Config> Pallet<T> {
	/// Internal counter for the number of rounds.
	///
	/// This is useful for de-duplication of transactions submitted to the pool, and general
	/// diagnostics of the pallet.
	///
	/// This is merely incremented once per every time that an upstream `elect` is called.
	pub fn round() -> u32 {
		Round::<T>::get()
	}

	/// Current phase.
	pub fn current_phase() -> Phase<BlockNumberFor<T>> {
		CurrentPhase::<T>::get()
	}

	/// Current best solution, signed or unsigned, queued to be returned upon `elect`.
	///
	/// Always sorted by score.
	pub fn queued_solution() -> Option<ReadySolutionOf<T::MinerConfig>> {
		QueuedSolution::<T>::get()
	}

	/// Snapshot data of the round.
	///
	/// This is created at the beginning of the signed phase and cleared upon calling `elect`.
	/// Note: This storage type must only be mutated through [`SnapshotWrapper`].
	pub fn snapshot() -> Option<RoundSnapshot<T::AccountId, VoterOf<T>>> {
		Snapshot::<T>::get()
	}

	/// Desired number of targets to elect for this round.
	///
	/// Only exists when [`Snapshot`] is present.
	/// Note: This storage type must only be mutated through [`SnapshotWrapper`].
	pub fn desired_targets() -> Option<u32> {
		DesiredTargets::<T>::get()
	}

	/// The metadata of the [`RoundSnapshot`]
	///
	/// Only exists when [`Snapshot`] is present.
	/// Note: This storage type must only be mutated through [`SnapshotWrapper`].
	pub fn snapshot_metadata() -> Option<SolutionOrSnapshotSize> {
		SnapshotMetadata::<T>::get()
	}

	/// The minimum score that each 'untrusted' solution must attain in order to be considered
	/// feasible.
	///
	/// Can be set via `set_minimum_untrusted_score`.
	pub fn minimum_untrusted_score() -> Option<ElectionScore> {
		MinimumUntrustedScore::<T>::get()
	}

	/// Internal logic of the offchain worker, to be executed only when the offchain lock is
	/// acquired with success.
	fn do_synchronized_offchain_worker(now: BlockNumberFor<T>) {
		let current_phase = CurrentPhase::<T>::get();
		log!(trace, "lock for offchain worker acquired. Phase = {:?}", current_phase);
		match current_phase {
			Phase::Unsigned((true, opened)) if opened == now => {
				// Mine a new solution, cache it, and attempt to submit it
				let initial_output = Self::ensure_offchain_repeat_frequency(now).and_then(|_| {
					// This is executed at the beginning of each round. Any cache is now invalid.
					// Clear it.
					unsigned::kill_ocw_solution::<T>();
					Self::mine_check_save_submit()
				});
				log!(debug, "initial offchain thread output: {:?}", initial_output);
			},
			Phase::Unsigned((true, opened)) if opened < now => {
				// Try and resubmit the cached solution, and recompute ONLY if it is not
				// feasible.
				let resubmit_output = Self::ensure_offchain_repeat_frequency(now)
					.and_then(|_| Self::restore_or_compute_then_maybe_submit());
				log!(debug, "resubmit offchain thread output: {:?}", resubmit_output);
			},
			_ => {},
		}
	}

	/// Phase transition helper.
	pub(crate) fn phase_transition(to: Phase<BlockNumberFor<T>>) {
		log!(info, "Starting phase {:?}, round {}.", to, Round::<T>::get());
		Self::deposit_event(Event::PhaseTransitioned {
			from: CurrentPhase::<T>::get(),
			to,
			round: Round::<T>::get(),
		});
		CurrentPhase::<T>::put(to);
	}

	/// Parts of [`create_snapshot`] that happen inside of this pallet.
	///
	/// Extracted for easier weight calculation.
	fn create_snapshot_internal(
		targets: Vec<T::AccountId>,
		voters: Vec<VoterOf<T>>,
		desired_targets: u32,
	) {
		let metadata =
			SolutionOrSnapshotSize { voters: voters.len() as u32, targets: targets.len() as u32 };
		log!(info, "creating a snapshot with metadata {:?}", metadata);

		// instead of using storage APIs, we do a manual encoding into a fixed-size buffer.
		// `encoded_size` encodes it without storing it anywhere, this should not cause any
		// allocation.
		let snapshot = RoundSnapshot::<T::AccountId, VoterOf<T>> { voters, targets };
		let size = snapshot.encoded_size();
		log!(debug, "snapshot pre-calculated size {:?}", size);
		let mut buffer = Vec::with_capacity(size);
		snapshot.encode_to(&mut buffer);

		// do some checks.
		debug_assert_eq!(buffer, snapshot.encode());
		// buffer should have not re-allocated since.
		debug_assert!(buffer.len() == size && size == buffer.capacity());

		SnapshotWrapper::<T>::set(metadata, desired_targets, &buffer);
	}

	/// Parts of [`create_snapshot`] that happen outside of this pallet.
	///
	/// Extracted for easier weight calculation.
	///
	/// Note: this pallet only supports one page of voter and target snapshots.
	fn create_snapshot_external(
	) -> Result<(Vec<T::AccountId>, Vec<VoterOf<T>>, u32), ElectionError<T>> {
		let election_bounds = T::ElectionBounds::get();
		let targets = T::DataProvider::electable_targets_stateless(election_bounds.targets)
			.and_then(|t| {
				election_bounds.ensure_targets_limits(
					CountBound(t.len() as u32),
					SizeBound(t.encoded_size() as u32),
				)?;
				Ok(t)
			})
			.map_err(ElectionError::DataProvider)?;

		let voters = T::DataProvider::electing_voters_stateless(election_bounds.voters)
			.and_then(|v| {
				election_bounds.ensure_voters_limits(
					CountBound(v.len() as u32),
					SizeBound(v.encoded_size() as u32),
				)?;
				Ok(v)
			})
			.map_err(ElectionError::DataProvider)?;

		let mut desired_targets = <Pallet<T> as ElectionProvider>::desired_targets_checked()
			.map_err(|e| ElectionError::DataProvider(e))?;

		// If `desired_targets` > `targets.len()`, cap `desired_targets` to that level and emit a
		// warning
		let max_desired_targets: u32 = targets.len() as u32;
		if desired_targets > max_desired_targets {
			log!(
				warn,
				"desired_targets: {} > targets.len(): {}, capping desired_targets",
				desired_targets,
				max_desired_targets
			);
			desired_targets = max_desired_targets;
		}

		Ok((targets, voters, desired_targets))
	}

	/// Creates the snapshot. Writes new data to:
	///
	/// 1. [`SnapshotMetadata`]
	/// 2. [`RoundSnapshot`]
	/// 3. [`DesiredTargets`]
	///
	/// Returns `Ok(())` if operation is okay.
	///
	/// This is a *self-weighing* function, it will register its own extra weight as
	/// [`DispatchClass::Mandatory`] with the system pallet.
	pub fn create_snapshot() -> Result<(), ElectionError<T>> {
		// this is self-weighing itself..
		let (targets, voters, desired_targets) = Self::create_snapshot_external()?;

		// ..therefore we only measure the weight of this and add it.
		let internal_weight =
			T::WeightInfo::create_snapshot_internal(voters.len() as u32, targets.len() as u32);
		Self::create_snapshot_internal(targets, voters, desired_targets);
		Self::register_weight(internal_weight);
		Ok(())
	}

	/// Register some amount of weight directly with the system pallet.
	///
	/// This is always mandatory weight.
	fn register_weight(weight: Weight) {
		frame_system::Pallet::<T>::register_extra_weight_unchecked(
			weight,
			DispatchClass::Mandatory,
		);
	}

	/// Checks the feasibility of a solution.
	pub fn feasibility_check(
		raw_solution: RawSolution<SolutionOf<T::MinerConfig>>,
		compute: ElectionCompute,
	) -> Result<ReadySolutionOf<T::MinerConfig>, FeasibilityError> {
		let desired_targets =
			DesiredTargets::<T>::get().ok_or(FeasibilityError::SnapshotUnavailable)?;

		let snapshot = Snapshot::<T>::get().ok_or(FeasibilityError::SnapshotUnavailable)?;
		let round = Round::<T>::get();
		let minimum_untrusted_score = MinimumUntrustedScore::<T>::get();

		Miner::<T::MinerConfig>::feasibility_check(
			raw_solution,
			compute,
			desired_targets,
			snapshot,
			round,
			minimum_untrusted_score,
		)
	}

	/// Perform the tasks to be done after a new `elect` has been triggered:
	///
	/// 1. Increment round.
	/// 2. Change phase to [`Phase::Off`]
	/// 3. Clear all snapshot data.
	fn rotate_round() {
		// Inc round.
		Round::<T>::mutate(|r| *r += 1);

		// Phase is off now.
		Self::phase_transition(Phase::Off);

		// Kill snapshot and relevant metadata (everything created by [`SnapshotMetadata::set`]).
		SnapshotWrapper::<T>::kill();
	}

	fn do_elect() -> Result<BoundedSupportsOf<Self>, ElectionError<T>> {
		// We have to unconditionally try finalizing the signed phase here. There are only two
		// possibilities:
		//
		// - signed phase was open, in which case this is essential for correct functioning of the
		//   system
		// - signed phase was complete or not started, in which case finalization is idempotent and
		//   inexpensive (1 read of an empty vector).
		let _ = Self::finalize_signed_phase();

		QueuedSolution::<T>::take()
			.ok_or(ElectionError::<T>::NothingQueued)
			.or_else(|_| {
				log!(warn, "No solution queued, falling back to instant fallback.",);

				#[cfg(feature = "runtime-benchmarks")]
				Self::asap();

				let (voters, targets, desired_targets) = if T::Fallback::bother() {
					let RoundSnapshot { voters, targets } = Snapshot::<T>::get().ok_or(
						ElectionError::<T>::Feasibility(FeasibilityError::SnapshotUnavailable),
					)?;
					let desired_targets = DesiredTargets::<T>::get().ok_or(
						ElectionError::<T>::Feasibility(FeasibilityError::SnapshotUnavailable),
					)?;
					(voters, targets, desired_targets)
				} else {
					(Default::default(), Default::default(), Default::default())
				};
				T::Fallback::instant_elect(voters, targets, desired_targets)
					.map_err(|fe| ElectionError::Fallback(fe))
					.and_then(|supports| {
						Ok(ReadySolution {
							supports,
							score: Default::default(),
							compute: ElectionCompute::Fallback,
						})
					})
			})
			.map(|ReadySolution { compute, score, supports }| {
				Self::deposit_event(Event::ElectionFinalized { compute, score });
				log!(info, "Finalized election round with compute {:?}.", compute);
				supports
			})
			.map_err(|err| {
				Self::deposit_event(Event::ElectionFailed);
				log!(warn, "Failed to finalize election round. reason {:?}", err);
				err
			})
	}

	/// record the weight of the given `supports`.
	fn weigh_supports(supports: &BoundedSupportsOf<Self>) {
		let active_voters = supports
			.iter()
			.map(|(_, x)| x)
			.fold(Zero::zero(), |acc, next| acc + next.voters.len() as u32);
		let desired_targets = supports.len() as u32;
		Self::register_weight(T::WeightInfo::elect_queued(active_voters, desired_targets));
	}
}

#[cfg(feature = "try-runtime")]
impl<T: Config> Pallet<T> {
	fn do_try_state() -> Result<(), TryRuntimeError> {
		Self::try_state_snapshot()?;
		Self::try_state_signed_submissions_map()?;
		Self::try_state_phase_off()
	}

	// [`Snapshot`] state check. Invariants:
	// - [`DesiredTargets`] exists if and only if [`Snapshot`] is present.
	// - [`SnapshotMetadata`] exist if and only if [`Snapshot`] is present.
	fn try_state_snapshot() -> Result<(), TryRuntimeError> {
		if SnapshotWrapper::<T>::is_consistent() {
			Ok(())
		} else {
			Err("If snapshot exists, metadata and desired targets should be set too. Otherwise, none should be set.".into())
		}
	}

	// [`SignedSubmissionsMap`] state check. Invariants:
	// - All [`SignedSubmissionIndices`] are present in [`SignedSubmissionsMap`], and no more;
	// - [`SignedSubmissionNextIndex`] is not present in [`SignedSubmissionsMap`];
	// - [`SignedSubmissionIndices`] is sorted by election score.
	fn try_state_signed_submissions_map() -> Result<(), TryRuntimeError> {
		let mut last_score: ElectionScore = Default::default();
		let indices = SignedSubmissionIndices::<T>::get();

		for (i, indice) in indices.iter().enumerate() {
			let submission = SignedSubmissionsMap::<T>::get(indice.2);
			if submission.is_none() {
				return Err(
					"All signed submissions indices must be part of the submissions map".into()
				)
			}

			if i == 0 {
				last_score = indice.0
			} else {
				if last_score.strict_threshold_better(indice.0, Perbill::zero()) {
					return Err(
						"Signed submission indices vector must be ordered by election score".into()
					)
				}
				last_score = indice.0;
			}
		}

		if SignedSubmissionsMap::<T>::iter().nth(indices.len()).is_some() {
			return Err(
				"Signed submissions map length should be the same as the indices vec length".into()
			)
		}

		match SignedSubmissionNextIndex::<T>::get() {
			0 => Ok(()),
			next =>
				if SignedSubmissionsMap::<T>::get(next).is_some() {
					return Err(
						"The next submissions index should not be in the submissions maps already"
							.into(),
					)
				} else {
					Ok(())
				},
		}
	}

	// [`Phase::Off`] state check. Invariants:
	// - If phase is `Phase::Off`, [`Snapshot`] must be none.
	fn try_state_phase_off() -> Result<(), TryRuntimeError> {
		match CurrentPhase::<T>::get().is_off() {
			false => Ok(()),
			true =>
				if Snapshot::<T>::get().is_some() {
					Err("Snapshot must be none when in Phase::Off".into())
				} else {
					Ok(())
				},
		}
	}
}

impl<T: Config> ElectionProvider for Pallet<T> {
	type AccountId = T::AccountId;
	type BlockNumber = BlockNumberFor<T>;
	type Error = ElectionError<T>;
	type MaxWinnersPerPage = T::MaxWinners;
	type MaxBackersPerWinner = T::MaxBackersPerWinner;
	type MaxBackersPerWinnerFinal = T::MaxBackersPerWinner;
	type Pages = sp_core::ConstU32<1>;
	type DataProvider = T::DataProvider;

	fn elect(page: PageIndex) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		// Note: this pallet **MUST** only by used in the single-page mode.
		ensure!(page == SINGLE_PAGE, ElectionError::<T>::MultiPageNotSupported);

		let res = match Self::do_elect() {
			Ok(bounded_supports) => {
				// All went okay, record the weight, put sign to be Off, clean snapshot, etc.
				Self::weigh_supports(&bounded_supports);
				Self::rotate_round();
				Ok(bounded_supports)
			},
			Err(why) => {
				log!(error, "Entering emergency mode: {:?}", why);
				Self::phase_transition(Phase::Emergency);
				Err(why)
			},
		};

		log!(info, "ElectionProvider::elect({}) => {:?}", page, res.as_ref().map(|s| s.len()));
		res
	}

	fn duration() -> Self::BlockNumber {
		let signed: BlockNumberFor<T> = T::SignedPhase::get().saturated_into();
		let unsigned: BlockNumberFor<T> = T::UnsignedPhase::get().saturated_into();
		signed + unsigned
	}

	fn start() -> Result<(), Self::Error> {
		log!(
			warn,
			"we received signal, but this pallet works in the basis of legacy pull based election"
		);
		Ok(())
	}

	fn status() -> Result<bool, ()> {
		let has_queued = QueuedSolution::<T>::exists();
		let phase = CurrentPhase::<T>::get();
		match (phase, has_queued) {
			(Phase::Unsigned(_), true) => Ok(true),
			(Phase::Off, _) => Err(()),
			_ => Ok(false),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn asap() {
		// prepare our snapshot so we can "hopefully" run a fallback.
		if !Snapshot::<T>::exists() {
			Self::create_snapshot()
				.inspect_err(|e| {
					crate::log!(error, "failed to create snapshot while asap-preparing: {:?}", e)
				})
				.unwrap()
		}
	}
}

/// convert a DispatchError to a custom InvalidTransaction with the inner code being the error
/// number.
pub fn dispatch_error_to_invalid(error: DispatchError) -> InvalidTransaction {
	let error_number = match error {
		DispatchError::Module(ModuleError { error, .. }) => error[0],
		_ => 0,
	};
	InvalidTransaction::Custom(error_number)
}

#[cfg(test)]
mod feasibility_check {
	//! All of the tests here should be dedicated to only testing the feasibility check and nothing
	//! more. The best way to audit and review these tests is to try and come up with a solution
	//! that is invalid, but gets through the system as valid.

	use super::*;
	use crate::mock::{
		raw_solution, roll_to, EpochLength, ExtBuilder, MultiPhase, Runtime, SignedPhase,
		TargetIndex, UnsignedPhase, VoterIndex,
	};
	use frame_support::{assert_noop, assert_ok};

	const COMPUTE: ElectionCompute = ElectionCompute::OnChain;

	#[test]
	fn snapshot_is_there() {
		ExtBuilder::default().build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());
			let solution = raw_solution();

			// kill `Snapshot`, `SnapshotMetadata` and `DesiredTargets` for the storage state to
			// be consistent, by using the `SnapshotWrapper` for the try_state checks to pass.
			SnapshotWrapper::<Runtime>::kill();

			assert_noop!(
				MultiPhase::feasibility_check(solution, COMPUTE),
				FeasibilityError::SnapshotUnavailable
			);
		})
	}

	#[test]
	fn round() {
		ExtBuilder::default().build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let mut solution = raw_solution();
			solution.round += 1;
			assert_noop!(
				MultiPhase::feasibility_check(solution, COMPUTE),
				FeasibilityError::InvalidRound
			);
		})
	}

	#[test]
	fn desired_targets_gets_capped() {
		ExtBuilder::default().desired_targets(8).build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let raw = raw_solution();

			assert_eq!(raw.solution.unique_targets().len(), 4);
			// desired_targets is capped to the number of targets which is 4
			assert_eq!(DesiredTargets::<Runtime>::get().unwrap(), 4);

			// It should succeed
			assert_ok!(MultiPhase::feasibility_check(raw, COMPUTE));
		})
	}

	#[test]
	fn less_than_desired_targets_fails() {
		ExtBuilder::default().desired_targets(8).build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let mut raw = raw_solution();

			assert_eq!(raw.solution.unique_targets().len(), 4);
			// desired_targets is capped to the number of targets which is 4
			assert_eq!(DesiredTargets::<Runtime>::get().unwrap(), 4);

			// Force the number of winners to be bigger to fail
			raw.solution.votes1[0].1 = 4;

			// It should succeed
			assert_noop!(
				MultiPhase::feasibility_check(raw, COMPUTE),
				FeasibilityError::WrongWinnerCount,
			);
		})
	}

	#[test]
	fn winner_indices() {
		ExtBuilder::default().desired_targets(2).build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let mut raw = raw_solution();
			assert_eq!(Snapshot::<Runtime>::get().unwrap().targets.len(), 4);
			// ----------------------------------------------------^^ valid range is [0..3].

			// Swap all votes from 3 to 4. This will ensure that the number of unique winners will
			// still be 4, but one of the indices will be gibberish. Requirement is to make sure 3 a
			// winner, which we don't do here.
			raw.solution
				.votes1
				.iter_mut()
				.filter(|(_, t)| *t == TargetIndex::from(3u16))
				.for_each(|(_, t)| *t += 1);
			raw.solution.votes2.iter_mut().for_each(|(_, [(t0, _)], t1)| {
				if *t0 == TargetIndex::from(3u16) {
					*t0 += 1
				};
				if *t1 == TargetIndex::from(3u16) {
					*t1 += 1
				};
			});
			assert_noop!(
				MultiPhase::feasibility_check(raw, COMPUTE),
				FeasibilityError::NposElection(sp_npos_elections::Error::SolutionInvalidIndex)
			);
		})
	}

	#[test]
	fn voter_indices() {
		// Should be caught in `solution.into_assignment`.
		ExtBuilder::default().desired_targets(2).build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let mut solution = raw_solution();
			assert_eq!(Snapshot::<Runtime>::get().unwrap().voters.len(), 8);
			// ----------------------------------------------------^^ valid range is [0..7].

			// Check that there is an index 7 in votes1, and flip to 8.
			assert!(
				solution
					.solution
					.votes1
					.iter_mut()
					.filter(|(v, _)| *v == VoterIndex::from(7u32))
					.map(|(v, _)| *v = 8)
					.count() > 0
			);
			assert_noop!(
				MultiPhase::feasibility_check(solution, COMPUTE),
				FeasibilityError::NposElection(sp_npos_elections::Error::SolutionInvalidIndex),
			);
		})
	}

	#[test]
	fn voter_votes() {
		ExtBuilder::default().desired_targets(2).build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let mut solution = raw_solution();
			assert_eq!(Snapshot::<Runtime>::get().unwrap().voters.len(), 8);
			// ----------------------------------------------------^^ valid range is [0..7].

			// First, check that voter at index 7 (40) actually voted for 3 (40) -- this is self
			// vote. Then, change the vote to 2 (30).
			assert_eq!(
				solution
					.solution
					.votes1
					.iter_mut()
					.filter(|(v, t)| *v == 7 && *t == 3)
					.map(|(_, t)| *t = 2)
					.count(),
				1,
			);
			assert_noop!(
				MultiPhase::feasibility_check(solution, COMPUTE),
				FeasibilityError::InvalidVote,
			);
		})
	}

	#[test]
	fn score() {
		ExtBuilder::default().desired_targets(2).build_and_execute(|| {
			roll_to(<EpochLength>::get() - <SignedPhase>::get() - <UnsignedPhase>::get());
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let mut solution = raw_solution();
			assert_eq!(Snapshot::<Runtime>::get().unwrap().voters.len(), 8);

			// Simply faff with the score.
			solution.score.minimal_stake += 1;

			assert_noop!(
				MultiPhase::feasibility_check(solution, COMPUTE),
				FeasibilityError::InvalidScore,
			);
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		mock::{
			multi_phase_events, raw_solution, roll_to, roll_to_signed, roll_to_unsigned,
			ElectionsBounds, ExtBuilder, MockWeightInfo, MockedWeightInfo, MultiPhase, Runtime,
			RuntimeOrigin, SignedMaxSubmissions, System, Voters,
		},
		Phase,
	};
	use frame_election_provider_support::bounds::ElectionBoundsBuilder;
	use frame_support::{assert_noop, assert_ok};
	use sp_npos_elections::{BalancingConfig, Support};

	#[test]
	fn phase_rotation_works() {
		ExtBuilder::default().build_and_execute(|| {
			// 0 ------- 15 ------- 25 ------- 30 ------- ------- 45 ------- 55 ------- 60
			//           |           |          |                 |           |          |
			//         Signed      Unsigned   Elect             Signed     Unsigned    Elect

			assert_eq!(System::block_number(), 0);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Off);
			assert_eq!(Round::<Runtime>::get(), 1);

			roll_to(4);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Off);
			assert!(Snapshot::<Runtime>::get().is_none());
			assert_eq!(Round::<Runtime>::get(), 1);

			roll_to_signed();
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Signed);
			assert_eq!(
				multi_phase_events(),
				vec![Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 }]
			);
			assert!(Snapshot::<Runtime>::get().is_some());
			assert_eq!(Round::<Runtime>::get(), 1);

			roll_to(24);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Signed);
			assert!(Snapshot::<Runtime>::get().is_some());
			assert_eq!(Round::<Runtime>::get(), 1);

			roll_to_unsigned();
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Unsigned((true, 25)));
			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 25)),
						round: 1
					},
				],
			);
			assert!(Snapshot::<Runtime>::get().is_some());

			roll_to(29);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Unsigned((true, 25)));
			assert!(Snapshot::<Runtime>::get().is_some());

			roll_to(30);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Unsigned((true, 25)));
			assert!(Snapshot::<Runtime>::get().is_some());

			// We close when upstream tells us to elect.
			roll_to(32);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Unsigned((true, 25)));
			assert!(Snapshot::<Runtime>::get().is_some());

			assert_ok!(MultiPhase::elect(SINGLE_PAGE));

			assert!(CurrentPhase::<Runtime>::get().is_off());
			assert!(Snapshot::<Runtime>::get().is_none());
			assert_eq!(Round::<Runtime>::get(), 2);

			roll_to(44);
			assert!(CurrentPhase::<Runtime>::get().is_off());

			roll_to_signed();
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			roll_to(55);
			assert!(CurrentPhase::<Runtime>::get().is_unsigned_open_at(55));

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 25)),
						round: 1
					},
					Event::ElectionFinalized {
						compute: ElectionCompute::Fallback,
						score: ElectionScore {
							minimal_stake: 0,
							sum_stake: 0,
							sum_stake_squared: 0
						}
					},
					Event::PhaseTransitioned {
						from: Phase::Unsigned((true, 25)),
						to: Phase::Off,
						round: 2
					},
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 2 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 55)),
						round: 2
					},
				]
			);
		})
	}

	#[test]
	fn signed_phase_void() {
		ExtBuilder::default().phases(0, 10).build_and_execute(|| {
			roll_to(15);
			assert!(CurrentPhase::<Runtime>::get().is_off());

			roll_to(19);
			assert!(CurrentPhase::<Runtime>::get().is_off());

			roll_to(20);
			assert!(CurrentPhase::<Runtime>::get().is_unsigned_open_at(20));
			assert!(Snapshot::<Runtime>::get().is_some());

			roll_to(30);
			assert!(CurrentPhase::<Runtime>::get().is_unsigned_open_at(20));

			assert_ok!(MultiPhase::elect(SINGLE_PAGE));

			assert!(CurrentPhase::<Runtime>::get().is_off());
			assert!(Snapshot::<Runtime>::get().is_none());

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned {
						from: Phase::Off,
						to: Phase::Unsigned((true, 20)),
						round: 1
					},
					Event::ElectionFinalized {
						compute: ElectionCompute::Fallback,
						score: ElectionScore {
							minimal_stake: 0,
							sum_stake: 0,
							sum_stake_squared: 0
						}
					},
					Event::PhaseTransitioned {
						from: Phase::Unsigned((true, 20)),
						to: Phase::Off,
						round: 2
					},
				]
			);
		});
	}

	#[test]
	fn unsigned_phase_void() {
		ExtBuilder::default().phases(10, 0).build_and_execute(|| {
			roll_to(15);
			assert!(CurrentPhase::<Runtime>::get().is_off());

			roll_to(19);
			assert!(CurrentPhase::<Runtime>::get().is_off());

			roll_to_signed();
			assert!(CurrentPhase::<Runtime>::get().is_signed());
			assert!(Snapshot::<Runtime>::get().is_some());

			roll_to(30);
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			assert_ok!(MultiPhase::elect(SINGLE_PAGE));

			assert!(CurrentPhase::<Runtime>::get().is_off());
			assert!(Snapshot::<Runtime>::get().is_none());

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::ElectionFinalized {
						compute: ElectionCompute::Fallback,
						score: ElectionScore {
							minimal_stake: 0,
							sum_stake: 0,
							sum_stake_squared: 0
						}
					},
					Event::PhaseTransitioned { from: Phase::Signed, to: Phase::Off, round: 2 },
				]
			)
		});
	}

	#[test]
	fn early_termination() {
		// An early termination in the signed phase, with no queued solution.
		ExtBuilder::default().build_and_execute(|| {
			// Signed phase started at block 15 and will end at 25.

			roll_to_signed();
			assert_eq!(
				multi_phase_events(),
				vec![Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 }]
			);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Signed);
			assert_eq!(Round::<Runtime>::get(), 1);

			// An unexpected call to elect.
			assert_ok!(MultiPhase::elect(SINGLE_PAGE));

			// We surely can't have any feasible solutions. This will cause an on-chain election.
			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::ElectionFinalized {
						compute: ElectionCompute::Fallback,
						score: Default::default()
					},
					Event::PhaseTransitioned { from: Phase::Signed, to: Phase::Off, round: 2 },
				],
			);
			// All storage items must be cleared.
			assert_eq!(Round::<Runtime>::get(), 2);
			assert!(Snapshot::<Runtime>::get().is_none());
			assert!(SnapshotMetadata::<Runtime>::get().is_none());
			assert!(DesiredTargets::<Runtime>::get().is_none());
			assert!(QueuedSolution::<Runtime>::get().is_none());
			assert!(MultiPhase::signed_submissions().is_empty());
		})
	}

	#[test]
	fn early_termination_with_submissions() {
		// an early termination in the signed phase, with no queued solution.
		ExtBuilder::default().build_and_execute(|| {
			// signed phase started at block 15 and will end at 25.

			roll_to_signed();
			assert_eq!(
				multi_phase_events(),
				vec![Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 }]
			);
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Signed);
			assert_eq!(Round::<Runtime>::get(), 1);

			// fill the queue with signed submissions
			for s in 0..SignedMaxSubmissions::get() {
				let solution = RawSolution {
					score: ElectionScore { minimal_stake: (5 + s).into(), ..Default::default() },
					..Default::default()
				};
				assert_ok!(MultiPhase::submit(
					crate::mock::RuntimeOrigin::signed(99),
					Box::new(solution)
				));
			}

			// an unexpected call to elect.
			assert_ok!(MultiPhase::elect(SINGLE_PAGE));

			// all storage items must be cleared.
			assert_eq!(Round::<Runtime>::get(), 2);
			assert!(Snapshot::<Runtime>::get().is_none());
			assert!(SnapshotMetadata::<Runtime>::get().is_none());
			assert!(DesiredTargets::<Runtime>::get().is_none());
			assert!(QueuedSolution::<Runtime>::get().is_none());
			assert!(MultiPhase::signed_submissions().is_empty());

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::SolutionStored {
						compute: ElectionCompute::Signed,
						origin: Some(99),
						prev_ejected: false
					},
					Event::SolutionStored {
						compute: ElectionCompute::Signed,
						origin: Some(99),
						prev_ejected: false
					},
					Event::SolutionStored {
						compute: ElectionCompute::Signed,
						origin: Some(99),
						prev_ejected: false
					},
					Event::SolutionStored {
						compute: ElectionCompute::Signed,
						origin: Some(99),
						prev_ejected: false
					},
					Event::SolutionStored {
						compute: ElectionCompute::Signed,
						origin: Some(99),
						prev_ejected: false
					},
					Event::Slashed { account: 99, value: 5 },
					Event::Slashed { account: 99, value: 5 },
					Event::Slashed { account: 99, value: 5 },
					Event::Slashed { account: 99, value: 5 },
					Event::Slashed { account: 99, value: 5 },
					Event::ElectionFinalized {
						compute: ElectionCompute::Fallback,
						score: ElectionScore {
							minimal_stake: 0,
							sum_stake: 0,
							sum_stake_squared: 0
						}
					},
					Event::PhaseTransitioned { from: Phase::Signed, to: Phase::Off, round: 2 },
				]
			);
		})
	}

	#[test]
	fn check_events_with_compute_signed() {
		ExtBuilder::default().build_and_execute(|| {
			roll_to_signed();
			assert!(CurrentPhase::<Runtime>::get().is_signed());

			let solution = raw_solution();
			assert_ok!(MultiPhase::submit(
				crate::mock::RuntimeOrigin::signed(99),
				Box::new(solution)
			));

			roll_to(30);
			assert_ok!(MultiPhase::elect(SINGLE_PAGE));

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::SolutionStored {
						compute: ElectionCompute::Signed,
						origin: Some(99),
						prev_ejected: false
					},
					Event::Rewarded { account: 99, value: 7 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 25)),
						round: 1
					},
					Event::ElectionFinalized {
						compute: ElectionCompute::Signed,
						score: ElectionScore {
							minimal_stake: 40,
							sum_stake: 100,
							sum_stake_squared: 5200
						}
					},
					Event::PhaseTransitioned {
						from: Phase::Unsigned((true, 25)),
						to: Phase::Off,
						round: 2
					},
				],
			);
		})
	}

	#[test]
	fn check_events_with_compute_unsigned() {
		ExtBuilder::default().build_and_execute(|| {
			roll_to_unsigned();
			assert!(CurrentPhase::<Runtime>::get().is_unsigned());

			// ensure we have snapshots in place.
			assert!(Snapshot::<Runtime>::get().is_some());
			assert_eq!(DesiredTargets::<Runtime>::get().unwrap(), 2);

			// mine seq_phragmen solution with 2 iters.
			let (solution, witness, _) = MultiPhase::mine_solution().unwrap();

			// ensure this solution is valid.
			assert!(QueuedSolution::<Runtime>::get().is_none());
			assert_ok!(MultiPhase::submit_unsigned(
				crate::mock::RuntimeOrigin::none(),
				Box::new(solution),
				witness
			));
			assert!(QueuedSolution::<Runtime>::get().is_some());

			assert_ok!(MultiPhase::elect(SINGLE_PAGE));

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 25)),
						round: 1
					},
					Event::SolutionStored {
						compute: ElectionCompute::Unsigned,
						origin: None,
						prev_ejected: false
					},
					Event::ElectionFinalized {
						compute: ElectionCompute::Unsigned,
						score: ElectionScore {
							minimal_stake: 40,
							sum_stake: 100,
							sum_stake_squared: 5200
						}
					},
					Event::PhaseTransitioned {
						from: Phase::Unsigned((true, 25)),
						to: Phase::Off,
						round: 2
					},
				],
			);
		})
	}

	#[test]
	fn try_elect_multi_page_fails() {
		let prepare_election = || {
			roll_to_signed();
			assert!(Snapshot::<Runtime>::get().is_some());

			// submit solution and assert it is queued and ready for elect to be called.
			let (solution, _, _) = MultiPhase::mine_solution().unwrap();
			assert_ok!(MultiPhase::submit(
				crate::mock::RuntimeOrigin::signed(99),
				Box::new(solution),
			));
			roll_to(30);
			assert!(QueuedSolution::<Runtime>::get().is_some());
		};

		ExtBuilder::default().onchain_fallback(false).build_and_execute(|| {
			prepare_election();
			// single page elect call works as expected.
			assert_ok!(MultiPhase::elect(SINGLE_PAGE));
		});

		ExtBuilder::default().onchain_fallback(false).build_and_execute(|| {
			prepare_election();
			// multi page calls will fail with multi-page not supported error.
			assert_noop!(MultiPhase::elect(SINGLE_PAGE + 1), ElectionError::MultiPageNotSupported);
		})
	}

	#[test]
	fn fallback_strategy_works() {
		ExtBuilder::default().onchain_fallback(true).build_and_execute(|| {
			roll_to_unsigned();
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Unsigned((true, 25)));

			// Zilch solutions thus far, but we get a result.
			assert!(QueuedSolution::<Runtime>::get().is_none());
			let supports = MultiPhase::elect(SINGLE_PAGE).unwrap();

			let expected_supports = vec![
				(30, Support { total: 40, voters: vec![(2, 5), (4, 5), (30, 30)] }),
				(40, Support { total: 60, voters: vec![(2, 5), (3, 10), (4, 5), (40, 40)] }),
			]
			.try_into()
			.unwrap();

			assert_eq!(supports, expected_supports);

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 25)),
						round: 1
					},
					Event::ElectionFinalized {
						compute: ElectionCompute::Fallback,
						score: ElectionScore {
							minimal_stake: 0,
							sum_stake: 0,
							sum_stake_squared: 0
						}
					},
					Event::PhaseTransitioned {
						from: Phase::Unsigned((true, 25)),
						to: Phase::Off,
						round: 2
					},
				]
			);
		});

		ExtBuilder::default().onchain_fallback(false).build_and_execute(|| {
			roll_to_unsigned();
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Unsigned((true, 25)));

			// Zilch solutions thus far.
			assert!(QueuedSolution::<Runtime>::get().is_none());
			assert_eq!(
				MultiPhase::elect(SINGLE_PAGE).unwrap_err(),
				ElectionError::Fallback("NoFallback.")
			);
			// phase is now emergency.
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Emergency);
			// snapshot is still there until election finalizes.
			assert!(Snapshot::<Runtime>::get().is_some());

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 25)),
						round: 1
					},
					Event::ElectionFailed,
					Event::PhaseTransitioned {
						from: Phase::Unsigned((true, 25)),
						to: Phase::Emergency,
						round: 1
					},
				]
			);
		})
	}

	#[test]
	fn governance_fallback_works() {
		ExtBuilder::default().onchain_fallback(false).build_and_execute(|| {
			roll_to_unsigned();
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Unsigned((true, 25)));

			// Zilch solutions thus far.
			assert!(QueuedSolution::<Runtime>::get().is_none());
			assert_eq!(
				MultiPhase::elect(SINGLE_PAGE).unwrap_err(),
				ElectionError::Fallback("NoFallback.")
			);

			// phase is now emergency.
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Emergency);
			assert!(QueuedSolution::<Runtime>::get().is_none());
			assert!(Snapshot::<Runtime>::get().is_some());

			// no single account can trigger this
			assert_noop!(
				MultiPhase::governance_fallback(RuntimeOrigin::signed(99)),
				DispatchError::BadOrigin
			);

			// only root can
			assert_ok!(MultiPhase::governance_fallback(RuntimeOrigin::root()));
			// something is queued now
			assert!(QueuedSolution::<Runtime>::get().is_some());
			// next election call with fix everything.;
			assert!(MultiPhase::elect(SINGLE_PAGE).is_ok());
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Off);

			assert_eq!(
				multi_phase_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Signed, round: 1 },
					Event::PhaseTransitioned {
						from: Phase::Signed,
						to: Phase::Unsigned((true, 25)),
						round: 1
					},
					Event::ElectionFailed,
					Event::PhaseTransitioned {
						from: Phase::Unsigned((true, 25)),
						to: Phase::Emergency,
						round: 1
					},
					Event::SolutionStored {
						compute: ElectionCompute::Fallback,
						origin: None,
						prev_ejected: false
					},
					Event::ElectionFinalized {
						compute: ElectionCompute::Fallback,
						score: Default::default()
					},
					Event::PhaseTransitioned { from: Phase::Emergency, to: Phase::Off, round: 2 },
				]
			);
		})
	}

	#[test]
	fn snapshot_too_big_truncate() {
		// but if there are too many voters, we simply truncate them.
		ExtBuilder::default().build_and_execute(|| {
			// we have 8 voters in total.
			assert_eq!(Voters::get().len(), 8);
			// but we want to take 2.
			let new_bounds = ElectionBoundsBuilder::default().voters_count(2.into()).build();
			ElectionsBounds::set(new_bounds);

			// Signed phase opens just fine.
			roll_to_signed();
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Signed);

			assert_eq!(
				SnapshotMetadata::<Runtime>::get().unwrap(),
				SolutionOrSnapshotSize { voters: 2, targets: 4 }
			);
		})
	}

	#[test]
	fn untrusted_score_verification_is_respected() {
		ExtBuilder::default().build_and_execute(|| {
			roll_to_signed();
			assert_eq!(CurrentPhase::<Runtime>::get(), Phase::Signed);

			// set the solution balancing to get the desired score.
			crate::mock::Balancing::set(Some(BalancingConfig { iterations: 2, tolerance: 0 }));

			let (solution, _, _) = MultiPhase::mine_solution().unwrap();
			// Default solution's score.
			assert!(matches!(solution.score, ElectionScore { minimal_stake: 50, .. }));

			MinimumUntrustedScore::<Runtime>::put(ElectionScore {
				minimal_stake: 49,
				..Default::default()
			});
			assert_ok!(MultiPhase::feasibility_check(solution.clone(), ElectionCompute::Signed));

			MinimumUntrustedScore::<Runtime>::put(ElectionScore {
				minimal_stake: 51,
				..Default::default()
			});
			assert_noop!(
				MultiPhase::feasibility_check(solution, ElectionCompute::Signed),
				FeasibilityError::UntrustedScoreTooLow,
			);
		})
	}

	#[test]
	fn number_of_voters_allowed_2sec_block() {
		// Just a rough estimate with the substrate weights.
		assert_eq!(MockWeightInfo::get(), MockedWeightInfo::Real);

		let all_voters: u32 = 10_000;
		let all_targets: u32 = 5_000;
		let desired: u32 = 1_000;
		let weight_with = |active| {
			<Runtime as Config>::WeightInfo::submit_unsigned(
				all_voters,
				all_targets,
				active,
				desired,
			)
		};

		let mut active = 1;
		while weight_with(active)
			.all_lte(<Runtime as frame_system::Config>::BlockWeights::get().max_block) ||
			active == all_voters
		{
			active += 1;
		}

		println!("can support {} voters to yield a weight of {}", active, weight_with(active));
	}
}
