#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(test)]
mod mock;

pub use self::pallet::*;

#[allow(unused_variables)]
#[allow(clippy::large_enum_variant)]
#[frame_support::pallet]
pub mod pallet {
	use codec::{Decode, Encode};
	use frame_support::{
		dispatch::DispatchResult, pallet_prelude::*, traits::StorageVersion, transactional,
	};
	use frame_system::pallet_prelude::*;
	use scale_info::TypeInfo;
	use sp_core::{hash::H256, U256};
	use sp_runtime::{traits::Clear, RuntimeDebug};
	use sp_std::{convert::From, vec, vec::Vec};
	use sygma_traits::{DepositNonce, DomainID, FeeHandler, ResourceId};
	use xcm::latest::{prelude::*, MultiLocation};
	use xcm_executor::traits::TransactAsset;

	#[allow(dead_code)]
	const LOG_TARGET: &str = "runtime::sygmabridge";
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[derive(PartialEq, Eq, Clone, Encode, Decode, TypeInfo, RuntimeDebug)]
	pub struct Proposal {
		origin_domain_id: DomainID,
		deposit_nonce: DepositNonce,
		resource_id: ResourceId,
		data: Vec<u8>,
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	#[pallet::storage_version(STORAGE_VERSION)]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Origin used to administer the pallet
		type BridgeCommitteeOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The identifier for this chain.
		/// This must be unique and must not collide with existing IDs within a set of bridged
		/// chains.
		#[pallet::constant]
		type DestDomainID: Get<DomainID>;

		/// Bridge transfer reserve account
		#[pallet::constant]
		type TransferReserveAccount: Get<Self::AccountId>;

		/// Fee reserve account
		#[pallet::constant]
		type FeeReserveAccount: Get<Self::AccountId>;

		/// Fee information getter
		type FeeHandler: FeeHandler;

		/// Implementation of withdraw and deposit an asset.
		type AssetTransactor: TransactAsset;

		/// AssetId and ResourceId pairs
		type ResourcePairs: Get<Vec<(AssetId, ResourceId)>>;
	}

	#[allow(dead_code)]
	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// When initial bridge transfer send to dest domain
		/// args: [dest_domain_id, resource_id, deposit_nonce, sender, deposit_data,
		/// handler_reponse]
		Deposit(DomainID, ResourceId, DepositNonce, T::AccountId, Vec<u8>, Vec<u8>),
		/// When user is going to retry a bridge transfer
		/// args: [tx_hash]
		Retry(H256),
		/// When bridge is paused
		/// args: [dest_domain_id]
		BridgePaused(DomainID),
		/// When bridge is unpaused
		/// args: [dest_domain_id]
		BridgeUnpaused(DomainID),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Protected operation, must be performed by relayer
		BadMpcSignature,
		/// MPC key not set
		MissingMpcKey,
		/// MPC key can not be updated
		MpcKeyNotUpdatable,
		/// Bridge is paused
		BridgePaused,
		/// Bridge is unpaused
		BridgeUnpaused,
		/// Fee config option missing
		MissingFeeConfig,
		/// Asset not bound to a resource id
		AssetNotBound,
		/// Proposal has either failed or succeeded
		ProposalAlreadyComplete,
		/// Transactor operation failed
		TransactorFailed,
		/// Function unimplemented
		Unimplemented,
	}

	/// Deposit counter of dest domain
	#[pallet::storage]
	#[pallet::getter(fn dest_counts)]
	pub type DepositCounts<T> = StorageValue<_, DepositNonce, ValueQuery>;

	/// Bridge Pause indicator
	/// Bridge is unpaused initially, until pause
	/// After MPC key setup, bridge should be paused until ready to unpause
	#[pallet::storage]
	#[pallet::getter(fn is_paused)]
	pub type IsPaused<T> = StorageValue<_, bool, ValueQuery>;

	/// Pre-set MPC public key
	#[pallet::storage]
	#[pallet::getter(fn mpc_key)]
	pub type MpcKey<T> = StorageValue<_, [u8; 32], ValueQuery>;

	/// Mark whether a deposit nonce was used. Used to mark execution status of a proposal.
	#[pallet::storage]
	#[pallet::getter(fn mpc_keys)]
	pub type UsedNonces<T: Config> =
		StorageDoubleMap<_, Twox64Concat, DomainID, Twox64Concat, U256, U256>;

	#[pallet::call]
	impl<T: Config> Pallet<T>
	where
		<T as frame_system::Config>::AccountId: From<[u8; 32]> + Into<[u8; 32]>,
	{
		/// Pause bridge, this would lead to bridge transfer failure before it being unpaused.
		#[pallet::weight(195_000_000)]
		pub fn pause_bridge(origin: OriginFor<T>) -> DispatchResult {
			// Ensure bridge committee
			T::BridgeCommitteeOrigin::ensure_origin(origin)?;

			// make sure MPC key is set up
			ensure!(!MpcKey::<T>::get().is_clear(), Error::<T>::MissingMpcKey);

			// Mark as paused
			IsPaused::<T>::set(true);

			// Emit BridgePause event
			Self::deposit_event(Event::BridgePaused(T::DestDomainID::get()));
			Ok(())
		}

		/// Unpause bridge.
		#[pallet::weight(195_000_000)]
		pub fn unpause_bridge(origin: OriginFor<T>) -> DispatchResult {
			// Ensure bridge committee
			T::BridgeCommitteeOrigin::ensure_origin(origin)?;

			// make sure MPC key is set up
			ensure!(!MpcKey::<T>::get().is_clear(), Error::<T>::MissingMpcKey);

			// make sure the current status is paused
			ensure!(IsPaused::<T>::get(), Error::<T>::BridgeUnpaused);

			// Mark as unpaused
			IsPaused::<T>::set(false);

			// Emit BridgeUnpause event
			Self::deposit_event(Event::BridgeUnpaused(T::DestDomainID::get()));
			Ok(())
		}

		/// Mark an ECDSA public key as a MPC account.
		#[pallet::weight(195_000_000)]
		pub fn set_mpc_key(origin: OriginFor<T>, _key: [u8; 32]) -> DispatchResult {
			// Ensure bridge committee
			T::BridgeCommitteeOrigin::ensure_origin(origin)?;

			// Cannot set MPC key is it's already set
			ensure!(MpcKey::<T>::get().is_clear(), Error::<T>::MpcKeyNotUpdatable);

			// Set MPC account public key
			MpcKey::<T>::set(_key);
			Ok(())
		}

		/// Initiates a transfer.
		#[pallet::weight(195_000_000)]
		#[transactional]
		pub fn deposit(
			_origin: OriginFor<T>,
			_asset: MultiAsset,
			_dest: MultiLocation,
		) -> DispatchResult {
			// Asset transactor

			// Extract asset (MultiAsset) to get corresponding ResourceId

			// Extract dest (MultiLocation) to get corresponding DomainId and Etheruem address

			// Handle asset with Transactor, potential examples:
			// T::Transactor::withdraw_asset(fee + amount, sender_location);
			// T::Transactor::deposit_asset(fee, T::FeeReserveAccount::get().into());
			// T::Transactor::deposit_asset(amount, T::TransferReserveAccount::get().into());

			// Bump deposit nonce

			// Emit Deposit event

			Err(Error::<T>::Unimplemented.into())
		}

		/// This method is used to trigger the process for retrying failed deposits on the MPC side.
		#[pallet::weight(195_000_000)]
		#[transactional]
		pub fn retry(_origin: OriginFor<T>, hash: H256) -> DispatchResult {
			// Emit retry event
			// For clippy happy
			Self::deposit_event(Event::<T>::Retry(hash));
			Err(Error::<T>::Unimplemented.into())
		}

		/// Executes a batch of deposit proposals (only if signature is signed by MPC).
		#[pallet::weight(195_000_000)]
		#[transactional]
		pub fn execute_proposal(
			_origin: OriginFor<T>,
			_proposals: Vec<Proposal>,
			_signature: Vec<u8>,
		) -> DispatchResult {
			// Verify MPC signature

			// Parse proposal

			// Extract ResourceId from proposal data to get corresponding asset (MultiAsset)

			// Extract Receipt from proposal data to get corresponding location (MultiLocation)

			// Handle asset with Transactor

			// Update proposal status

			Err(Error::<T>::Unimplemented.into())
		}
	}

	impl<T: Config> Pallet<T>
	where
		<T as frame_system::Config>::AccountId: From<[u8; 32]> + Into<[u8; 32]>,
	{
		/// Verifies that proposal data is signed by MPC address.
		#[allow(dead_code)]
		fn verify(_proposals: Vec<Proposal>, _signature: Vec<u8>) -> bool {
			false
		}
	}

	#[cfg(test)]
	mod test {
		use crate as bridge;
		use crate::{Event as SygmaBridgeEvent, IsPaused, MpcKey};
		use bridge::mock::{
			assert_events, new_test_ext, Runtime, RuntimeEvent, RuntimeOrigin as Origin,
			SygmaBridge, ALICE,
		};
		use frame_support::{assert_noop, assert_ok, sp_runtime::traits::BadOrigin};

		#[test]
		fn set_mpc_key() {
			new_test_ext().execute_with(|| {
				let default_key: [u8; 32] = Default::default();
				let test_mpc_key_a: [u8; 32] = [1; 32];
				let test_mpc_key_b: [u8; 32] = [2; 32];

				assert_eq!(MpcKey::<Runtime>::get(), default_key);

				// set to test_key_a
				assert_ok!(SygmaBridge::set_mpc_key(Origin::root(), test_mpc_key_a));
				assert_eq!(MpcKey::<Runtime>::get(), test_mpc_key_a);

				// set to test_key_b: should be MpcKeyNotUpdatable error
				assert_noop!(
					SygmaBridge::set_mpc_key(Origin::root(), test_mpc_key_b),
					bridge::Error::<Runtime>::MpcKeyNotUpdatable
				);

				// permission test: unauthorized account should not be able to set mpc key
				let unauthorized_account = Origin::from(Some(ALICE));
				assert_noop!(
					SygmaBridge::set_mpc_key(unauthorized_account, test_mpc_key_a),
					BadOrigin
				);
				assert_eq!(MpcKey::<Runtime>::get(), test_mpc_key_a);
			})
		}

		#[test]
		fn pause_bridge() {
			new_test_ext().execute_with(|| {
				let default_key: [u8; 32] = Default::default();
				let test_mpc_key_a: [u8; 32] = [1; 32];

				assert_eq!(MpcKey::<Runtime>::get(), default_key);

				// pause bridge when mpc key is not set, should be err
				assert_noop!(
					SygmaBridge::pause_bridge(Origin::root()),
					bridge::Error::<Runtime>::MissingMpcKey
				);

				// set mpc key to test_key_a
				assert_ok!(SygmaBridge::set_mpc_key(Origin::root(), test_mpc_key_a));
				assert_eq!(MpcKey::<Runtime>::get(), test_mpc_key_a);

				// pause bridge again, should be ok
				assert_ok!(SygmaBridge::pause_bridge(Origin::root()));
				assert!(IsPaused::<Runtime>::get());
				assert_events(vec![RuntimeEvent::SygmaBridge(SygmaBridgeEvent::BridgePaused(1))]);

				// pause bridge again after paused, should be ok
				assert_ok!(SygmaBridge::pause_bridge(Origin::root()));
				assert!(IsPaused::<Runtime>::get());
				assert_events(vec![RuntimeEvent::SygmaBridge(SygmaBridgeEvent::BridgePaused(1))]);

				// permission test: unauthorized account should not be able to pause bridge
				let unauthorized_account = Origin::from(Some(ALICE));
				assert_noop!(SygmaBridge::pause_bridge(unauthorized_account), BadOrigin);
				assert!(IsPaused::<Runtime>::get());
			})
		}

		#[test]
		fn unpause_bridge() {
			new_test_ext().execute_with(|| {
				let default_key: [u8; 32] = Default::default();
				let test_mpc_key_a: [u8; 32] = [1; 32];

				assert_eq!(MpcKey::<Runtime>::get(), default_key);

				// unpause bridge when mpc key is not set, should be error
				assert_noop!(
					SygmaBridge::unpause_bridge(Origin::root()),
					bridge::Error::<Runtime>::MissingMpcKey
				);

				// set mpc key to test_key_a and pause bridge
				assert_ok!(SygmaBridge::set_mpc_key(Origin::root(), test_mpc_key_a));
				assert_eq!(MpcKey::<Runtime>::get(), test_mpc_key_a);
				assert_ok!(SygmaBridge::pause_bridge(Origin::root()));
				assert_events(vec![RuntimeEvent::SygmaBridge(SygmaBridgeEvent::BridgePaused(1))]);

				// bridge should be paused here
				assert!(IsPaused::<Runtime>::get());

				// ready to unpause bridge, should be ok
				assert_ok!(SygmaBridge::unpause_bridge(Origin::root()));
				assert_events(vec![RuntimeEvent::SygmaBridge(SygmaBridgeEvent::BridgeUnpaused(1))]);

				// try to unpause it again, should be error
				assert_noop!(
					SygmaBridge::unpause_bridge(Origin::root()),
					bridge::Error::<Runtime>::BridgeUnpaused
				);

				// permission test: unauthorized account should not be able to unpause a recognized
				// bridge
				let unauthorized_account = Origin::from(Some(ALICE));
				assert_noop!(SygmaBridge::unpause_bridge(unauthorized_account), BadOrigin);
				assert!(!IsPaused::<Runtime>::get());
			})
		}
	}
}
