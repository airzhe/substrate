// This file is part of Substrate.

// Copyright (C) 2022 Parity Technologies (UK) Ltd.
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

// TODO: clean and standardize the imports

use crate::{helpers, SolutionOf, SupportsOf};
use codec::{Decode, Encode, MaxEncodedLen};
use frame_election_provider_support::{ExtendedBalance, PageIndex, TryIntoBoundedSupports};
use sp_npos_elections::{ElectionScore, EvaluateSupport, NposSolution};
use sp_std::{collections::btree_map::BTreeMap, prelude::*};

use super::*;
use frame_support::{dispatch::Weight, ensure, traits::Get, RuntimeDebug};

use pallet::*;

#[derive(Encode, Decode, scale_info::TypeInfo, Clone, Copy, MaxEncodedLen, RuntimeDebug)]
#[cfg_attr(any(test, debug_assertions), derive(PartialEq, Eq))]
pub enum Status {
	Ongoing(PageIndex),
	Nothing,
}

impl Default for Status {
	fn default() -> Self {
		Self::Nothing
	}
}

#[derive(Encode, Decode, scale_info::TypeInfo, Clone, Copy, MaxEncodedLen)]
enum ValidSolution {
	X,
	Y,
}

impl Default for ValidSolution {
	fn default() -> Self {
		ValidSolution::Y
	}
}

impl ValidSolution {
	fn other(&self) -> Self {
		match *self {
			ValidSolution::X => ValidSolution::Y,
			ValidSolution::Y => ValidSolution::X,
		}
	}
}

#[frame_support::pallet]
pub(crate) mod pallet {
	use crate::{
		types::{Pagify, SupportsOf},
		verifier::Verifier,
	};

	use super::*;
	use frame_support::pallet_prelude::{ValueQuery, *};
	use sp_npos_elections::evaluate_support_core;

	/// A simple newtype that represents the partial backing of a winner. It only stores the total
	/// backing, and the sum of backings, as opposed to a [`sp_npos_elections::Support`] that also
	/// stores all of the backers' individual contribution.
	///
	/// This is mainly here to allow us to implement `Backings` for it.
	#[derive(Default, Encode, Decode, MaxEncodedLen, TypeInfo)]
	pub struct PartialBackings {
		/// The total backing of this particular winner.
		pub total: ExtendedBalance,
		/// The number of backers.
		pub backers: u32,
	}

	impl sp_npos_elections::Backings for PartialBackings {
		fn total(&self) -> ExtendedBalance {
			self.total
		}
	}

	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config: crate::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// Origin that can control this pallet. Note that any action taken by this origin (such)
		/// as providing an emergency solution is not checked. Thus, it must be a trusted origin.
		type ForceOrigin: EnsureOrigin<Self::Origin>;

		/// The minimum amount of improvement to the solution score that defines a solution as
		/// "better".
		#[pallet::constant]
		type SolutionImprovementThreshold: Get<sp_runtime::Perbill>;

		/// Maximum number of voters that can support a single target, among ALL pages of a
		/// verifying solution. It can only ever be checked on the last page of any given
		/// verification.
		///
		/// This must be set such that the memory limits in the rest of the system are well
		/// respected.
		type MaxBackersPerWinner: Get<u32>;

		/// Maximum number of supports (aka. winners/validators/targets) that can be represented in
		/// a page of results.
		type MaxWinnersPerPage: Get<u32>;

		/// Something that can provide the solution data to the verifier.
		///
		/// In reality, this will be fulfilled by the signed phase.
		type SolutionDataProvider: crate::verifier::SolutionDataProvider<Solution = Self::Solution>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T> {
		/// The verification data was unavailable and it could not continue.
		VerificationDataUnavailable,
		/// A verification failed at the given page.
		///
		/// NOTE: if the index is 0, then this could mean either the feasibility of the last page
		/// was wrong, or the final checks of `finalize_verification` failed.
		VerificationFailed(PageIndex, FeasibilityError),
		/// The given page of a solution has been verified, with the given number of winners being
		/// found in it.
		Verified(PageIndex, u32),
		/// A solution with the given score has replaced our current best solution.
		Queued(ElectionScore, Option<ElectionScore>),
	}

	// ---- All storage items about the verifying solution.
	/// A wrapper interface for the storage items related to the queued solution.
	pub struct QueuedSolution<T: Config>(sp_std::marker::PhantomData<T>);
	impl<T: Config> QueuedSolution<T> {
		/// Return the `score` and `winner_count` of verifying solution.
		///
		/// Assumes that all the corresponding pages of `QueuedSolutionBackings` exist, then it
		/// computes the final score of the solution that is currently at the end of its
		/// verification process.
		///
		/// This solution corresponds to whatever is stored in the INVALID variant of
		/// `QueuedSolution`. Recall that the score of this solution is not yet verified, so it
		/// should never become `valid`.
		pub(crate) fn final_score() -> Result<(ElectionScore, u32), FeasibilityError> {
			// ensure that this is only called when all pages are verified individually.
			if QueuedSolutionBackings::<T>::iter_keys().count() != T::Pages::get() as usize {
				return Err(FeasibilityError::Incomplete)
			}

			let mut total_supports: BTreeMap<T::AccountId, PartialBackings> = Default::default();
			// ASSUMPTION: in the staking level, we will eventually collect all exposures, which
			// has the same length as the `total_supports`, but it even has more byte size to it.
			// Thus, this code is 100% safe, but on the staking side there should be caution to
			// make sure exposure collection cannot fail.
			for (who, PartialBackings { backers, total }) in
				QueuedSolutionBackings::<T>::iter().map(|(_, pb)| pb).flatten()
			{
				let mut entry = total_supports.entry(who).or_default();
				entry.total = entry.total.saturating_add(total);
				entry.backers = entry.backers.saturating_add(backers);

				if entry.backers > T::MaxBackersPerWinner::get() {
					return Err(FeasibilityError::TooManyBackings)
				}
			}

			let winner_count = total_supports.len() as u32;
			let score = evaluate_support_core(total_supports.into_iter().map(|(_who, pb)| pb));

			Ok((score, winner_count))
		}

		/// Finalize a correct solution.
		///
		/// Should be called at the end of a verification process, once we are sure that a certain
		/// solution is 100% correct. It stores its score, flips the pointer to it being the current
		/// best one, and clears all the backings.
		///
		/// NOTE: we don't check if this is a better score, the call site must ensure that.
		pub(crate) fn finalize_correct(score: ElectionScore) {
			sublog!(
				info,
				"verifier",
				"finalizing verification a correct solution, replacing old score {:?} with {:?}",
				QueuedSolutionScore::<T>::get(),
				score
			);

			QueuedValidVariant::<T>::mutate(|v| *v = v.other());
			QueuedSolutionScore::<T>::put(score);

			// Clear what was previously the valid variant. Also clears the partial backings.
			Self::clear_invalid_and_backings();
		}

		/// Clear all relevant information of an invalid solution.
		///
		/// Should be called at any step, if we encounter an issue which makes the solution
		/// infeasible.
		pub(crate) fn clear_invalid_and_backings() {
			match Self::invalid() {
				ValidSolution::X => QueuedSolutionX::<T>::remove_all(None),
				ValidSolution::Y => QueuedSolutionY::<T>::remove_all(None),
			};
			QueuedSolutionBackings::<T>::remove_all(None);
			// NOTE: we don't flip the variant, this is still the empty slot.
		}

		/// Clear all relevant information of the valid solution.
		///
		/// This should only be used when we intend to replace the valid solution with something
		/// else (either better, or when being forced).
		pub(crate) fn clear_valid() {
			match Self::valid() {
				ValidSolution::X => QueuedSolutionX::<T>::remove_all(None),
				ValidSolution::Y => QueuedSolutionY::<T>::remove_all(None),
			};
			QueuedSolutionScore::<T>::kill();
		}

		/// Write a single page of a valid solution into the `invalid` variant of the storage.
		///
		/// This should only be called once we are sure that this particular page is 100% correct.
		///
		/// This is called after *a page* has been validated, but the entire solution is not yet
		/// known to be valid. At this stage, we write to the invalid variant. Once all pages are
		/// verified, a call to [`finalize_correct`] will seal the correct pages and flip the
		/// invalid/valid variants.
		pub(crate) fn set_invalid_page(page: PageIndex, supports: SupportsOf<Pallet<T>>) {
			use frame_support::traits::TryCollect;
			let backings: BoundedVec<_, _> = supports
				.iter()
				.map(|(x, s)| (x.clone(), PartialBackings { total: s.total, backers: s.voters.len() as u32 } ))
				.try_collect()
				.expect("`SupportsOf` is bounded by <Pallet<T> as Verifier>::MaxWinnersPerPage, which is assured to be the same as `T::MaxWinnersPerPage` in an integrity test");
			QueuedSolutionBackings::<T>::insert(page, backings);

			match Self::invalid() {
				ValidSolution::X => QueuedSolutionX::<T>::insert(page, supports),
				ValidSolution::Y => QueuedSolutionY::<T>::insert(page, supports),
			}
		}

		/// Forcibly set a valid solution.
		///
		/// Writes all the given pages, and the provided score blindly.
		pub(crate) fn force_set_valid(
			paged_supports: BoundedVec<SupportsOf<Pallet<T>>, T::Pages>,
			score: ElectionScore,
		) {
			for (page_index, supports) in paged_supports.pagify(T::Pages::get()) {
				match Self::valid() {
					ValidSolution::X => QueuedSolutionX::<T>::insert(page_index, supports),
					ValidSolution::Y => QueuedSolutionY::<T>::insert(page_index, supports),
				}
			}
			QueuedSolutionScore::<T>::put(score);
		}

		/// Write a single page to the valid variant directly.
		///
		/// This is not the normal flow of writing, and the solution is not checked.
		///
		/// This is only useful to override the valid solution with a single (likely backup)
		/// solution.
		pub(crate) fn force_set_single_page_valid(
			page: PageIndex,
			supports: SupportsOf<Pallet<T>>,
			score: ElectionScore,
		) {
			// clear everything about valid solutions.
			Self::clear_valid();

			// write a single new page.
			match Self::valid() {
				ValidSolution::X => QueuedSolutionX::<T>::insert(page, supports),
				ValidSolution::Y => QueuedSolutionY::<T>::insert(page, supports),
			}

			// write the score.
			QueuedSolutionScore::<T>::put(score);
		}

		/// Clear all storage items.
		///
		/// Should only be called once everything is done.
		pub(crate) fn kill() {
			QueuedSolutionX::<T>::remove_all(None);
			QueuedSolutionY::<T>::remove_all(None);
			QueuedValidVariant::<T>::kill();
			QueuedSolutionBackings::<T>::remove_all(None);
			QueuedSolutionScore::<T>::kill();
		}

		/// The score of the current best solution, if any.
		pub(crate) fn queued_solution() -> Option<ElectionScore> {
			QueuedSolutionScore::<T>::get()
		}

		/// Get a page of the current queued (aka valid) solution.
		pub(crate) fn get_queued_solution_page(page: PageIndex) -> Option<SupportsOf<Pallet<T>>> {
			match Self::valid() {
				ValidSolution::X => QueuedSolutionX::<T>::get(page),
				ValidSolution::Y => QueuedSolutionY::<T>::get(page),
			}
		}

		fn valid() -> ValidSolution {
			QueuedValidVariant::<T>::get()
		}

		fn invalid() -> ValidSolution {
			QueuedValidVariant::<T>::get().other()
		}

		#[cfg(test)]
		pub(crate) fn valid_iter() -> impl Iterator<Item = (PageIndex, SupportsOf<Pallet<T>>)> {
			match Self::valid() {
				ValidSolution::X => QueuedSolutionX::<T>::iter(),
				ValidSolution::Y => QueuedSolutionY::<T>::iter(),
			}
		}

		#[cfg(any(test, debug_assertions))]
		pub(crate) fn invalid_iter() -> impl Iterator<Item = (PageIndex, SupportsOf<Pallet<T>>)> {
			match Self::invalid() {
				ValidSolution::X => QueuedSolutionX::<T>::iter(),
				ValidSolution::Y => QueuedSolutionY::<T>::iter(),
			}
		}

		#[cfg(test)]
		pub(crate) fn get_invalid_page(page: PageIndex) -> Option<SupportsOf<Pallet<T>>> {
			match Self::invalid() {
				ValidSolution::X => QueuedSolutionX::<T>::get(page),
				ValidSolution::Y => QueuedSolutionY::<T>::get(page),
			}
		}

		#[cfg(test)]
		pub(crate) fn get_valid_page(page: PageIndex) -> Option<SupportsOf<Pallet<T>>> {
			match Self::valid() {
				ValidSolution::X => QueuedSolutionX::<T>::get(page),
				ValidSolution::Y => QueuedSolutionY::<T>::get(page),
			}
		}

		#[cfg(test)]
		pub(crate) fn get_backing_page(
			page: PageIndex,
		) -> Option<BoundedVec<(T::AccountId, PartialBackings), T::MaxWinnersPerPage>> {
			QueuedSolutionBackings::<T>::get(page)
		}

		#[cfg(test)]
		pub(crate) fn backing_iter() -> impl Iterator<
			Item = (PageIndex, BoundedVec<(T::AccountId, PartialBackings), T::MaxWinnersPerPage>),
		> {
			QueuedSolutionBackings::<T>::iter()
		}

		/// Ensure that all the storage items managed by this struct are in `kill` state, meaning
		/// that in the expect state after an election is OVER.
		#[cfg(any(test, debug_assertions))]
		pub(crate) fn ensure_killed() {
			use frame_support::assert_storage_noop;
			assert_storage_noop!(Self::kill());
		}
	}

	/// The `X` variant of the current queued solution. Might be the valid one or not.
	///
	/// The two variants of this storage item is to avoid the need of copying. Recall that once a
	/// `VerifyingSolution` is being processed, it needs to write its partial supports *somewhere*.
	/// Writing theses supports on top of a *good* queued supports is wrong, since we might bail.
	/// Writing them to a bugger and copying at the ned is slightly better, but expensive. This flag
	/// system is best of both worlds.
	#[pallet::storage]
	type QueuedSolutionX<T: Config> = StorageMap<_, Twox64Concat, PageIndex, SupportsOf<Pallet<T>>>;
	#[pallet::storage]
	/// The `Y` variant of the current queued solution. Might be the valid one or not.
	type QueuedSolutionY<T: Config> = StorageMap<_, Twox64Concat, PageIndex, SupportsOf<Pallet<T>>>;
	/// Pointer to the variant of [`QueuedSolutionX`] or [`QueuedSolutionY`] that is currently
	/// valid.
	#[pallet::storage]
	type QueuedValidVariant<T: Config> = StorageValue<_, ValidSolution, ValueQuery>;
	/// The `(amount, count)` of backings, divided per page.
	///
	/// This is stored because in the last block of verification we need them to compute the score,
	/// and check `MaxBackersPerWinner`.
	///
	/// This can only ever live for the invalid variant of the solution. Once it is valid, we don't
	/// need this information anymore; the score is already computed once in
	/// [`QueuedSolutionScore`], and the backing counts are checked.
	#[pallet::storage]
	type QueuedSolutionBackings<T: Config> = StorageMap<
		_,
		Twox64Concat,
		PageIndex,
		BoundedVec<(T::AccountId, PartialBackings), T::MaxWinnersPerPage>,
	>;

	/// The score of the valid variant of [`QueuedSolution`].
	///
	/// This only ever lives for the `valid` variant.
	#[pallet::storage]
	type QueuedSolutionScore<T: Config> = StorageValue<_, ElectionScore>;

	/// The minimum score that each solution must attain in order to be considered feasible.
	#[pallet::storage]
	#[pallet::getter(fn minimum_score)]
	pub(crate) type MinimumScore<T: Config> = StorageValue<_, ElectionScore>;

	#[pallet::storage]
	#[pallet::getter(fn status_storage)]
	pub(crate) type StatusStorage<T: Config> = StorageValue<_, Status, ValueQuery>;

	#[pallet::pallet]
	#[pallet::generate_storage_info]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::call]
	impl<T: Config> Pallet<T> {}

	#[pallet::hooks]
	impl<T: Config> Hooks<T::BlockNumber> for Pallet<T> {
		fn integrity_test() {
			// ensure that we have funneled some of our type parameters EXACTLY as-is to the
			// verifier pallet.
			assert_eq!(T::MaxWinnersPerPage::get(), <Self as Verifier>::MaxWinnersPerPage::get());
			assert_eq!(
				T::MaxBackersPerWinner::get(),
				<Self as Verifier>::MaxBackersPerWinner::get()
			);
		}

		fn on_initialize(_n: T::BlockNumber) -> Weight {
			Self::do_on_initialize()
		}
	}
}

impl<T: Config> Pallet<T> {
	fn do_on_initialize() -> Weight {
		if let Status::Ongoing(current_page) = Self::status_storage() {
			let maybe_page_solution =
				<T::SolutionDataProvider as SolutionDataProvider>::get_page(current_page);

			if maybe_page_solution.is_none() {
				// the data provider has zilch, revert to a clean state, waiting for a new `start`.
				sublog!(
					error,
					"verifier",
					"T::SolutionDataProvider failed to deliver page {}. This is an expected error and should not happen.",
					current_page,
				);
				QueuedSolution::<T>::clear_invalid_and_backings();
				StatusStorage::<T>::put(Status::Nothing);
				T::SolutionDataProvider::report_result(VerificationResult::DataUnavailable);
				Self::deposit_event(Event::<T>::VerificationDataUnavailable);

				return 0
			}

			let page_solution = maybe_page_solution.expect("Option checked to not be None; qed");
			let maybe_supports = Self::feasibility_check_page_inner(page_solution, current_page);

			sublog!(
				debug,
				"verifier",
				"verified page {} of a solution, outcome = {:?}",
				current_page,
				maybe_supports.as_ref().map(|s| s.len())
			);

			match maybe_supports {
				Ok(supports) => {
					Self::deposit_event(Event::<T>::Verified(current_page, supports.len() as u32));
					QueuedSolution::<T>::set_invalid_page(current_page, supports);

					if current_page > crate::Pallet::<T>::lsp() {
						// not last page, just tick forward.
						StatusStorage::<T>::put(Status::Ongoing(current_page.saturating_sub(1)));
					} else {
						// last page, finalize everything.
						let claimed_score = T::SolutionDataProvider::get_score().unwrap(); // TODO: defensive unwrap or default

						// in both cases of the following match, we are not back to the nothing
						// state.
						StatusStorage::<T>::put(Status::Nothing);

						match Self::finalize_async_verification(claimed_score) {
							Ok(_) => {
								T::SolutionDataProvider::report_result(VerificationResult::Queued);
							},
							Err(err) =>
								T::SolutionDataProvider::report_result(VerificationResult::Rejected),
						}
					}
				},
				Err(err) => {
					// the page solution was invalid.
					Self::deposit_event(Event::<T>::VerificationFailed(current_page, err));
					StatusStorage::<T>::put(Status::Nothing);
					QueuedSolution::<T>::clear_invalid_and_backings();
					T::SolutionDataProvider::report_result(VerificationResult::Rejected)
				},
			}
		}

		0
	}

	/// This should only be called when all pages of an async verification are done.
	///
	/// Returns `Ok()` if everything is okay, at which point the valid variant of the queued
	/// solution will be updated. Returns
	/// `Err(Feasibility)` if any of the last verification steps fail.
	fn finalize_async_verification(claimed_score: ElectionScore) -> Result<(), FeasibilityError> {
		let outcome = QueuedSolution::<T>::final_score()
			.and_then(|(final_score, winner_count)| {
				let desired_targets = crate::Snapshot::<T>::desired_targets().unwrap();
				// claimed_score checked prior in seal_unverified_solution
				match (final_score == claimed_score, winner_count == desired_targets) {
					(true, true) => {
						// all good, finalize this solution
						// NOTE: must be before the call to `finalize_correct`.
						Self::deposit_event(Event::<T>::Queued(
							final_score,
							QueuedSolution::<T>::queued_solution(),
						));
						QueuedSolution::<T>::finalize_correct(final_score);
						Ok(())
					},
					(false, true) => Err(FeasibilityError::InvalidScore),
					(true, false) => Err(FeasibilityError::WrongWinnerCount),
					(false, false) => Err(FeasibilityError::InvalidScore),
				}
			})
			.map_err(|err| {
				sublog!(warn, "verifier", "Finalizing solution was invalid due to {:?}.", err);
				// In case of any of the errors, kill the solution.
				QueuedSolution::<T>::clear_invalid_and_backings();
				// and deposit an event about it.
				Self::deposit_event(Event::<T>::VerificationFailed(0, err.clone()));
				err
			});
		sublog!(debug, "verifier", "finalize verification outcome: {:?}", outcome);
		outcome
	}

	/// Ensure that the given score is:
	///
	/// - better than the queued solution, if one exists.
	/// - greater than the minimum untrusted score.
	pub(crate) fn ensure_score_quality(score: ElectionScore) -> Result<(), FeasibilityError> {
		let is_improvement = <Self as Verifier>::queued_solution().map_or(true, |best_score| {
			sp_npos_elections::is_score_better::<sp_runtime::Perbill>(
				score,
				best_score,
				T::SolutionImprovementThreshold::get(),
			)
		});
		ensure!(is_improvement, FeasibilityError::ScoreTooLow);

		let is_greater_than_min_trusted = Self::minimum_score().map_or(true, |min_score| {
			sp_npos_elections::is_score_better(score, min_score, sp_runtime::Perbill::zero())
		});
		ensure!(is_greater_than_min_trusted, FeasibilityError::ScoreTooLow);

		Ok(())
	}

	/// Do the full feasibility check:
	///
	/// - check all edges.
	/// - checks `MaxBackersPerWinner` to be respected IN THIS PAGE.
	/// - checks the number of winners to be less than or equal to `DesiredTargets` IN THIS PAGE
	///   ONLY.
	pub(super) fn feasibility_check_page_inner(
		partial_solution: SolutionOf<T>,
		page: PageIndex,
	) -> Result<SupportsOf<Self>, FeasibilityError> {
		// Read the corresponding snapshots.
		let snapshot_targets =
			crate::Snapshot::<T>::targets().ok_or(FeasibilityError::SnapshotUnavailable)?;
		let snapshot_voters =
			crate::Snapshot::<T>::voters(page).ok_or(FeasibilityError::SnapshotUnavailable)?;

		// ----- Start building. First, we need some closures.
		let cache = helpers::generate_voter_cache::<T, _>(&snapshot_voters);
		let voter_at = helpers::voter_at_fn::<T>(&snapshot_voters);
		let target_at = helpers::target_at_fn::<T>(&snapshot_targets);
		let voter_index = helpers::voter_index_fn_usize::<T>(&cache);

		// Then convert solution -> assignment. This will fail if any of the indices are
		// gibberish.
		let assignments = partial_solution
			.into_assignment(voter_at, target_at)
			.map_err::<FeasibilityError, _>(Into::into)?;

		// Ensure that assignments are all correct.
		let _ = assignments
			.iter()
			.map(|ref assignment| {
				// Check that assignment.who is actually a voter (defensive-only). NOTE: while
				// using the index map from `voter_index` is better than a blind linear search,
				// this *still* has room for optimization. Note that we had the index when we
				// did `solution -> assignment` and we lost it. Ideal is to keep the index
				// around.

				// Defensive-only: must exist in the snapshot.
				let snapshot_index =
					voter_index(&assignment.who).ok_or(FeasibilityError::InvalidVoter)?;
				// Defensive-only: index comes from the snapshot, must exist.
				let (_voter, _stake, targets) =
					snapshot_voters.get(snapshot_index).ok_or(FeasibilityError::InvalidVoter)?;
				debug_assert!(*_voter == assignment.who);

				// Check that all of the targets are valid based on the snapshot.
				if assignment.distribution.iter().any(|(t, _)| !targets.contains(t)) {
					return Err(FeasibilityError::InvalidVote)
				}
				Ok(())
			})
			.collect::<Result<(), FeasibilityError>>()?;

		// ----- Start building support. First, we need one more closure.
		let stake_of = helpers::stake_of_fn::<T, _>(&snapshot_voters, &cache);

		// This might fail if the normalization fails. Very unlikely. See `integrity_test`.
		let staked_assignments =
			sp_npos_elections::assignment_ratio_to_staked_normalized(assignments, stake_of)
				.map_err::<FeasibilityError, _>(Into::into)?;

		let supports = sp_npos_elections::to_supports(&staked_assignments);

		// Check the maximum number of backers per winner. If this is a single-page solution, this
		// is enough to check `MaxBackersPerWinner`. Else, this is just a heuristic, and needs to be
		// checked again at the end (via `QueuedSolutionBackings`).
		ensure!(
			supports
				.iter()
				.all(|(_, s)| (s.voters.len() as u32) <= T::MaxBackersPerWinner::get()),
			FeasibilityError::TooManyBackings
		);

		// Ensure some heuristics. These conditions must hold in the **entire** support, this is
		// just a single page. But, they must hold in a single page as well.
		let desired_targets =
			crate::Snapshot::<T>::desired_targets().ok_or(FeasibilityError::SnapshotUnavailable)?;
		ensure!((supports.len() as u32) <= desired_targets, FeasibilityError::WrongWinnerCount);

		// almost-defensive-only: `MaxBackersPerWinner` is already checked. A sane value of
		// `MaxWinnersPerPage` should be more than any possible value of `desired_targets()`, which
		// is ALSO checked, so this conversion can almost never fail.
		let bounded_supports = supports.try_into_bounded_supports().map_err(|_| {
			debug_assert!(false);
			FeasibilityError::WrongWinnerCount
		})?;
		Ok(bounded_supports)
	}

	#[cfg(test)]
	pub(crate) fn sanity_check() -> Result<(), &'static str> {
		Ok(())
	}
}

impl<T: Config> Verifier for Pallet<T> {
	type AccountId = T::AccountId;
	type Solution = SolutionOf<T>;
	type MaxBackersPerWinner = T::MaxBackersPerWinner;
	type MaxWinnersPerPage = T::MaxWinnersPerPage;

	fn set_minimum_score(score: ElectionScore) {
		MinimumScore::<T>::put(score);
	}

	fn ensure_claimed_score_improves(claimed_score: ElectionScore) -> bool {
		Self::ensure_score_quality(claimed_score).is_ok()
	}

	fn queued_solution() -> Option<ElectionScore> {
		QueuedSolution::<T>::queued_solution()
	}

	fn kill() {
		QueuedSolution::<T>::kill();
		<StatusStorage<T>>::put(Status::Nothing);
	}

	fn get_queued_solution_page(page: PageIndex) -> Option<SupportsOf<Self>> {
		QueuedSolution::<T>::get_queued_solution_page(page)
	}

	fn verify_synchronous(
		partial_solution: Self::Solution,
		claimed_score: ElectionScore,
		page: PageIndex,
	) -> Result<SupportsOf<Self>, FeasibilityError> {
		// first, ensure this score will be good enough, even if valid..
		let _ = Self::ensure_score_quality(claimed_score)?;

		// then actually check feasibility...
		// NOTE: `MaxBackersPerWinner` is also already checked here.
		let supports = Self::feasibility_check_page_inner(partial_solution, page)?;

		// then check that the number of winners was exactly enough..
		let desired_targets =
			crate::Snapshot::<T>::desired_targets().ok_or(FeasibilityError::SnapshotUnavailable)?;
		ensure!(supports.len() as u32 == desired_targets, FeasibilityError::WrongWinnerCount);

		// then check the score was truth..
		let truth_score = supports.evaluate();
		ensure!(truth_score == claimed_score, FeasibilityError::InvalidScore);

		// and finally queue the solution.
		QueuedSolution::<T>::force_set_single_page_valid(0, supports.clone(), truth_score);

		Ok(supports)
	}

	fn feasibility_check_page(
		partial_solution: Self::Solution,
		page: PageIndex,
	) -> Result<SupportsOf<Self>, FeasibilityError> {
		Self::feasibility_check_page_inner(partial_solution, page)
	}
}

impl<T: Config> AsynchronousVerifier for Pallet<T> {
	type SolutionDataProvider = T::SolutionDataProvider;

	fn status() -> Status {
		Pallet::<T>::status_storage()
	}

	fn start() {
		if let Status::Nothing = Self::status() {
			let claimed_score = Self::SolutionDataProvider::get_score().unwrap_or_default();
			if Self::ensure_score_quality(claimed_score).is_err() {
				// don't do anything, report back that this solution was garbage.
				Self::deposit_event(Event::<T>::VerificationFailed(
					crate::Pallet::<T>::msp(),
					FeasibilityError::ScoreTooLow,
				));
				T::SolutionDataProvider::report_result(VerificationResult::Rejected)
			} else {
				StatusStorage::<T>::put(Status::Ongoing(crate::Pallet::<T>::msp()));
			}
		} else {
			sublog!(warn, "verifier", "start signal received while busy. This will be ignored.");
		}
	}

	fn stop() {
		sublog!(warn, "verifier", "stop signal received. clearing everything.");

		// we clear any ongoing solution's no been verified in any case, although this should only
		// exist if we were doing something.
		debug_assert!(
			!matches!(StatusStorage::<T>::get(), Status::Ongoing(_)) ||
				(matches!(StatusStorage::<T>::get(), Status::Ongoing(_)) &&
					QueuedSolution::<T>::invalid_iter().count() > 0)
		);
		QueuedSolution::<T>::clear_invalid_and_backings();

		// we also mutate the status back to doing nothing.
		StatusStorage::<T>::mutate(|old| {
			if matches!(old, Status::Ongoing(_)) {
				T::SolutionDataProvider::report_result(VerificationResult::Rejected)
			}
			*old = Status::Nothing;
		});
	}
}