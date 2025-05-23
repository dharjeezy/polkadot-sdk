// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: MIT-0

// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is furnished to do
// so, subject to the following conditions:

// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! # Default Config Pallet Example
//!
//! A simple example of a FRAME pallet that utilizes [`frame_support::derive_impl`] to demonstrate
//! the simpler way to implement `Config` trait of pallets. This example only showcases this in a
//! `mock.rs` environment, but the same applies to a real runtime as well.
//!
//! See the source code of [`tests`] for a real examples.
//!
//! Study the following types:
//!
//! - [`pallet::DefaultConfig`], and how it differs from [`pallet::Config`].
//! - [`struct@pallet::config_preludes::TestDefaultConfig`] and how it implements
//!   [`pallet::DefaultConfig`].
//! - Notice how [`pallet::DefaultConfig`] is independent of [`frame_system::Config`].

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[frame_support::pallet]
pub mod pallet {
	use frame_support::pallet_prelude::*;

	/// This pallet is annotated to have a default config. This will auto-generate
	/// [`DefaultConfig`].
	///
	/// It will be an identical, but won't have anything that is `#[pallet::no_default]`.
	#[pallet::config(with_default)]
	pub trait Config: frame_system::Config {
		/// The overarching task type. This is coming from the runtime, and cannot have a default.  
		/// In general, `Runtime*`-oriented types cannot have a sensible default.
		#[pallet::no_default] // optional. `RuntimeEvent` is automatically excluded as well.
		type RuntimeTask: Task;

		/// An input parameter to this pallet. This value can have a default, because it is not
		/// reliant on `frame_system::Config` or the overarching runtime in any way.
		type WithDefaultValue: Get<u32>;

		/// Same as [`Config::WithDefaultValue`], but we don't intend to define a default for this
		/// in our tests below.
		type OverwrittenDefaultValue: Get<u32>;

		/// An input parameter that relies on `<Self as frame_system::Config>::AccountId`. This can
		/// too have a default, as long as it is present in `frame_system::DefaultConfig`.
		type CanDeriveDefaultFromSystem: Get<Self::AccountId>;

		/// We might choose to declare as one that doesn't have a default, for whatever semantical
		/// reason.
		#[pallet::no_default]
		type HasNoDefault: Get<u32>;

		/// Some types can technically have no default, such as those the rely on
		/// `frame_system::Config` but are not present in `frame_system::DefaultConfig`. For
		/// example, a `RuntimeCall` cannot reasonably have a default.
		#[pallet::no_default] // if we skip this, there will be a compiler error.
		type CannotHaveDefault: Get<Self::RuntimeCall>;

		/// Something that is a normal type, with default.
		type WithDefaultType;

		/// Same as [`Config::WithDefaultType`], but we don't intend to define a default for this
		/// in our tests below.
		type OverwrittenDefaultType;
	}

	/// Container for different types that implement [`DefaultConfig`]` of this pallet.
	pub mod config_preludes {
		// This will help use not need to disambiguate anything when using `derive_impl`.
		use super::*;
		use frame_support::derive_impl;

		/// A type providing default configurations for this pallet in testing environment.
		pub struct TestDefaultConfig;

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for TestDefaultConfig {}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			type WithDefaultValue = frame_support::traits::ConstU32<42>;
			type OverwrittenDefaultValue = frame_support::traits::ConstU32<42>;

			// `frame_system::config_preludes::TestDefaultConfig` declares account-id as u64.
			type CanDeriveDefaultFromSystem = frame_support::traits::ConstU64<42>;

			type WithDefaultType = u32;
			type OverwrittenDefaultType = u32;
		}

		/// A type providing default configurations for this pallet in another environment. Examples
		/// could be a parachain, or a solochain.
		///
		/// Appropriate derive for `frame_system::DefaultConfig` needs to be provided. In this
		/// example, we simple derive `frame_system::config_preludes::TestDefaultConfig` again.
		pub struct OtherDefaultConfig;

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for OtherDefaultConfig {}

		#[frame_support::register_default_impl(OtherDefaultConfig)]
		impl DefaultConfig for OtherDefaultConfig {
			type WithDefaultValue = frame_support::traits::ConstU32<66>;
			type OverwrittenDefaultValue = frame_support::traits::ConstU32<66>;
			type CanDeriveDefaultFromSystem = frame_support::traits::ConstU64<42>;
			type WithDefaultType = u32;
			type OverwrittenDefaultType = u32;
		}
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::event]
	pub enum Event<T: Config> {}
}

#[cfg(any(test, doc))]
pub mod tests {
	use super::*;
	use frame_support::{derive_impl, parameter_types};
	use pallet::{self as pallet_default_config_example, config_preludes::*};

	type Block = frame_system::mocking::MockBlock<Runtime>;

	frame_support::construct_runtime!(
		pub enum Runtime {
			System: frame_system,
			DefaultPallet: pallet_default_config_example,
		}
	);

	#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
	impl frame_system::Config for Runtime {
		// these items are defined by frame-system as `no_default`, so we must specify them here.
		type Block = Block;

		// all of this is coming from `frame_system::config_preludes::TestDefaultConfig`.

		// type Nonce = u32;
		// type BlockNumber = u32;
		// type Hash = sp_core::hash::H256;
		// type Hashing = sp_runtime::traits::BlakeTwo256;
		// type AccountId = u64;
		// type Lookup = sp_runtime::traits::IdentityLookup<u64>;
		// type BlockHashCount = frame_support::traits::ConstU32<10>;
		// type MaxConsumers = frame_support::traits::ConstU32<16>;
		// type AccountData = ();
		// type OnNewAccount = ();
		// type OnKilledAccount = ();
		// type SystemWeightInfo = ();
		// type SS58Prefix = ();
		// type Version = ();
		// type BlockWeights = ();
		// type BlockLength = ();
		// type DbWeight = ();
		// type BaseCallFilter = frame_support::traits::Everything;
		// type BlockHashCount = frame_support::traits::ConstU64<10>;
		// type OnSetCode = ();

		// These are marked as `#[inject_runtime_type]`. Hence, they are being injected as
		// types generated by `construct_runtime`.

		// type RuntimeOrigin = RuntimeOrigin;
		// type RuntimeCall = RuntimeCall;
		// type RuntimeEvent = RuntimeEvent;
		// type PalletInfo = PalletInfo;

		// you could still overwrite any of them if desired.
		type SS58Prefix = frame_support::traits::ConstU16<456>;
	}

	parameter_types! {
		pub const SomeCall: RuntimeCall = RuntimeCall::System(frame_system::Call::<Runtime>::remark { remark: alloc::vec![] });
	}

	#[derive_impl(TestDefaultConfig as pallet::DefaultConfig)]
	impl pallet_default_config_example::Config for Runtime {
		// This cannot have default.
		type RuntimeTask = RuntimeTask;

		type HasNoDefault = frame_support::traits::ConstU32<1>;
		type CannotHaveDefault = SomeCall;

		type OverwrittenDefaultValue = frame_support::traits::ConstU32<678>;
		type OverwrittenDefaultType = u128;
	}

	#[test]
	fn it_works() {
		use frame_support::traits::Get;
		use pallet::{Config, DefaultConfig};

		// assert one of the value types that is not overwritten.
		assert_eq!(
			<<Runtime as Config>::WithDefaultValue as Get<u32>>::get(),
			<<TestDefaultConfig as DefaultConfig>::WithDefaultValue as Get<u32>>::get()
		);

		// assert one of the value types that is overwritten.
		assert_eq!(<<Runtime as Config>::OverwrittenDefaultValue as Get<u32>>::get(), 678u32);

		// assert one of the types that is not overwritten.
		assert_eq!(
			std::any::TypeId::of::<<Runtime as Config>::WithDefaultType>(),
			std::any::TypeId::of::<<TestDefaultConfig as DefaultConfig>::WithDefaultType>()
		);

		// assert one of the types that is overwritten.
		assert_eq!(
			std::any::TypeId::of::<<Runtime as Config>::OverwrittenDefaultType>(),
			std::any::TypeId::of::<u128>()
		)
	}
}
