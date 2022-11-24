// Copyright 2022 Parity Technologies (UK) Ltd.
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

#![cfg(test)]

pub use super::mock_helpers::*;
use super::*;

use crate as pallet_message_queue;
use frame_support::{
	parameter_types,
	traits::{ConstU32, ConstU64, Defensive},
};
use sp_core::H256;
use sp_runtime::{
	testing::Header,
	traits::{BlakeTwo256, IdentityLookup},
};
use sp_std::collections::btree_map::BTreeMap;

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test where
		Block = Block,
		NodeBlock = Block,
		UncheckedExtrinsic = UncheckedExtrinsic,
	{
		System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
		MessageQueue: pallet_message_queue::{Pallet, Call, Storage, Event<T>},
	}
);
parameter_types! {
	pub BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(frame_support::weights::Weight::from_ref_time(1024));
}
impl frame_system::Config for Test {
	type BaseCallFilter = frame_support::traits::Everything;
	type BlockWeights = ();
	type BlockLength = ();
	type DbWeight = ();
	type RuntimeOrigin = RuntimeOrigin;
	type Index = u64;
	type BlockNumber = u64;
	type Hash = H256;
	type RuntimeCall = RuntimeCall;
	type Hashing = BlakeTwo256;
	type AccountId = u64;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Header = Header;
	type RuntimeEvent = RuntimeEvent;
	type BlockHashCount = ConstU64<250>;
	type Version = ();
	type PalletInfo = PalletInfo;
	type AccountData = ();
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type SystemWeightInfo = ();
	type SS58Prefix = ();
	type OnSetCode = ();
	type MaxConsumers = ConstU32<16>;
}
parameter_types! {
	pub const HeapSize: u32 = 24;
	pub const MaxStale: u32 = 2;
	pub const ServiceWeight: Option<Weight> = Some(Weight::from_parts(10, 10));
}
impl Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = MockedWeightInfo;
	type MessageProcessor = RecordingMessageProcessor;
	type Size = u32;
	type QueueChangeHandler = RecordingQueueChangeHandler;
	type HeapSize = HeapSize;
	type MaxStale = MaxStale;
	type ServiceWeight = ServiceWeight;
}

/// Mocked `WeightInfo` impl with allows to set the weight per call.
pub struct MockedWeightInfo;

parameter_types! {
	/// Storage for `MockedWeightInfo`, do not use directly.
	pub static WeightForCall: BTreeMap<String, Weight> = Default::default();
}

/// Set the return value for a function from the `WeightInfo` trait.
impl MockedWeightInfo {
	/// Set the weight of a specific weight function.
	pub fn set_weight<T: Config>(call_name: &str, weight: Weight) {
		let mut calls = WeightForCall::get();
		calls.insert(call_name.into(), weight);
		WeightForCall::set(calls);
	}
}

impl crate::weights::WeightInfo for MockedWeightInfo {
	fn reap_page() -> Weight {
		WeightForCall::get().get("reap_page").copied().unwrap_or_default()
	}
	fn execute_overweight() -> Weight {
		WeightForCall::get().get("execute_overweight").copied().unwrap_or_default()
	}
	fn service_page_base_completion() -> Weight {
		WeightForCall::get()
			.get("service_page_base_completion")
			.copied()
			.unwrap_or_default()
	}
	fn service_page_base_no_completion() -> Weight {
		WeightForCall::get()
			.get("service_page_base_no_completion")
			.copied()
			.unwrap_or_default()
	}
	fn service_queue_base() -> Weight {
		WeightForCall::get().get("service_queue_base").copied().unwrap_or_default()
	}
	fn bump_service_head() -> Weight {
		WeightForCall::get().get("bump_service_head").copied().unwrap_or_default()
	}
	fn service_page_item() -> Weight {
		WeightForCall::get().get("service_page_item").copied().unwrap_or_default()
	}
	fn ready_ring_unknit() -> Weight {
		WeightForCall::get().get("ready_ring_unknit").copied().unwrap_or_default()
	}
}

parameter_types! {
	pub static MessagesProcessed: Vec<(Vec<u8>, MessageOrigin)> = vec![];
}

pub struct RecordingMessageProcessor;
impl ProcessMessage for RecordingMessageProcessor {
	/// The transport from where a message originates.
	type Origin = MessageOrigin;

	/// Process the given message, using no more than `weight_limit` in weight to do so.
	///
	/// Consumes exactly `n` weight of all components if it starts `weight=n` and `1` otherwise.
	/// Errors if given the `weight_limit` is insufficient to process the message.
	fn process_message(
		message: &[u8],
		origin: Self::Origin,
		weight_limit: Weight,
	) -> Result<(bool, Weight), ProcessMessageError> {
		let weight = if message.starts_with(&b"weight="[..]) {
			let mut w: u64 = 0;
			for &c in &message[7..] {
				if (b'0'..=b'9').contains(&c) {
					w = w * 10 + (c - b'0') as u64;
				} else {
					break
				}
			}
			w
		} else {
			1
		};
		let weight = Weight::from_parts(weight, weight);
		if weight.all_lte(weight_limit) {
			let mut m = MessagesProcessed::get();
			m.push((message.to_vec(), origin));
			MessagesProcessed::set(m);
			Ok((true, weight))
		} else {
			Err(ProcessMessageError::Overweight(weight))
		}
	}
}

parameter_types! {
	pub static NumMessagesProcessed: usize = 0;
}

/// Similar to [`RecordingMessageProcessor`] but only counts the number of messages processed and
/// does always consume one weight per message.
///
/// The [`RecordingMessageProcessor`] is a bit too slow for the integration tests.
pub struct CountingMessageProcessor;
impl ProcessMessage for CountingMessageProcessor {
	type Origin = MessageOrigin;

	fn process_message(
		_message: &[u8],
		_origin: Self::Origin,
		weight_limit: Weight,
	) -> Result<(bool, Weight), ProcessMessageError> {
		let weight = Weight::from_parts(1, 1);

		if weight.all_lte(weight_limit) {
			NumMessagesProcessed::set(NumMessagesProcessed::get() + 1);
			Ok((true, weight))
		} else {
			Err(ProcessMessageError::Overweight(weight))
		}
	}
}

parameter_types! {
	/// Storage for `RecordingQueueChangeHandler`, do not use directly.
	pub static QueueChanges: Vec<(MessageOrigin, u32, u32)> = vec![];
}

/// Records all queue changes into [`QueueChanges`].
pub struct RecordingQueueChangeHandler;
impl OnQueueChanged<MessageOrigin> for RecordingQueueChangeHandler {
	fn on_queue_changed(id: MessageOrigin, items_count: u32, items_size: u32) {
		QueueChanges::mutate(|cs| cs.push((id, items_count, items_size)));
	}
}

/// Create new test externalities.
///
/// Is generic since it is used by the unit test, integration tests and benchmarks.
pub fn new_test_ext<T: Config>() -> sp_io::TestExternalities
where
	<T as frame_system::Config>::BlockNumber: From<u32>,
{
	sp_tracing::try_init_simple();
	WeightForCall::set(Default::default());
	QueueChanges::set(Default::default());
	let t = frame_system::GenesisConfig::default().build_storage::<T>().unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| frame_system::Pallet::<T>::set_block_number(1.into()));
	ext
}

/// Set the weight of a specific weight function.
pub fn set_weight(name: &str, w: Weight) {
	MockedWeightInfo::set_weight::<Test>(name, w);
}

/// Assert that exactly these pages are present. Assumes `Here` origin.
pub fn assert_pages(indices: &[u32]) {
	assert_eq!(Pages::<Test>::iter().count(), indices.len());
	for i in indices {
		assert!(Pages::<Test>::contains_key(MessageOrigin::Here, i));
	}
}

/// Knit a queue into the ready-ring and write it back to storage.
pub fn knit(o: &MessageOrigin) {
	let mut b = BookStateFor::<Test>::get(o);
	b.ready_neighbours = MessageQueue::ready_ring_knit(o).ok().defensive();
	BookStateFor::<Test>::insert(o, b);
}

/// Unknit a queue into the ready-ring and write it back to storage.
pub fn unknit(o: &MessageOrigin) {
	let mut b = BookStateFor::<Test>::get(o);
	MessageQueue::ready_ring_unknit(o, b.ready_neighbours.unwrap());
	b.ready_neighbours = None;
	BookStateFor::<Test>::insert(o, b);
}

/// Build a ring with three queues: `Here`, `There` and `Everywhere(0)`.
pub fn build_triple_ring() {
	use MessageOrigin::*;
	BookStateFor::<Test>::insert(Here, empty_book::<Test>());
	BookStateFor::<Test>::insert(There, empty_book::<Test>());
	BookStateFor::<Test>::insert(Everywhere(0), empty_book::<Test>());

	// Knit them into the ready ring.
	knit(&Here);
	knit(&There);
	knit(&Everywhere(0));
	assert_ring(&[Here, There, Everywhere(0)]);
}

/// Check that the Ready Ring consists of `neighbours` in that exact order.
///
/// Also check that all backlinks are valid and that the first element is the service head.
pub fn assert_ring(neighbours: &[MessageOrigin]) {
	for (i, origin) in neighbours.iter().enumerate() {
		let book = BookStateFor::<Test>::get(origin);
		assert_eq!(
			book.ready_neighbours,
			Some(Neighbours {
				prev: neighbours[(i + neighbours.len() - 1) % neighbours.len()],
				next: neighbours[(i + 1) % neighbours.len()],
			})
		);
	}
	assert_eq!(ServiceHead::<Test>::get(), neighbours.first().cloned());
}
