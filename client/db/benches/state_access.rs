// This file is part of Substrate.

// Copyright (C) 2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use pprof::criterion::{Output, PProfProfiler};
use rand::{distributions::Uniform, rngs::StdRng, Rng, SeedableRng};
use sc_client_api::{Backend as _, BlockImportOperation, NewBlockState, StateBackend};
use sc_client_db::{
	Backend, DatabaseSettings, DatabaseSource, KeepBlocks, PruningMode, TransactionStorageMode,
	TrieNodeCacheSettings,
};
use sp_core::H256;
use sp_runtime::{
	generic::BlockId,
	testing::{Block as RawBlock, ExtrinsicWrapper, Header},
	StateVersion, Storage,
};
use std::path::PathBuf;
use tempfile::TempDir;

pub(crate) type Block = RawBlock<ExtrinsicWrapper<u64>>;

fn insert_blocks(db: &Backend<Block>, storage: Vec<(Vec<u8>, Vec<u8>)>) -> H256 {
	let mut op = db.begin_operation().unwrap();
	let mut header = Header {
		number: 0,
		parent_hash: Default::default(),
		state_root: Default::default(),
		digest: Default::default(),
		extrinsics_root: Default::default(),
	};

	header.state_root = op
		.set_genesis_state(
			Storage {
				top: vec![(vec![1], vec![2])].into_iter().collect(),
				children_default: Default::default(),
			},
			true,
			StateVersion::V1,
		)
		.unwrap();

	op.set_block_data(header.clone(), Some(vec![]), None, None, NewBlockState::Best)
		.unwrap();

	db.commit_operation(op).unwrap();

	let mut number = 1;
	let mut parent_hash = header.hash();

	for i in 0..10 {
		let mut op = db.begin_operation().unwrap();

		db.begin_state_operation(&mut op, BlockId::Hash(parent_hash)).unwrap();

		let mut header = Header {
			number,
			parent_hash,
			state_root: Default::default(),
			digest: Default::default(),
			extrinsics_root: Default::default(),
		};

		let changes = storage
			.iter()
			.skip(i * 100_000)
			.take(100_000)
			.map(|(k, v)| (k.clone(), Some(v.clone())))
			.collect::<Vec<_>>();

		let (state_root, tx) = db
			.state_at(BlockId::Number(number - 1))
			.unwrap()
			.storage_root(changes.iter().map(|(k, v)| (k.as_slice(), v.as_deref())), StateVersion::V1);
		header.state_root = state_root;

		op.update_db_storage(tx).unwrap();
		op.update_storage(changes.clone(), Default::default()).unwrap();

		op.set_block_data(header.clone(), Some(vec![]), None, None, NewBlockState::Best)
			.unwrap();

		db.commit_operation(op).unwrap();

		number += 1;
		parent_hash = header.hash();
	}

	parent_hash
}

enum BenchmarkConfig {
	NoCache,
	TrieNodeCache,
	TrieNodeCacheWithoutDataCache,
	StateCache,
}

fn create_backend(config: BenchmarkConfig, temp_dir: &TempDir) -> Backend<Block> {
	let path = temp_dir.path().to_owned();
	std::fs::remove_dir_all(&path).expect("Deletes old db files");

	let (state_cache_size, trie_node_cache_settings) = match config {
		BenchmarkConfig::NoCache => (0, TrieNodeCacheSettings { enable: false, fast_cache: false }),
		BenchmarkConfig::TrieNodeCache =>
			(0, TrieNodeCacheSettings { enable: true, fast_cache: true }),
		BenchmarkConfig::TrieNodeCacheWithoutDataCache =>
			(0, TrieNodeCacheSettings { enable: true, fast_cache: false }),
		BenchmarkConfig::StateCache =>
			(2048 * 1024 * 1024, TrieNodeCacheSettings { enable: false, fast_cache: false }),
	};

	let settings = DatabaseSettings {
		state_cache_size,
		trie_node_cache_settings,
		state_cache_child_ratio: None,
		state_pruning: PruningMode::ArchiveAll,
		source: DatabaseSource::ParityDb { path },
		keep_blocks: KeepBlocks::All,
		transaction_storage: TransactionStorageMode::BlockBody,
	};

	Backend::new(settings, 100).expect("Creates backend")
}

/// Generate the storage that will be used for the benchmark
///
/// Returns the `Vec<key>` and the `Vec<(key, value)>`
fn generate_storage() -> (Vec<Vec<u8>>, Vec<(Vec<u8>, Vec<u8>)>) {
	let mut rng = StdRng::seed_from_u64(353893213);

	let mut storage = Vec::new();
	let mut keys = Vec::new();

	for _ in 0..1_000_000 {
		let key_len: usize = rng.gen_range(32..128);
		let key = (&mut rng)
			.sample_iter(Uniform::new_inclusive(0, 255))
			.take(key_len)
			.collect::<Vec<u8>>();

		let value_len: usize = rng.gen_range(20..60);
		let value = (&mut rng)
			.sample_iter(Uniform::new_inclusive(0, 255))
			.take(value_len)
			.collect::<Vec<u8>>();

		keys.push(key.clone());
		storage.push((key, value));
	}

	(keys, storage)
}

fn state_access_benchmarks(c: &mut Criterion) {
	sp_tracing::try_init_simple();

	let (keys, storage) = generate_storage();
	let path = TempDir::new().expect("Creates temporary directory");

	let mut group = c.benchmark_group("State access multiple values");
	group.sample_size(20);

	let mut bench_multiple_values = |config, desc, multiplier| {
		let backend = create_backend(config, &path);
		let block_hash = insert_blocks(&backend, storage.clone());

		group.bench_function(desc, |b| {
			b.iter_batched(
				|| backend.state_at(BlockId::Hash(block_hash)).expect("Creates state"),
				|state| {
					for key in keys.iter().cycle().take(keys.len() * multiplier) {
						let _ = state.storage(&key).expect("Doesn't fail").unwrap();
					}
				},
				BatchSize::SmallInput,
			)
		});
	};

	bench_multiple_values(
		BenchmarkConfig::StateCache,
		"with state cache and reading each key once",
		1,
	);
	bench_multiple_values(
		BenchmarkConfig::TrieNodeCache,
		"with trie node cache and reading each key once",
		1,
	);
	bench_multiple_values(
		BenchmarkConfig::TrieNodeCacheWithoutDataCache,
		"with trie node cache (without value cache) and reading each key once",
		1,
	);
	bench_multiple_values(BenchmarkConfig::NoCache, "no cache and reading each key once", 1);

	bench_multiple_values(
		BenchmarkConfig::StateCache,
		"with state cache and reading 4 times each key",
		4,
	);
	bench_multiple_values(
		BenchmarkConfig::TrieNodeCache,
		"with trie node cache and reading 4 times each key",
		4,
	);
	bench_multiple_values(
		BenchmarkConfig::TrieNodeCacheWithoutDataCache,
		"with trie node cache (without value cache) and reading 4 times each key",
		4,
	);
	bench_multiple_values(BenchmarkConfig::NoCache, "no cache and reading 4 times each key", 4);

	group.finish();

	let mut group = c.benchmark_group("State access single value");

	let mut bench_single_value = |config, desc, multiplier| {
		let backend = create_backend(config, &path);
		let block_hash = insert_blocks(&backend, storage.clone());

		group.bench_function(desc, |b| {
			b.iter_batched(
				|| backend.state_at(BlockId::Hash(block_hash)).expect("Creates state"),
				|state| {
					for key in keys.iter().take(1).cycle().take(multiplier) {
						let _ = state.storage(&key).expect("Doesn't fail").unwrap();
					}
				},
				BatchSize::SmallInput,
			)
		});
	};

	bench_single_value(BenchmarkConfig::StateCache, "with state cache and reading the key once", 1);
	bench_single_value(
		BenchmarkConfig::TrieNodeCache,
		"with trie node cache and reading the key once",
		1,
	);
	bench_single_value(
		BenchmarkConfig::TrieNodeCacheWithoutDataCache,
		"with trie node cache (without value cache) and reading the key once",
		1,
	);
	bench_single_value(BenchmarkConfig::NoCache, "no cache and reading the key once", 1);

	bench_single_value(
		BenchmarkConfig::StateCache,
		"with state cache and reading 4 times the key",
		4,
	);
	bench_single_value(
		BenchmarkConfig::TrieNodeCache,
		"with trie node cache and reading 4 times the key",
		4,
	);
	bench_single_value(
		BenchmarkConfig::TrieNodeCacheWithoutDataCache,
		"with trie node cache (without value cache) and reading 4 times the key",
		4,
	);
	bench_single_value(BenchmarkConfig::NoCache, "no cache and reading 4 times the key", 4);

	group.finish();

	let mut group = c.benchmark_group("State value hash");

	let mut bench_single_value = |config, desc, multiplier| {
		let backend = create_backend(config, &path);
		let block_hash = insert_blocks(&backend, storage.clone());

		group.bench_function(desc, |b| {
			b.iter_batched(
				|| backend.state_at(BlockId::Hash(block_hash)).expect("Creates state"),
				|state| {
					for key in keys.iter().take(1).cycle().take(multiplier) {
						let _ = state.storage_hash(&key).expect("Doesn't fail").unwrap();
					}
				},
				BatchSize::SmallInput,
			)
		});
	};

	bench_single_value(BenchmarkConfig::StateCache, "with state cache and hashing the key once", 1);
	bench_single_value(
		BenchmarkConfig::TrieNodeCache,
		"with trie node cache and hashing the key once",
		1,
	);
	bench_single_value(
		BenchmarkConfig::TrieNodeCacheWithoutDataCache,
		"with trie node cache (without value cache) and hashing the key once",
		1,
	);
	bench_single_value(BenchmarkConfig::NoCache, "no cache and hashing the key once", 1);

	bench_single_value(
		BenchmarkConfig::StateCache,
		"with state cache and hashing 4 times the key",
		4,
	);
	bench_single_value(
		BenchmarkConfig::TrieNodeCache,
		"with trie node cache and hashing 4 times the key",
		4,
	);
	bench_single_value(
		BenchmarkConfig::TrieNodeCacheWithoutDataCache,
		"with trie node cache (without value cache) and hashing 4 times the key",
		4,
	);
	bench_single_value(BenchmarkConfig::NoCache, "no cache and hashing 4 times the key", 4);

	group.finish();
}

criterion_group! {
	name = benches;
	config = Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
	targets = state_access_benchmarks
}
criterion_main!(benches);