// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

#![recursion_limit = "128"]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(all(feature = "bench", test), feature(test))]
#![feature(drain_filter)]

#[cfg(all(feature = "bench", test))]
extern crate test;

use codec::{Decode, Encode, HasCompact};
use rstd::{prelude::*, result};
use session::{historical::OnSessionEnding, SelectInitialValidators};
use sr_primitives::traits::{Bounded, CheckedSub, Convert, One, SaturatedConversion, Saturating, StaticLookup, Zero};
#[cfg(feature = "std")]
use sr_primitives::{Deserialize, Serialize};
use sr_primitives::{Perbill, Perquintill, RuntimeDebug};

use sr_staking_primitives::{
	offence::{Offence, OffenceDetails, OnOffenceHandler, ReportOffence},
	SessionIndex,
};

use srml_support::{
	decl_event, decl_module, decl_storage, ensure,
	traits::{
		Currency, Get, Imbalance, LockIdentifier, LockableCurrency, OnFreeBalanceZero, OnUnbalanced, Time,
		WithdrawReason, WithdrawReasons,
	},
};
use system::{ensure_root, ensure_signed};

use phragmen::{elect, equalize, ExtendedBalance, PhragmenStakedAssignment, Support, SupportMap};

mod utils;

#[allow(unused)]
#[cfg(any(feature = "bench", test))]
mod mock;
//
#[cfg(test)]
mod tests;

mod phragmen;

//#[cfg(all(feature = "bench", test))]
//mod benches;

const RECENT_OFFLINE_COUNT: usize = 32;
const DEFAULT_MINIMUM_VALIDATOR_COUNT: u32 = 4;
const MAX_NOMINATIONS: usize = 16;
const MAX_UNSTAKE_THRESHOLD: u32 = 10;
const MAX_UNLOCKING_CHUNKS: usize = 32;
const MONTH_IN_SECONDS: u32 = 2_592_000;
const STAKING_ID: LockIdentifier = *b"staking ";

/// Counter for the number of eras that have passed.
pub type EraIndex = u32;

#[derive(RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub enum StakerStatus<AccountId> {
	/// Chilling.
	Idle,
	/// Declared desire in validating or already participating in it.
	Validator,
	/// Nominating for a group of other stakers.
	Nominator(Vec<AccountId>),
}

#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
pub struct ValidatorPrefs {
	/// Validator should ensure this many more slashes than is necessary before being unstaked.
	#[codec(compact)]
	pub unstake_threshold: u32,
	/// percent of Reward that validator takes up-front; only the rest is split between themselves and
	/// nominators.
	pub validator_payment_ratio: Perbill,
}

impl Default for ValidatorPrefs {
	fn default() -> Self {
		ValidatorPrefs {
			unstake_threshold: 3,
			validator_payment_ratio: Default::default(),
		}
	}
}

#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
pub enum StakingBalance<RingBalance, KtonBalance> {
	Ring(RingBalance),
	Kton(KtonBalance),
}

impl<RingBalance: Default, KtonBalance: Default> Default for StakingBalance<RingBalance, KtonBalance> {
	fn default() -> Self {
		StakingBalance::Ring(Default::default())
	}
}

/// A destination account for payment.
#[derive(PartialEq, Eq, Copy, Clone, Encode, Decode, RuntimeDebug)]
pub enum RewardDestination {
	/// Pay into the stash account, increasing the amount at stake accordingly.
	/// for now, we don't use this.
	//    DeprecatedStaked,
	/// Pay into the stash account, not increasing the amount at stake.
	Stash,
	/// Pay into the controller account.
	Controller,
}

impl Default for RewardDestination {
	fn default() -> Self {
		RewardDestination::Stash
	}
}

#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
pub struct UnlockChunk<StakingBalance> {
	/// Amount of funds to be unlocked.
	value: StakingBalance,
	/// Era number at which point it'll be unlocked.
	#[codec(compact)]
	era: EraIndex,
}

#[derive(PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug)]
pub struct TimeDepositItem<RingBalance: HasCompact, Moment> {
	#[codec(compact)]
	value: RingBalance,
	#[codec(compact)]
	start_time: Moment,
	#[codec(compact)]
	expire_time: Moment,
}

#[derive(PartialEq, Eq, Default, Clone, Encode, Decode, RuntimeDebug)]
pub struct StakingLedgers<AccountId, RingBalance: HasCompact, KtonBalance: HasCompact, StakingBalance, Moment> {
	/// The stash account whose balance is actually locked and at stake.
	pub stash: AccountId,

	/// The total amount of the stash's balance that we are currently accounting for.
	/// It's just `active` plus all the `unlocking` balances.
	/// active_ring = normal_ring + time_deposit_ring
	#[codec(compact)]
	pub total_ring: RingBalance,
	/// The total amount of the stash's balance that will be at stake in any forthcoming
	/// rounds.
	#[codec(compact)]
	pub active_ring: RingBalance,
	// active time-deposit ring
	#[codec(compact)]
	pub active_deposit_ring: RingBalance,

	/// The total amount of the stash's balance that we are currently accounting for.
	/// It's just `active` plus all the `unlocking` balances.
	#[codec(compact)]
	pub total_kton: KtonBalance,
	/// The total amount of the stash's balance that will be at stake in any forthcoming
	/// rounds.
	#[codec(compact)]
	pub active_kton: KtonBalance,
	// time-deposit items:
	// if you deposit ring for a minimum period,
	// you can get KTON as bonus
	// which can also be used for staking
	pub deposit_items: Vec<TimeDepositItem<RingBalance, Moment>>,

	/// Any balance that is becoming free, which may eventually be transferred out
	/// of the stash (assuming it doesn't get slashed first).
	pub unlocking: Vec<UnlockChunk<StakingBalance>>,
}

/// The amount of exposure (to slashing) than an individual nominator has.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, RuntimeDebug)]
pub struct IndividualExposure<AccountId, Power> {
	/// The stash account of the nominator in question.
	who: AccountId,
	/// Amount of funds exposed.
	value: Power,
}

/// A snapshot of the stake backing a single validator in the system.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, Default, RuntimeDebug)]
pub struct Exposure<AccountId, Power> {
	/// The total balance backing this validator.
	pub total: Power,
	/// The validator's own stash that is exposed.
	pub own: Power,
	/// The portions of nominators stashes that are exposed.
	pub others: Vec<IndividualExposure<AccountId, Power>>,
}

/// A slashing event occurred, slashing a validator for a given amount of balance.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Encode, Decode, Default, RuntimeDebug)]
pub struct SlashJournalEntry<AccountId, Balance: HasCompact> {
	who: AccountId,
	amount: Balance,
	own_slash: Balance, // the amount of `who`'s own exposure that was slashed
}

type RingBalanceOf<T> = <<T as Trait>::Ring as Currency<<T as system::Trait>::AccountId>>::Balance;
type KtonBalanceOf<T> = <<T as Trait>::Kton as Currency<<T as system::Trait>::AccountId>>::Balance;

// for ring
type RingPositiveImbalanceOf<T> = <<T as Trait>::Ring as Currency<<T as system::Trait>::AccountId>>::PositiveImbalance;
type RingNegativeImbalanceOf<T> = <<T as Trait>::Ring as Currency<<T as system::Trait>::AccountId>>::NegativeImbalance;

// for kton
type KtonPositiveImbalanceOf<T> = <<T as Trait>::Kton as Currency<<T as system::Trait>::AccountId>>::PositiveImbalance;
type KtonNegativeImbalanceOf<T> = <<T as Trait>::Kton as Currency<<T as system::Trait>::AccountId>>::NegativeImbalance;

type MomentOf<T> = <<T as Trait>::Time as Time>::Moment;

pub trait SessionInterface<AccountId>: system::Trait {
	/// Disable a given validator by stash ID.
	///
	/// Returns `true` if new era should be forced at the end of this session.
	/// This allows preventing a situation where there is too many validators
	/// disabled and block production stalls.
	fn disable_validator(validator: &AccountId) -> Result<bool, ()>;
	/// Get the validators from session.
	fn validators() -> Vec<AccountId>;
	/// Prune historical session tries up to but not including the given index.
	fn prune_historical_up_to(up_to: SessionIndex);
}

impl<T: Trait> SessionInterface<<T as system::Trait>::AccountId> for T
where
	T: session::Trait<ValidatorId = <T as system::Trait>::AccountId>,
	T: session::historical::Trait<
		FullIdentification = Exposure<<T as system::Trait>::AccountId, ExtendedBalance>,
		FullIdentificationOf = ExposureOf<T>,
	>,
	T::SessionHandler: session::SessionHandler<<T as system::Trait>::AccountId>,
	T::OnSessionEnding: session::OnSessionEnding<<T as system::Trait>::AccountId>,
	T::SelectInitialValidators: session::SelectInitialValidators<<T as system::Trait>::AccountId>,
	T::ValidatorIdOf: Convert<<T as system::Trait>::AccountId, Option<<T as system::Trait>::AccountId>>,
{
	fn disable_validator(validator: &<T as system::Trait>::AccountId) -> Result<bool, ()> {
		<session::Module<T>>::disable(validator)
	}
	fn validators() -> Vec<<T as system::Trait>::AccountId> {
		<session::Module<T>>::validators()
	}

	fn prune_historical_up_to(up_to: SessionIndex) {
		<session::historical::Module<T>>::prune_up_to(up_to);
	}
}

pub trait Trait: timestamp::Trait + session::Trait {
	type Ring: LockableCurrency<Self::AccountId, Moment = Self::BlockNumber>;
	type Kton: LockableCurrency<Self::AccountId, Moment = Self::BlockNumber>;

	/// Time used for computing era duration.
	type Time: Time;

	type CurrencyToVote: Convert<KtonBalanceOf<Self>, u64> + Convert<u128, KtonBalanceOf<Self>>;

	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;

	/// Handler for the unbalanced reduction when slashing a staker.
	type RingSlash: OnUnbalanced<RingNegativeImbalanceOf<Self>>;

	/// Handler for the unbalanced increment when rewarding a staker.
	type RingReward: OnUnbalanced<RingPositiveImbalanceOf<Self>>;

	type KtonSlash: OnUnbalanced<KtonNegativeImbalanceOf<Self>>;
	type KtonReward: OnUnbalanced<KtonPositiveImbalanceOf<Self>>;

	/// Number of sessions per era.
	type SessionsPerEra: Get<SessionIndex>;

	/// Number of eras that staked funds must remain bonded for.
	type BondingDuration: Get<EraIndex>;

	// custom
	type Cap: Get<<Self::Ring as Currency<Self::AccountId>>::Balance>;
	type ErasPerEpoch: Get<EraIndex>;
	// TODO: move it to sesions module later
	type SessionLength: Get<Self::BlockNumber>;
	/// Interface for interacting with a session module.
	type SessionInterface: self::SessionInterface<Self::AccountId>;
}

/// Mode of era-forcing.
#[derive(Copy, Clone, PartialEq, Eq, Encode, Decode, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub enum Forcing {
	/// Not forcing anything - just let whatever happen.
	NotForcing,
	/// Force a new era, then reset to `NotForcing` as soon as it is done.
	ForceNew,
	/// Avoid a new era indefinitely.
	ForceNone,
	/// Force a new era at the end of all sessions indefinitely.
	ForceAlways,
}

impl Default for Forcing {
	fn default() -> Self {
		Forcing::NotForcing
	}
}

decl_storage! {
	trait Store for Module<T: Trait> as Staking {

		pub ValidatorCount get(validator_count) config(): u32;

		pub MinimumValidatorCount get(minimum_validator_count) config():
			u32 = DEFAULT_MINIMUM_VALIDATOR_COUNT;

		/// Any validators that may never be slashed or forcibly kicked. It's a Vec since they're
		/// easy to initialize and the performance hit is minimal (we expect no more than four
		/// invulnerables) and restricted to testnets.
		pub Invulnerables get(fn invulnerables) config(): Vec<T::AccountId>;

		pub SessionReward get(session_reward) config(): Perbill = Perbill::from_percent(60);

		pub Bonded get(bonded): map T::AccountId => Option<T::AccountId>;

		pub Ledger get(ledger): map T::AccountId => Option<StakingLedgers<
			T::AccountId, RingBalanceOf<T>, KtonBalanceOf<T>, StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
			T::Moment>>;

		pub Payee get(payee): map T::AccountId => RewardDestination;

		pub Validators get(validators): linked_map T::AccountId => ValidatorPrefs;

		pub Nominators get(nominators): linked_map T::AccountId => Vec<T::AccountId>;

		pub Stakers get(stakers): map T::AccountId => Exposure<T::AccountId, ExtendedBalance>;

		pub CurrentElected get(current_elected): Vec<T::AccountId>;

		pub CurrentEra get(current_era) config(): EraIndex;

		/// The start of the current era.
		pub CurrentEraStart get(fn current_era_start): MomentOf<T>;

		/// The session index at which the current era started.
		pub CurrentEraStartSessionIndex get(fn current_era_start_session_index): SessionIndex;

		/// Rewards for the current era. Using indices of current elected set.
		CurrentEraPointsEarned get(fn current_era_reward): EraPoints;

		pub SlotStake get(slot_stake): ExtendedBalance;

		/// True if the next session change will be a new era regardless of index.
		pub ForceEra get(fn force_era) config(): Forcing;

		/// The percentage of the slash that is distributed to reporters.
		///
		/// The rest of the slashed value is handled by the `Slash`.
		pub SlashRewardFraction get(fn slash_reward_fraction) config(): Perbill;

		pub EpochIndex get(epoch_index): u32 = 0;

		/// The accumulated reward for the current era. Reset to zero at the beginning of the era
		/// and increased for every successfully finished session.
		pub CurrentEraTotalReward get(current_era_total_reward) config(): RingBalanceOf<T>;

		/// All slashes that have occurred in a given era.
		EraSlashJournal get(fn era_slash_journal):
			map EraIndex => Vec<SlashJournalEntry<T::AccountId, BalanceOf<T>>>;

		pub NodeName get(node_name): map T::AccountId => Vec<u8>;

		pub RingPool get(ring_pool): RingBalanceOf<T>;

		pub KtonPool get(kton_pool): KtonBalanceOf<T>;

	}
	add_extra_genesis {
		config(stakers):
			Vec<(T::AccountId, T::AccountId, RingBalanceOf<T>, StakerStatus<T::AccountId>)>;
		build(| config: &GenesisConfig<T>| {
				for &(ref stash, ref controller, balance, ref status) in &config.stakers {
					assert!(T::Ring::free_balance(&stash) >= balance);
					let _ = <Module<T>>::bond(
						T::Origin::from(Some(stash.clone()).into()),
						T::Lookup::unlookup(controller.clone()),
						StakingBalance::Ring(balance),
						RewardDestination::Stash,
						12
					);
					let _ = match status {
						StakerStatus::Validator => {
							<Module<T>>::validate(
								T::Origin::from(Some(controller.clone()).into()),
								[0;8].to_vec(),
								0,
								3
							)
						},
						StakerStatus::Nominator(votes) => {
							<Module<T>>::nominate(
								T::Origin::from(Some(controller.clone()).into()),
								votes.iter().map(|l| {T::Lookup::unlookup(l.clone())}).collect()
							)
						}, _ => Ok(())
					};
				}
			});
	}
}

decl_event!(
    pub enum Event<T> where Balance = RingBalanceOf<T>, <T as system::Trait>::AccountId {
        /// All validators have been rewarded by the given balance.
		Reward(Balance),
		/// One validator (and its nominators) has been given an offline-warning (it is still
		/// within its grace). The accrued number of slashes is recorded, too.
		OfflineWarning(AccountId, u32),
		/// One validator (and its nominators) has been slashed by the given ratio.
		OfflineSlash(AccountId, u32),
		/// NodeName changed
	    NodeNameUpdated,
    }
);

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		/// Number of sessions per era.
		const SessionsPerEra: SessionIndex = T::SessionsPerEra::get();

		/// Number of eras that staked funds must remain bonded for.
		const BondingDuration: EraIndex = T::BondingDuration::get();

		const SessionLength: T::BlockNumber = T::SessionLength::get();

		fn deposit_event() = default;

		fn bond(origin,
			controller: <T::Lookup as StaticLookup>::Source,
			value: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
			payee: RewardDestination,
			promise_month: u32
		) {
			let stash = ensure_signed(origin)?;
			ensure!( promise_month <= 36, "months at most is 36.");

			if <Bonded<T>>::exists(&stash) {
				return Err("stash already bonded")
			}

			let controller = T::Lookup::lookup(controller)?;

			if <Ledger<T>>::exists(&controller) {
				return Err("controller already paired")
			}

			<Bonded<T>>::insert(&stash, &controller);
			<Payee<T>>::insert(&stash, payee);

			let ledger = StakingLedgers {stash: stash.clone(), ..Default::default()};
			match value {
				StakingBalance::Ring(r) => {
					let stash_balance = T::Ring::free_balance(&stash);
					let value = r.min(stash_balance);
					// increase ring pool
					<RingPool<T>>::mutate(|r| *r += value);
					Self::bond_helper_in_ring(&stash, &controller, value, promise_month, ledger);
				},
				StakingBalance::Kton(k) => {
					let stash_balance = T::Kton::free_balance(&stash);
					let value: KtonBalanceOf<T> = k.min(stash_balance);
					// increase kton pool
					<KtonPool<T>>::mutate(|k| *k += value);
					Self::bond_helper_in_kton(&controller, value, ledger);
				},
			}
		}

		fn bond_extra(origin,
			value: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
			promise_month: u32
		) {
			let stash = ensure_signed(origin)?;
			ensure!( promise_month <= 36, "months at most is 36.");
			let controller = Self::bonded(&stash).ok_or("not a stash")?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			match value {
				 StakingBalance::Ring(r) => {
					let stash_balance = T::Ring::free_balance(&stash);
					if let Some(extra) = stash_balance.checked_sub(&ledger.total_ring) {
						let extra = extra.min(r);
						<RingPool<T>>::mutate(|r| *r += extra);
						Self::bond_helper_in_ring(&stash, &controller, extra, promise_month, ledger);
					}
				},
				StakingBalance::Kton(k) => {
					let stash_balance = T::Kton::free_balance(&stash);
					if let Some(extra) = stash_balance.checked_sub(&ledger.total_kton) {
						let extra = extra.min(k);
						<KtonPool<T>>::mutate(|k| *k += extra);
						Self::bond_helper_in_kton(&controller, extra, ledger);
					}
				},
			}
		}

		/// for normal_ring or normal_kton, follow the original substrate pattern
		/// for time_deposit_ring, transform it into normal_ring first
		/// modify time_deposit_items and time_deposit_ring amount
		fn unbond(origin, value: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>) {
			let controller = ensure_signed(origin)?;

			Self::clear_mature_deposits(&controller);

			let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let StakingLedgers {
				active_ring,
				active_deposit_ring,
				active_kton,
				unlocking,
				..
			} = &mut ledger;

			ensure!(
				unlocking.len() < MAX_UNLOCKING_CHUNKS,
				"can not schedule more unlock chunks"
			);

			let era = Self::current_era() + T::BondingDuration::get();

			match value {
				StakingBalance::Ring(r) => {
					// total_active_ring = normal_ring + time_deposit_ring
					// Only active normal ring can be unbond
					let active_normal_ring = *active_ring - *active_deposit_ring;
					// unbond normal ring first
					let available_unbund_ring = r.min(active_normal_ring);

					<RingPool<T>>::mutate(|r| *r -= available_unbund_ring);

					if !available_unbund_ring.is_zero() {
						*active_ring -= available_unbund_ring;
						unlocking.push(UnlockChunk {
							value: StakingBalance::Ring(available_unbund_ring),
							era,
						});

						Self::update_ledger(&controller, &ledger, value);
					}
				},
				StakingBalance::Kton(k) => {
					let unbound_kton = k.min(*active_kton);

					if !unbound_kton.is_zero() {
						<KtonPool<T>>::mutate(|k| *k -= unbound_kton);

						*active_kton -= unbound_kton;

						unlocking.push(UnlockChunk {
							value: StakingBalance::Kton(unbound_kton),
							era,
						});

						Self::update_ledger(&controller, &ledger, value);
					}
				},
			}
		}

		/// called by controller
		fn deposit_extra(origin, value: RingBalanceOf<T>, promise_month: u32) {
			let controller = ensure_signed(origin)?;

			ensure!( promise_month <= 36, "months at most is 36.");
			let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let StakingLedgers {
				active_ring,
				active_deposit_ring,
				deposit_items,
				stash,
				..
			} = &mut ledger;

			Self::clear_mature_deposits(&controller);

			let value = value.min(*active_ring - *active_deposit_ring); // active_normal_ring

			let now = <timestamp::Module<T>>::now();

			if promise_month >= 3 {
				// update active_deposit_ring
				*active_deposit_ring += value;

				// for now, kton_return is free
				// mint kton
				let kton_return = utils::compute_kton_return::<T>(value, promise_month);
				let kton_positive_imbalance = T::Kton::deposit_creating(stash, kton_return);
				T::KtonReward::on_unbalanced(kton_positive_imbalance);

				let expire_time = now.clone() + (MONTH_IN_SECONDS * promise_month).into();
				deposit_items.push(TimeDepositItem {
					value,
					start_time: now,
					expire_time,
				});
			}
		}

		fn claim_mature_deposits(origin) {
			let controller = ensure_signed(origin)?;
			Self::clear_mature_deposits(&controller);
		}

		// TODO: Replace expire_time with Deposit Id.
		fn claim_deposits_with_punish(origin, expire_time: T::Moment) {
			let controller = ensure_signed(origin)?;
			let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let StakingLedgers {
				stash,
				active_deposit_ring,
				deposit_items,
				..
			} = &mut ledger;

			let now = <timestamp::Module<T>>::now();

			ensure!(expire_time > now, "use unbond instead.");

			deposit_items.retain(|item| {
				if item.expire_time == expire_time {
					// at least 1 month
					let month_left = (
						(expire_time.clone() - now.clone()).saturated_into::<u32>()
						/ MONTH_IN_SECONDS
						).max(1);
					let kton_slash = utils::compute_kton_return::<T>(item.value, month_left) * 3.into();

					// check total free balance and locked one
					// strict on punishing in kton
					if T::Kton::free_balance(stash).checked_sub(&kton_slash).and_then(
						|new_balance| {
								T::Kton::ensure_can_withdraw(
									stash,
									kton_slash,
									WithdrawReason::Transfer.into(),
									new_balance
								).ok()
							}
						)
						.is_some()
					{
						*active_deposit_ring = active_deposit_ring.saturating_sub(item.value);

						let (imbalance, _) = T::Kton::slash(stash, kton_slash);
						T::KtonSlash::on_unbalanced(imbalance);

						return false;
					}
				}

				true
			});
		}

		/// may both withdraw ring and kton at the same time
		fn withdraw_unbonded(origin) {
			let controller = ensure_signed(origin)?;
			let mut ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let StakingLedgers {
				total_ring,
				total_kton,
				unlocking,
				..
			} = &mut ledger;
			let mut balance_kind = 0u8;
			let current_era = Self::current_era();

			unlocking.retain(|UnlockChunk {
				value,
				era,
			}| {
				if *era > current_era {
					return true;
				}

				match value {
					StakingBalance::Ring(ring) => {
						balance_kind |= 0b01;
						*total_ring = total_ring.saturating_sub(*ring);
					}
					StakingBalance::Kton(kton) => {
						balance_kind |= 0b10;
						*total_kton = total_kton.saturating_sub(*kton);
					}
				}

				false
			});

			match balance_kind {
				0 => (),
				1 => Self::update_ledger(&controller, &ledger, StakingBalance::Ring(0.into())),
				2 => Self::update_ledger(&controller, &ledger, StakingBalance::Kton(0.into())),
				3 => {
					Self::update_ledger(&controller, &ledger, StakingBalance::Ring(0.into()));
					Self::update_ledger(&controller, &ledger, StakingBalance::Kton(0.into()));
				}
				_ => unreachable!(),
			}
		}

		fn validate(origin, name: Vec<u8>, ratio: u32, unstake_threshold: u32) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			ensure!(
				unstake_threshold <= MAX_UNSTAKE_THRESHOLD,
				"unstake threshold too large"
			);
			// at most 100%
			let ratio = Perbill::from_percent(ratio.min(100));
			let prefs = ValidatorPrefs {unstake_threshold: unstake_threshold, validator_payment_ratio: ratio };

			<Nominators<T>>::remove(stash);
			<Validators<T>>::insert(stash, prefs);
			if !<NodeName<T>>::exists(&controller) {
				<NodeName<T>>::insert(controller, name);
				Self::deposit_event(RawEvent::NodeNameUpdated);
			}
		}

		fn nominate(origin, targets: Vec<<T::Lookup as StaticLookup>::Source>) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			ensure!(!targets.is_empty(), "targets cannot be empty");
			let targets = targets.into_iter()
				.take(MAX_NOMINATIONS)
				.map(T::Lookup::lookup)
				.collect::<result::Result<Vec<T::AccountId>, _>>()?;

			<Validators<T>>::remove(stash);
			<Nominators<T>>::insert(stash, targets);
		}

		fn chill(origin) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			<Validators<T>>::remove(stash);
			<Nominators<T>>::remove(stash);
		}

		fn set_payee(origin, payee: RewardDestination) {
			let controller = ensure_signed(origin)?;
			let ledger = Self::ledger(&controller).ok_or("not a controller")?;
			let stash = &ledger.stash;
			<Payee<T>>::insert(stash, payee);
		}

		fn set_controller(origin, controller: <T::Lookup as StaticLookup>::Source) {
			let stash = ensure_signed(origin)?;
			let old_controller = Self::bonded(&stash).ok_or("not a stash")?;
			let controller = T::Lookup::lookup(controller)?;
			if <Ledger<T>>::exists(&controller) {
				return Err("controller already paired")
			}
			if controller != old_controller {
				<Bonded<T>>::insert(&stash, &controller);
				if let Some(l) = <Ledger<T>>::take(&old_controller) {
					<Ledger<T>>::insert(&controller, l);
				}
			}
		}

		/// The ideal number of validators.
		fn set_validator_count(origin, #[compact] new: u32) {
			ensure_root(origin)?;
			ValidatorCount::put(new);
		}

		// ----- Root calls.

		fn force_new_era(origin) {
			ensure_root(origin)?;
			ForceEra::put(Forcing::ForceNone);
		}

		/// Set the validators who cannot be slashed (if any).
		fn set_invulnerables(origin, validators: Vec<T::AccountId>) {
			ensure_root(origin)?;
			<Invulnerables<T>>::put(validators);
		}
	}
}

impl<T: Trait> Module<T> {
	pub fn clear_mature_deposits(who: &T::AccountId) {
		let mut ledger = if let Some(l) = Self::ledger(&who) {
			l
		} else {
			return;
		};

		//		let mut ledger = Self::ledger(who).ok_or("not a controller")?;

		let StakingLedgers {
			active_deposit_ring,
			deposit_items,
			..
		} = &mut ledger;

		let now = <timestamp::Module<T>>::now();

		deposit_items.retain(|item| {
			if item.expire_time > now {
				return true;
			}

			*active_deposit_ring = active_deposit_ring.saturating_sub(item.value);

			false
		});
	}

	fn bond_helper_in_ring(
		stash: &T::AccountId,
		controller: &T::AccountId,
		value: RingBalanceOf<T>,
		promise_month: u32,
		mut ledger: StakingLedgers<
			T::AccountId,
			RingBalanceOf<T>,
			KtonBalanceOf<T>,
			StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
			T::Moment,
		>,
	) {
		// if stash promise to a extra-lock
		// there will be extra reward, kton, which
		// can also be use to stake.
		if promise_month >= 3 {
			ledger.active_deposit_ring += value;
			// for now, kton_return is free
			// mint kton
			let kton_return = utils::compute_kton_return::<T>(value, promise_month);
			let kton_positive_imbalance = T::Kton::deposit_creating(&stash, kton_return);
			T::KtonReward::on_unbalanced(kton_positive_imbalance);
			let now = <timestamp::Module<T>>::now();
			let expire_time = now.clone() + (MONTH_IN_SECONDS * promise_month).into();
			ledger.deposit_items.push(TimeDepositItem {
				value,
				start_time: now,
				expire_time,
			});
		}
		ledger.active_ring = ledger.active_ring.saturating_add(value);
		ledger.total_ring = ledger.total_ring.saturating_add(value);

		Self::update_ledger(&controller, &ledger, StakingBalance::Ring(value));
	}

	fn bond_helper_in_kton(
		controller: &T::AccountId,
		value: KtonBalanceOf<T>,
		mut ledger: StakingLedgers<
			T::AccountId,
			RingBalanceOf<T>,
			KtonBalanceOf<T>,
			StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
			T::Moment,
		>,
	) {
		ledger.total_kton += value;
		ledger.active_kton += value;

		Self::update_ledger(&controller, &ledger, StakingBalance::Kton(value));
	}

	fn update_ledger(
		controller: &T::AccountId,
		ledger: &StakingLedgers<
			T::AccountId,
			RingBalanceOf<T>,
			KtonBalanceOf<T>,
			StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
			T::Moment,
		>,
		staking_balance: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
	) {
		match staking_balance {
			StakingBalance::Ring(_r) => T::Ring::set_lock(
				STAKING_ID,
				&ledger.stash,
				ledger.total_ring,
				T::BlockNumber::max_value(),
				WithdrawReasons::all(),
			),

			StakingBalance::Kton(_k) => T::Kton::set_lock(
				STAKING_ID,
				&ledger.stash,
				ledger.total_kton,
				T::BlockNumber::max_value(),
				WithdrawReasons::all(),
			),
		}

		<Ledger<T>>::insert(controller, ledger);
	}

	fn slash_validator(stash: &T::AccountId, slash_ratio_in_u32: u32) {
		// construct Perbill here to make sure slash_ratio lt 0.
		let slash_ratio = Perbill::from_parts(slash_ratio_in_u32);
		// The exposures (backing stake) information of the validator to be slashed.
		let exposures = Self::stakers(stash);

		let (mut ring_imbalance, mut kton_imbalance) = Self::slash_individual(stash, slash_ratio);

		for i in exposures.others.iter() {
			let (rn, kn) = Self::slash_individual(&i.who, slash_ratio);
			ring_imbalance.subsume(rn);
			kton_imbalance.subsume(kn);
		}

		T::RingSlash::on_unbalanced(ring_imbalance);
		T::KtonSlash::on_unbalanced(kton_imbalance);
	}

	fn slash_individual(
		stash: &T::AccountId,
		slash_ratio: Perbill,
	) -> (RingNegativeImbalanceOf<T>, KtonNegativeImbalanceOf<T>) {
		let controller = Self::bonded(stash).unwrap();
		let mut ledger = Self::ledger(&controller).unwrap();

		// slash ring
		let (ring_imbalance, _) = if !ledger.total_ring.is_zero() {
			let slashable_ring = slash_ratio * ledger.total_ring;
			let value_slashed = Self::slash_helper(&controller, &mut ledger, StakingBalance::Ring(slashable_ring));
			T::Ring::slash(stash, value_slashed.0)
		} else {
			(<RingNegativeImbalanceOf<T>>::zero(), Zero::zero())
		};

		let (kton_imbalance, _) = if !ledger.total_kton.is_zero() {
			let slashable_kton = slash_ratio * ledger.total_kton;
			let value_slashed = Self::slash_helper(&controller, &mut ledger, StakingBalance::Kton(slashable_kton));
			T::Kton::slash(stash, value_slashed.1)
		} else {
			(<KtonNegativeImbalanceOf<T>>::zero(), Zero::zero())
		};

		(ring_imbalance, kton_imbalance)
	}

	fn slash_helper(
		controller: &T::AccountId,
		ledger: &mut StakingLedgers<
			T::AccountId,
			RingBalanceOf<T>,
			KtonBalanceOf<T>,
			StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
			T::Moment,
		>,
		value: StakingBalance<RingBalanceOf<T>, KtonBalanceOf<T>>,
	) -> (RingBalanceOf<T>, KtonBalanceOf<T>) {
		match value {
			StakingBalance::Ring(r) => {
				let StakingLedgers {
					total_ring,
					active_ring,
					active_deposit_ring,
					deposit_items,
					..
				} = ledger;

				// if slashing ring, first slashing normal ring
				// then, slashing time-deposit ring
				// TODO: check one more time (may be removed later)
				let total_value = r.min(*active_ring);
				let normal_active_value = total_value.min(*active_ring - *active_deposit_ring);

				// to prevent overflow
				// first slash normal bonded ring
				<RingPool<T>>::mutate(|r| *r -= normal_active_value);
				*active_ring -= normal_active_value;
				*total_ring -= normal_active_value;

				// bonded + unlocking
				// first slash active normal ring
				let mut value_left = total_value - normal_active_value;
				// then slash active time-promise ring
				// from the nearest expire time
				if !value_left.is_zero() {
					// sorted by expire_time from far to near
					deposit_items.sort_unstable_by_key(|item| {
						u64::max_value() - item.expire_time.clone().saturated_into::<u64>()
					});
					deposit_items.drain_filter(|item| {
						if value_left.is_zero() {
							return false;
						}

						let value_removed = value_left.min(item.value);

						*total_ring -= value_removed;
						*active_ring -= value_removed;
						*active_deposit_ring -= value_removed;

						item.value -= value_removed;
						value_left -= value_removed;

						<RingPool<T>>::mutate(|ring| *ring -= value_removed);

						item.value.is_zero()
					});
				}

				Self::update_ledger(controller, ledger, StakingBalance::Ring(0.into()));
				(total_value, 0.into())
			}
			StakingBalance::Kton(k) => {
				// check one more time
				// TODO: may be removed later
				let active_value = k.min(ledger.active_kton);
				// first slash active kton
				ledger.active_kton -= active_value;
				ledger.total_kton -= active_value;
				<KtonPool<T>>::mutate(|k| *k -= active_value);

				Self::update_ledger(controller, ledger, StakingBalance::Kton(0.into()));
				(0.into(), active_value)
			}
		}
	}

	fn new_session(
		session_index: SessionIndex,
	) -> Option<(
		Vec<T::AccountId>,
		Vec<(T::AccountId, Exposure<T::AccountId, ExtendedBalance>)>,
	)> {
		if ForceNewEra::take() || session_index % T::SessionsPerEra::get() == 0 {
			let validators = T::SessionInterface::validators();
			let prior = validators
				.into_iter()
				.map(|v| {
					let e = Self::stakers(&v);
					(v, e)
				})
				.collect();

			Self::new_era().map(move |new| (new, prior))
		} else {
			None
		}
	}

	/// The era has changed - enact new staking set.
	///
	/// NOTE: This always happens immediately before a session change to ensure that new validators
	/// get a chance to set their session keys.
	fn new_era() -> Option<Vec<T::AccountId>> {
		let reward = Self::session_reward() * Self::current_era_total_reward();
		if !reward.is_zero() {
			let validators = Self::current_elected();
			let len = validators.len() as u32; // validators length can never overflow u64
			let len: RingBalanceOf<T> = len.max(1).into();
			let block_reward_per_validator = reward / len;
			for v in validators.iter() {
				Self::reward_validator(v, block_reward_per_validator);
			}
			Self::deposit_event(RawEvent::Reward(block_reward_per_validator));
			// TODO: reward to treasury
		}

		// Increment current era.
		CurrentEra::mutate(|s| *s += 1);

		// check if ok to change epoch
		if Self::current_era() % T::ErasPerEpoch::get() == 0 {
			Self::new_epoch();
		}

		// Reassign all Stakers.
		let (_, maybe_new_validators) = Self::select_validators();

		maybe_new_validators
	}

	fn new_epoch() {
		EpochIndex::mutate(|e| *e += 1);
		let next_era_reward = utils::compute_current_era_reward::<T>();
		if !next_era_reward.is_zero() {
			<CurrentEraTotalReward<T>>::put(next_era_reward);
		}
	}

	fn reward_validator(stash: &T::AccountId, reward: RingBalanceOf<T>) {
		let off_the_table = Self::validators(stash).validator_payment_ratio * reward;
		let reward = reward - off_the_table;
		let mut imbalance = <RingPositiveImbalanceOf<T>>::zero();
		let validator_cut = if reward.is_zero() {
			Zero::zero()
		} else {
			let exposures = Self::stakers(stash);
			let total = exposures.total.max(One::one());

			for i in &exposures.others {
				let per_u64 = Perbill::from_rational_approximation(i.value, total);
				imbalance.maybe_subsume(Self::make_payout(&i.who, per_u64 * reward));
			}

			let per_u64 = Perbill::from_rational_approximation(exposures.own, total);
			per_u64 * reward
		};
		imbalance.maybe_subsume(Self::make_payout(stash, validator_cut + off_the_table));
		T::RingReward::on_unbalanced(imbalance);
	}

	/// Actually make a payment to a staker. This uses the currency's reward function
	/// to pay the right payee for the given staker account.
	fn make_payout(stash: &T::AccountId, amount: RingBalanceOf<T>) -> Option<RingPositiveImbalanceOf<T>> {
		let dest = Self::payee(stash);
		match dest {
			RewardDestination::Controller => {
				Self::bonded(stash).and_then(|controller| T::Ring::deposit_into_existing(&controller, amount).ok())
			}
			RewardDestination::Stash => T::Ring::deposit_into_existing(stash, amount).ok(),
		}
	}

	// TODO: Comments
	fn power_of(stash: &T::AccountId) -> ExtendedBalance {
		// power is a mixture of ring and kton
		// power = ring_ratio * POWER_COUNT / 2 + kton_ratio * POWER_COUNT / 2
		fn calc_power<S: rstd::convert::TryInto<u128>>(active: S, pool: S) -> ExtendedBalance {
			const HALF_POWER_COUNT: u128 = 1_000_000_000 / 2;

			Perquintill::from_rational_approximation(
				active.saturated_into::<ExtendedBalance>(),
				pool.saturated_into::<ExtendedBalance>().max(1),
			) * HALF_POWER_COUNT
		}

		Self::bonded(stash)
			.and_then(Self::ledger)
			.map(|l| calc_power(l.active_ring, Self::ring_pool()) + calc_power(l.active_kton, Self::kton_pool()))
			.unwrap_or_default()
	}

	/// Select a new validator set from the assembled stakers and their role preferences.
	///
	/// Returns the new `SlotStake` value.
	fn select_validators() -> (ExtendedBalance, Option<Vec<T::AccountId>>) {
		let maybe_elected_set = elect::<_, _>(
			Self::validator_count() as usize,
			Self::minimum_validator_count().max(1) as usize,
			<Validators<T>>::enumerate()
				.map(|(who, _)| who)
				.collect::<Vec<T::AccountId>>(),
			<Nominators<T>>::enumerate().collect(),
			Self::power_of,
			true,
		);

		if let Some(elected_set) = maybe_elected_set {
			let elected_stashes = elected_set
				.winners
				.iter()
				.map(|(s, _)| s.clone())
				.collect::<Vec<T::AccountId>>();
			let assignments = elected_set.assignments;

			// The return value of this is safe to be converted to u64.
			// Initialize the support of each candidate.
			let mut supports = <SupportMap<T::AccountId>>::new();
			elected_stashes
				.iter()
				.map(|e| (e, Self::power_of(e)))
				.for_each(|(e, s)| {
					let item = Support {
						own: s,
						total: s,
						..Default::default()
					};
					supports.insert(e.clone(), item);
				});

			// build support struct.
			for (n, assignment) in assignments.iter() {
				for (c, per_thing) in assignment.iter() {
					let nominator_stake = Self::power_of(n);
					// AUDIT: it is crucially important for the `Mul` implementation of all
					// per-things to be sound.
					let other_stake = *per_thing * nominator_stake;
					if let Some(support) = supports.get_mut(c) {
						// For an astronomically rich validator with more astronomically rich
						// set of nominators, this might saturate.
						support.total = support.total.saturating_add(other_stake);
						support.others.push((n.clone(), other_stake));
					}
				}
			}
			if cfg!(feature = "equalize") {
				let mut staked_assignments: Vec<(T::AccountId, Vec<PhragmenStakedAssignment<T::AccountId>>)> =
					Vec::with_capacity(assignments.len());
				for (n, assignment) in assignments.iter() {
					let mut staked_assignment: Vec<PhragmenStakedAssignment<T::AccountId>> =
						Vec::with_capacity(assignment.len());
					for (c, per_thing) in assignment.iter() {
						let nominator_stake = Self::power_of(n);
						let other_stake = *per_thing * nominator_stake;
						staked_assignment.push((c.clone(), other_stake));
					}
					staked_assignments.push((n.clone(), staked_assignment));
				}

				let tolerance = 0_u128;
				let iterations = 2_usize;
				equalize::<_, _>(staked_assignments, &mut supports, tolerance, iterations, Self::power_of);
			}

			// Clear Stakers.
			for v in Self::current_elected().iter() {
				<Stakers<T>>::remove(v);
			}

			// Populate Stakers and figure out the minimum stake behind a slot.
			let mut slot_stake = ExtendedBalance::max_value();
			for (c, s) in supports.into_iter() {
				// build `struct exposure` from `support`
				let exposure = Exposure {
					own: s.own,
					// This might reasonably saturate and we cannot do much about it. The sum of
					// someone's stake might exceed the balance type if they have the maximum amount
					// of balance and receive some support. This is super unlikely to happen, yet
					// we simulate it in some tests.
					total: s.total,
					others: s
						.others
						.into_iter()
						.map(|(who, value)| IndividualExposure { who, value: value })
						.collect::<Vec<IndividualExposure<_, _>>>(),
				};
				if exposure.total < slot_stake {
					slot_stake = exposure.total;
				}
				<Stakers<T>>::insert(&c, exposure.clone());
			}

			// Update slot stake.
			SlotStake::put(&slot_stake);

			// Set the new validator set in sessions.
			<CurrentElected<T>>::put(&elected_stashes);

			// In order to keep the property required by `n_session_ending`
			// that we must return the new validator set even if it's the same as the old,
			// as long as any underlying economic conditions have changed, we don't attempt
			// to do any optimization where we compare against the prior set.
			(slot_stake, Some(elected_stashes))
		} else {
			// There were not enough candidates for even our minimal level of functionality.
			// This is bad.
			// We should probably disable all functionality except for block production
			// and let the chain keep producing blocks until we can decide on a sufficiently
			// substantial set.
			// TODO: #2494
			(Self::slot_stake(), None)
		}
	}

	/// Remove all associated data of a stash account from the staking system.
	///
	/// This is called :
	/// - Immediately when an account's balance falls below existential deposit.
	/// - after a `withdraw_unbond()` call that frees all of a stash's bonded balance.
	fn kill_stash(stash: &T::AccountId) {
		if let Some(controller) = <Bonded<T>>::take(stash) {
			<Ledger<T>>::remove(&controller);
		}
		<Payee<T>>::remove(stash);
		<Validators<T>>::remove(stash);
		<Nominators<T>>::remove(stash);
	}

	pub fn reward_by_ids(validators_points: impl IntoIterator<Item = (T::AccountId, u32)>) {
		CurrentEraPointsEarned::mutate(|rewards| {
			let current_elected = <Module<T>>::current_elected();
			for (validator, points) in validators_points.into_iter() {
				if let Some(index) = current_elected.iter().position(|elected| *elected == validator) {
					rewards.add_points_to_index(index as u32, points);
				}
			}
		});
	}

	/// Ensures that at the end of the current session there will be a new era.
	fn ensure_new_era() {
		match ForceEra::get() {
			Forcing::ForceAlways | Forcing::ForceNew => (),
			_ => ForceEra::put(Forcing::ForceNew),
		}
	}
}

impl<T: Trait> session::OnSessionEnding<T::AccountId> for Module<T> {
	fn on_session_ending(_ending: SessionIndex, start_session: SessionIndex) -> Option<Vec<T::AccountId>> {
		Self::new_session(start_session - 1).map(|(new, _old)| new)
	}
}

impl<T: Trait> OnSessionEnding<T::AccountId, Exposure<T::AccountId, ExtendedBalance>> for Module<T> {
	fn on_session_ending(
		_ending: SessionIndex,
		start_session: SessionIndex,
	) -> Option<(
		Vec<T::AccountId>,
		Vec<(T::AccountId, Exposure<T::AccountId, ExtendedBalance>)>,
	)> {
		Self::new_session(start_session - 1)
	}
}

impl<T: Trait> OnFreeBalanceZero<T::AccountId> for Module<T> {
	fn on_free_balance_zero(stash: &T::AccountId) {
		Self::kill_stash(stash);
	}
}

/// Add reward points to block authors:
/// * 20 points to the block producer for producing a (non-uncle) block in the relay chain,
/// * 2 points to the block producer for each reference to a previously unreferenced uncle, and
/// * 1 point to the producer of each referenced uncle block.
impl<T: Trait + authorship::Trait> authorship::EventHandler<T::AccountId, T::BlockNumber> for Module<T> {
	fn note_author(_author: T::AccountId) {}
	fn note_uncle(_author: T::AccountId, _age: T::BlockNumber) {
		Self::reward_by_ids(vec![(<authorship::Module<T>>::author(), 2), (author, 1)])
	}
}

pub struct StashOf<T>(rstd::marker::PhantomData<T>);

impl<T: Trait> Convert<T::AccountId, Option<T::AccountId>> for StashOf<T> {
	fn convert(controller: T::AccountId) -> Option<T::AccountId> {
		<Module<T>>::ledger(&controller).map(|l| l.stash)
	}
}

/// A typed conversion from stash account ID to the current exposure of nominators
/// on that account.
pub struct ExposureOf<T>(rstd::marker::PhantomData<T>);

impl<T: Trait> Convert<T::AccountId, Option<Exposure<T::AccountId, ExtendedBalance>>> for ExposureOf<T> {
	fn convert(validator: T::AccountId) -> Option<Exposure<T::AccountId, ExtendedBalance>> {
		Some(<Module<T>>::stakers(&validator))
	}
}

impl<T: Trait> SelectInitialValidators<T::AccountId> for Module<T> {
	fn select_initial_validators() -> Option<Vec<T::AccountId>> {
		<Module<T>>::select_validators().1
	}
}

/// This is intended to be used with `FilterHistoricalOffences`.
impl<T: Trait> OnOffenceHandler<T::AccountId, session::historical::IdentificationTuple<T>> for Module<T>
where
	T: session::Trait<ValidatorId = <T as system::Trait>::AccountId>,
	T: session::historical::Trait<
		FullIdentification = Exposure<<T as system::Trait>::AccountId, BalanceOf<T>>,
		FullIdentificationOf = ExposureOf<T>,
	>,
	T::SessionHandler: session::SessionHandler<<T as system::Trait>::AccountId>,
	T::OnSessionEnding: session::OnSessionEnding<<T as system::Trait>::AccountId>,
	T::SelectInitialValidators: session::SelectInitialValidators<<T as system::Trait>::AccountId>,
	T::ValidatorIdOf: Convert<<T as system::Trait>::AccountId, Option<<T as system::Trait>::AccountId>>,
{
	fn on_offence(
		offenders: &[OffenceDetails<T::AccountId, session::historical::IdentificationTuple<T>>],
		slash_fraction: &[Perbill],
	) {
		let mut remaining_imbalance = <NegativeImbalanceOf<T>>::zero();
		let slash_reward_fraction = SlashRewardFraction::get();

		let era_now = Self::current_era();
		let mut journal = Self::era_slash_journal(era_now);
		for (details, slash_fraction) in offenders.iter().zip(slash_fraction) {
			let stash = &details.offender.0;
			let exposure = &details.offender.1;

			// Skip if the validator is invulnerable.
			if Self::invulnerables().contains(stash) {
				continue;
			}

			// Auto deselect validator on any offence and force a new era if they haven't previously
			// been deselected.
			if <Validators<T>>::exists(stash) {
				<Validators<T>>::remove(stash);
				Self::ensure_new_era();
			}

			// calculate the amount to slash
			let slash_exposure = exposure.total;
			let amount = *slash_fraction * slash_exposure;
			// in some cases `slash_fraction` can be just `0`,
			// which means we are not slashing this time.
			if amount.is_zero() {
				continue;
			}

			// make sure to disable validator till the end of this session
			if T::SessionInterface::disable_validator(stash).unwrap_or(false) {
				// force a new era, to select a new validator set
				Self::ensure_new_era();
			}
			// actually slash the validator
			let slashed_amount = Self::slash_validator(stash, amount, exposure, &mut journal);

			// distribute the rewards according to the slash
			let slash_reward = slash_reward_fraction * slashed_amount.peek();
			if !slash_reward.is_zero() && !details.reporters.is_empty() {
				let (mut reward, rest) = slashed_amount.split(slash_reward);
				// split the reward between reporters equally. Division cannot fail because
				// we guarded against it in the enclosing if.
				let per_reporter = reward.peek() / (details.reporters.len() as u32).into();
				for reporter in &details.reporters {
					let (reporter_reward, rest) = reward.split(per_reporter);
					reward = rest;
					T::Currency::resolve_creating(reporter, reporter_reward);
				}
				// The rest goes to the treasury.
				remaining_imbalance.subsume(reward);
				remaining_imbalance.subsume(rest);
			} else {
				remaining_imbalance.subsume(slashed_amount);
			}
		}
		<EraSlashJournal<T>>::insert(era_now, journal);

		// Handle the rest of imbalances
		T::Slash::on_unbalanced(remaining_imbalance);
	}
}

/// Filter historical offences out and only allow those from the current era.
pub struct FilterHistoricalOffences<T, R> {
	_inner: rstd::marker::PhantomData<(T, R)>,
}

impl<T, Reporter, Offender, R, O> ReportOffence<Reporter, Offender, O> for FilterHistoricalOffences<Module<T>, R>
where
	T: Trait,
	R: ReportOffence<Reporter, Offender, O>,
	O: Offence<Offender>,
{
	fn report_offence(reporters: Vec<Reporter>, offence: O) {
		// disallow any slashing from before the current era.
		let offence_session = offence.session_index();
		if offence_session >= <Module<T>>::current_era_start_session_index() {
			R::report_offence(reporters, offence)
		} else {
			<Module<T>>::deposit_event(RawEvent::OldSlashingReportDiscarded(offence_session))
		}
	}
}
