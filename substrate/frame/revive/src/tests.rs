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

mod pallet_dummy;
mod precompiles;

use self::test_utils::{ensure_stored, expected_deposit};
use crate::{
	self as pallet_revive,
	address::{create1, create2, AddressMapper},
	evm::{runtime::GAS_PRICE, CallTrace, CallTracer, CallType, GenericTransaction},
	exec::Key,
	limits,
	storage::DeletionQueueManager,
	test_utils::{builder::Contract, *},
	tests::test_utils::{get_contract, get_contract_checked},
	tracing::trace,
	weights::WeightInfo,
	AccountId32Mapper, AccountInfo, AccountInfoOf, BalanceOf, BalanceWithDust, BumpNonce, Code,
	CodeInfoOf, Config, ContractInfo, DeletionQueueCounter, DepositLimit, Error, EthTransactError,
	HoldReason, Origin, Pallet, PristineCode, H160,
};
use assert_matches::assert_matches;
use codec::Encode;
use frame_support::{
	assert_err, assert_err_ignore_postinfo, assert_noop, assert_ok, derive_impl,
	pallet_prelude::EnsureOrigin,
	parameter_types,
	storage::child,
	traits::{
		fungible::{BalancedHold, Inspect, Mutate, MutateHold},
		tokens::Preservation,
		ConstU32, ConstU64, FindAuthor, OnIdle, OnInitialize, StorageVersion,
	},
	weights::{constants::WEIGHT_REF_TIME_PER_SECOND, FixedFee, IdentityFee, Weight, WeightMeter},
};
use frame_system::{EventRecord, Phase};
use pallet_revive_fixtures::compile_module;
use pallet_revive_uapi::{ReturnErrorCode as RuntimeReturnCode, ReturnFlags};
use pallet_transaction_payment::{ConstFeeMultiplier, Multiplier};
use pretty_assertions::{assert_eq, assert_ne};
use sp_core::{Get, U256};
use sp_io::hashing::blake2_256;
use sp_keystore::{testing::MemoryKeystore, KeystoreExt};
use sp_runtime::{
	testing::H256,
	traits::{BlakeTwo256, Convert, IdentityLookup, One, Zero},
	AccountId32, BuildStorage, DispatchError, Perbill, TokenError,
};

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Timestamp: pallet_timestamp,
		Utility: pallet_utility,
		Contracts: pallet_revive,
		Proxy: pallet_proxy,
		TransactionPayment: pallet_transaction_payment,
		Dummy: pallet_dummy
	}
);

macro_rules! assert_return_code {
	( $x:expr , $y:expr $(,)? ) => {{
		assert_eq!(u32::from_le_bytes($x.data[..].try_into().unwrap()), $y as u32);
	}};
}

macro_rules! assert_refcount {
	( $code_hash:expr , $should:expr $(,)? ) => {{
		let is = crate::CodeInfoOf::<Test>::get($code_hash).map(|m| m.refcount()).unwrap();
		assert_eq!(is, $should);
	}};
}

pub mod test_utils {
	use super::{
		BalanceWithDust, CodeHashLockupDepositPercent, Contracts, DepositPerByte, DepositPerItem,
		Test,
	};
	use crate::{
		address::AddressMapper, exec::AccountIdOf, AccountInfo, AccountInfoOf, BalanceOf, CodeInfo,
		CodeInfoOf, Config, ContractInfo, PristineCode,
	};
	use codec::{Encode, MaxEncodedLen};
	use frame_support::traits::fungible::{InspectHold, Mutate};
	use sp_core::H160;

	pub fn place_contract(address: &AccountIdOf<Test>, code_hash: sp_core::H256) {
		set_balance(address, Contracts::min_balance() * 10);
		<CodeInfoOf<Test>>::insert(code_hash, CodeInfo::new(address.clone()));
		let address =
			<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_address(&address);
		let contract = <ContractInfo<Test>>::new(&address, 0, code_hash).unwrap();
		AccountInfo::<Test>::insert_contract(&address, contract);
	}
	pub fn set_balance(who: &AccountIdOf<Test>, amount: u64) {
		let _ = <Test as Config>::Currency::set_balance(who, amount);
	}
	pub fn get_balance(who: &AccountIdOf<Test>) -> u64 {
		<Test as Config>::Currency::free_balance(who)
	}
	pub fn get_balance_on_hold(
		reason: &<Test as Config>::RuntimeHoldReason,
		who: &AccountIdOf<Test>,
	) -> u64 {
		<Test as Config>::Currency::balance_on_hold(reason.into(), who)
	}
	pub fn get_contract(addr: &H160) -> ContractInfo<Test> {
		get_contract_checked(addr).unwrap()
	}
	pub fn get_contract_checked(addr: &H160) -> Option<ContractInfo<Test>> {
		AccountInfo::<Test>::load_contract(addr)
	}
	pub fn get_code_deposit(code_hash: &sp_core::H256) -> BalanceOf<Test> {
		crate::CodeInfoOf::<Test>::get(code_hash).unwrap().deposit()
	}
	pub fn lockup_deposit(code_hash: &sp_core::H256) -> BalanceOf<Test> {
		CodeHashLockupDepositPercent::get().mul_ceil(get_code_deposit(code_hash)).into()
	}
	pub fn contract_base_deposit(addr: &H160) -> BalanceOf<Test> {
		let contract_info = self::get_contract(&addr);
		let info_size = contract_info.encoded_size() as u64;
		let code_deposit = CodeHashLockupDepositPercent::get()
			.mul_ceil(get_code_deposit(&contract_info.code_hash));
		let deposit = DepositPerByte::get()
			.saturating_mul(info_size)
			.saturating_add(DepositPerItem::get())
			.saturating_add(code_deposit);
		let immutable_size = contract_info.immutable_data_len() as u64;
		if immutable_size > 0 {
			let immutable_deposit = DepositPerByte::get()
				.saturating_mul(immutable_size)
				.saturating_add(DepositPerItem::get());
			deposit.saturating_add(immutable_deposit)
		} else {
			deposit
		}
	}
	pub fn expected_deposit(code_len: usize) -> u64 {
		// For code_info, the deposit for max_encoded_len is taken.
		let code_info_len = CodeInfo::<Test>::max_encoded_len() as u64;
		// Calculate deposit to be reserved.
		// We add 2 storage items: one for code, other for code_info
		DepositPerByte::get().saturating_mul(code_len as u64 + code_info_len) +
			DepositPerItem::get().saturating_mul(2)
	}
	pub fn ensure_stored(code_hash: sp_core::H256) -> usize {
		// Assert that code_info is stored
		assert!(CodeInfoOf::<Test>::contains_key(&code_hash));
		// Assert that contract code is stored, and get its size.
		PristineCode::<Test>::try_get(&code_hash).unwrap().len()
	}
	pub fn u256_bytes(u: u64) -> [u8; 32] {
		let mut buffer = [0u8; 32];
		let bytes = u.to_le_bytes();
		buffer[..8].copy_from_slice(&bytes);
		buffer
	}

	pub fn set_balance_with_dust(address: &H160, value: BalanceWithDust<BalanceOf<Test>>) {
		use frame_support::traits::Currency;
		let ed = <Test as Config>::Currency::minimum_balance();
		let (value, dust) = value.deconstruct();
		let account_id = <Test as Config>::AddressMapper::to_account_id(&address);
		<Test as Config>::Currency::set_balance(&account_id, ed + value);
		if dust > 0 {
			AccountInfoOf::<Test>::mutate(&address, |account| {
				if let Some(account) = account {
					account.dust = dust;
				} else {
					*account = Some(AccountInfo { dust, ..Default::default() });
				}
			});
		}
	}
}

mod builder {
	use super::Test;
	use crate::{
		test_utils::{builder::*, ALICE},
		tests::RuntimeOrigin,
		Code,
	};
	use sp_core::{H160, H256};

	pub fn bare_instantiate(code: Code) -> BareInstantiateBuilder<Test> {
		BareInstantiateBuilder::<Test>::bare_instantiate(RuntimeOrigin::signed(ALICE), code)
	}

	pub fn bare_call(dest: H160) -> BareCallBuilder<Test> {
		BareCallBuilder::<Test>::bare_call(RuntimeOrigin::signed(ALICE), dest)
	}

	pub fn instantiate_with_code(code: Vec<u8>) -> InstantiateWithCodeBuilder<Test> {
		InstantiateWithCodeBuilder::<Test>::instantiate_with_code(
			RuntimeOrigin::signed(ALICE),
			code,
		)
	}

	pub fn instantiate(code_hash: H256) -> InstantiateBuilder<Test> {
		InstantiateBuilder::<Test>::instantiate(RuntimeOrigin::signed(ALICE), code_hash)
	}

	pub fn call(dest: H160) -> CallBuilder<Test> {
		CallBuilder::<Test>::call(RuntimeOrigin::signed(ALICE), dest)
	}

	pub fn eth_call(dest: H160) -> EthCallBuilder<Test> {
		EthCallBuilder::<Test>::eth_call(RuntimeOrigin::signed(ALICE), dest)
	}
}

impl Test {
	pub fn set_unstable_interface(unstable_interface: bool) {
		UNSTABLE_INTERFACE.with(|v| *v.borrow_mut() = unstable_interface);
	}
}

parameter_types! {
	pub BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(
			Weight::from_parts(2 * WEIGHT_REF_TIME_PER_SECOND, u64::MAX),
		);
	pub static ExistentialDeposit: u64 = 1;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type BlockWeights = BlockWeights;
	type AccountId = AccountId32;
	type Lookup = IdentityLookup<Self::AccountId>;
	type AccountData = pallet_balances::AccountData<u64>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type ExistentialDeposit = ExistentialDeposit;
	type ReserveIdentifier = [u8; 8];
	type AccountStore = System;
}

#[derive_impl(pallet_timestamp::config_preludes::TestDefaultConfig)]
impl pallet_timestamp::Config for Test {}

impl pallet_utility::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = ();
}

impl pallet_proxy::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = Balances;
	type ProxyType = ();
	type ProxyDepositBase = ConstU64<1>;
	type ProxyDepositFactor = ConstU64<1>;
	type MaxProxies = ConstU32<32>;
	type WeightInfo = ();
	type MaxPending = ConstU32<32>;
	type CallHasher = BlakeTwo256;
	type AnnouncementDepositBase = ConstU64<1>;
	type AnnouncementDepositFactor = ConstU64<1>;
	type BlockNumberProvider = frame_system::Pallet<Test>;
}

parameter_types! {
	pub FeeMultiplier: Multiplier = Multiplier::one();
}

#[derive_impl(pallet_transaction_payment::config_preludes::TestDefaultConfig)]
impl pallet_transaction_payment::Config for Test {
	type OnChargeTransaction = pallet_transaction_payment::FungibleAdapter<Balances, ()>;
	type WeightToFee = IdentityFee<<Self as pallet_balances::Config>::Balance>;
	type LengthToFee = FixedFee<100, <Self as pallet_balances::Config>::Balance>;
	type FeeMultiplierUpdate = ConstFeeMultiplier<FeeMultiplier>;
}

impl pallet_dummy::Config for Test {}

parameter_types! {
	pub static DepositPerByte: BalanceOf<Test> = 1;
	pub const DepositPerItem: BalanceOf<Test> = 2;
	pub const CodeHashLockupDepositPercent: Perbill = Perbill::from_percent(30);
	pub static ChainId: u64 = 448;
}

impl Convert<Weight, BalanceOf<Self>> for Test {
	fn convert(w: Weight) -> BalanceOf<Self> {
		w.ref_time()
	}
}

parameter_types! {
	pub static UploadAccount: Option<<Test as frame_system::Config>::AccountId> = None;
	pub static InstantiateAccount: Option<<Test as frame_system::Config>::AccountId> = None;
}

pub struct EnsureAccount<T, A>(core::marker::PhantomData<(T, A)>);
impl<T: Config, A: sp_core::Get<Option<crate::AccountIdOf<T>>>>
	EnsureOrigin<<T as frame_system::Config>::RuntimeOrigin> for EnsureAccount<T, A>
where
	<T as frame_system::Config>::AccountId: From<AccountId32>,
{
	type Success = T::AccountId;

	fn try_origin(o: T::RuntimeOrigin) -> Result<Self::Success, T::RuntimeOrigin> {
		let who = <frame_system::EnsureSigned<_> as EnsureOrigin<_>>::try_origin(o.clone())?;
		if matches!(A::get(), Some(a) if who != a) {
			return Err(o);
		}

		Ok(who)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin() -> Result<T::RuntimeOrigin, ()> {
		Err(())
	}
}
parameter_types! {
	pub static UnstableInterface: bool = true;
	pub CheckingAccount: AccountId32 = BOB.clone();
}

impl FindAuthor<<Test as frame_system::Config>::AccountId> for Test {
	fn find_author<'a, I>(_digests: I) -> Option<<Test as frame_system::Config>::AccountId>
	where
		I: 'a + IntoIterator<Item = (frame_support::ConsensusEngineId, &'a [u8])>,
	{
		Some(EVE)
	}
}

#[derive_impl(crate::config_preludes::TestDefaultConfig)]
impl Config for Test {
	type Time = Timestamp;
	type AddressMapper = AccountId32Mapper<Self>;
	type Currency = Balances;
	type DepositPerByte = DepositPerByte;
	type DepositPerItem = DepositPerItem;
	type UnsafeUnstableInterface = UnstableInterface;
	type UploadOrigin = EnsureAccount<Self, UploadAccount>;
	type InstantiateOrigin = EnsureAccount<Self, InstantiateAccount>;
	type CodeHashLockupDepositPercent = CodeHashLockupDepositPercent;
	type ChainId = ChainId;
	type FindAuthor = Test;
	type Precompiles = (precompiles::WithInfo<Self>, precompiles::NoInfo<Self>);
}

impl TryFrom<RuntimeCall> for crate::Call<Test> {
	type Error = ();

	fn try_from(value: RuntimeCall) -> Result<Self, Self::Error> {
		match value {
			RuntimeCall::Contracts(call) => Ok(call),
			_ => Err(()),
		}
	}
}

pub struct ExtBuilder {
	existential_deposit: u64,
	storage_version: Option<StorageVersion>,
	code_hashes: Vec<sp_core::H256>,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self {
			existential_deposit: ExistentialDeposit::get(),
			storage_version: None,
			code_hashes: vec![],
		}
	}
}

impl ExtBuilder {
	pub fn existential_deposit(mut self, existential_deposit: u64) -> Self {
		self.existential_deposit = existential_deposit;
		self
	}
	pub fn with_code_hashes(mut self, code_hashes: Vec<sp_core::H256>) -> Self {
		self.code_hashes = code_hashes;
		self
	}
	pub fn set_associated_consts(&self) {
		EXISTENTIAL_DEPOSIT.with(|v| *v.borrow_mut() = self.existential_deposit);
	}
	pub fn build(self) -> sp_io::TestExternalities {
		sp_tracing::try_init_simple();
		self.set_associated_consts();
		let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
		let checking_account = Pallet::<Test>::checking_account();

		pallet_balances::GenesisConfig::<Test> {
			balances: vec![(checking_account.clone(), 1_000_000_000_000)],
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();

		crate::GenesisConfig::<Test>::default().assimilate_storage(&mut t).unwrap();
		let mut ext = sp_io::TestExternalities::new(t);
		ext.register_extension(KeystoreExt::new(MemoryKeystore::new()));
		ext.execute_with(|| {
			use frame_support::traits::OnGenesis;

			Pallet::<Test>::on_genesis();
			if let Some(storage_version) = self.storage_version {
				storage_version.put::<Pallet<Test>>();
			}
			System::set_block_number(1)
		});
		ext.execute_with(|| {
			for code_hash in self.code_hashes {
				CodeInfoOf::<Test>::insert(code_hash, crate::CodeInfo::new(ALICE));
			}
		});
		ext.execute_with(|| {
			assert_ok!(Pallet::<Test>::map_account(RuntimeOrigin::signed(checking_account)));
		});
		ext
	}
}

fn initialize_block(number: u64) {
	System::reset_events();
	System::initialize(&number, &[0u8; 32].into(), &Default::default());
}

impl Default for Origin<Test> {
	fn default() -> Self {
		Self::Signed(ALICE)
	}
}

#[test]
fn transfer_with_dust_works() {
	struct TestCase {
		description: &'static str,
		from_balance: BalanceWithDust<u64>,
		to_balance: BalanceWithDust<u64>,
		amount: BalanceWithDust<u64>,
		expected_from_balance: BalanceWithDust<u64>,
		expected_to_balance: BalanceWithDust<u64>,
		total_issuance_diff: i64,
	}

	let plank: u32 = <Test as Config>::NativeToEthRatio::get();

	let test_cases = vec![
		TestCase {
			description: "without dust",
			from_balance: BalanceWithDust::new_unchecked::<Test>(100, 0),
			to_balance: BalanceWithDust::new_unchecked::<Test>(0, 0),
			amount: BalanceWithDust::new_unchecked::<Test>(1, 0),
			expected_from_balance: BalanceWithDust::new_unchecked::<Test>(99, 0),
			expected_to_balance: BalanceWithDust::new_unchecked::<Test>(1, 0),
			total_issuance_diff: 0,
		},
		TestCase {
			description: "with dust",
			from_balance: BalanceWithDust::new_unchecked::<Test>(100, 0),
			to_balance: BalanceWithDust::new_unchecked::<Test>(0, 0),
			amount: BalanceWithDust::new_unchecked::<Test>(1, 10),
			expected_from_balance: BalanceWithDust::new_unchecked::<Test>(98, plank - 10),
			expected_to_balance: BalanceWithDust::new_unchecked::<Test>(1, 10),
			total_issuance_diff: 1,
		},
		TestCase {
			description: "just dust",
			from_balance: BalanceWithDust::new_unchecked::<Test>(100, 0),
			to_balance: BalanceWithDust::new_unchecked::<Test>(0, 0),
			amount: BalanceWithDust::new_unchecked::<Test>(0, 10),
			expected_from_balance: BalanceWithDust::new_unchecked::<Test>(99, plank - 10),
			expected_to_balance: BalanceWithDust::new_unchecked::<Test>(0, 10),
			total_issuance_diff: 1,
		},
		TestCase {
			description: "with existing dust",
			from_balance: BalanceWithDust::new_unchecked::<Test>(100, 5),
			to_balance: BalanceWithDust::new_unchecked::<Test>(0, plank - 5),
			amount: BalanceWithDust::new_unchecked::<Test>(1, 10),
			expected_from_balance: BalanceWithDust::new_unchecked::<Test>(98, plank - 5),
			expected_to_balance: BalanceWithDust::new_unchecked::<Test>(2, 5),
			total_issuance_diff: 0,
		},
		TestCase {
			description: "with enough existing dust",
			from_balance: BalanceWithDust::new_unchecked::<Test>(100, 10),
			to_balance: BalanceWithDust::new_unchecked::<Test>(0, plank - 10),
			amount: BalanceWithDust::new_unchecked::<Test>(1, 10),
			expected_from_balance: BalanceWithDust::new_unchecked::<Test>(99, 0),
			expected_to_balance: BalanceWithDust::new_unchecked::<Test>(2, 0),
			total_issuance_diff: -1,
		},
	];

	for TestCase {
		description,
		from_balance,
		to_balance,
		amount,
		expected_from_balance,
		expected_to_balance,
		total_issuance_diff,
	} in test_cases.into_iter()
	{
		ExtBuilder::default().build().execute_with(|| {
			test_utils::set_balance_with_dust(&ALICE_ADDR, from_balance);
			test_utils::set_balance_with_dust(&BOB_ADDR, to_balance);

			let total_issuance = <Test as Config>::Currency::total_issuance();
			let evm_value = Pallet::<Test>::convert_native_to_evm(amount);

			let (value, dust) = amount.deconstruct();
			assert_eq!(Pallet::<Test>::has_dust(evm_value), !dust.is_zero());
			assert_eq!(Pallet::<Test>::has_balance(evm_value), !value.is_zero());

			let result =
				builder::bare_call(BOB_ADDR).evm_value(evm_value).build_and_unwrap_result();
			assert_eq!(result, Default::default(), "{description} tx failed");

			assert_eq!(
				Pallet::<Test>::evm_balance(&ALICE_ADDR),
				Pallet::<Test>::convert_native_to_evm(expected_from_balance),
				"{description}: invalid from balance"
			);

			assert_eq!(
				Pallet::<Test>::evm_balance(&BOB_ADDR),
				Pallet::<Test>::convert_native_to_evm(expected_to_balance),
				"{description}: invalid to balance"
			);

			assert_eq!(
				total_issuance as i64 - total_issuance_diff,
				<Test as Config>::Currency::total_issuance() as i64,
				"{description}: total issuance should match"
			);
		});
	}
}

#[test]
fn eth_call_transfer_with_dust_works() {
	let (binary, _) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		let balance =
			Pallet::<Test>::convert_native_to_evm(BalanceWithDust::new_unchecked::<Test>(100, 10));
		assert_ok!(builder::eth_call(addr).value(balance).build());

		assert_eq!(Pallet::<Test>::evm_balance(&addr), balance);
	});
}

#[test]
fn contract_call_transfer_with_dust_works() {
	let (binary_caller, _code_hash_caller) = compile_module("call_with_value").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller))
				.native_value(200)
				.build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		let balance =
			Pallet::<Test>::convert_native_to_evm(BalanceWithDust::new_unchecked::<Test>(100, 10));
		assert_ok!(builder::call(addr_caller).data((balance, addr_callee).encode()).build());

		assert_eq!(Pallet::<Test>::evm_balance(&addr_callee), balance);
	});
}

#[test]
fn instantiate_and_call_and_deposit_event() {
	let (binary, code_hash) = compile_module("event_and_return_on_deploy").unwrap();

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();
		let value = 100;

		// We determine the storage deposit limit after uploading because it depends on ALICEs
		// free balance which is changed by uploading a module.
		assert_ok!(Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			binary,
			deposit_limit::<Test>(),
		));

		// Drop previous events
		initialize_block(2);

		// Check at the end to get hash on error easily
		let Contract { addr, account_id } = builder::bare_instantiate(Code::Existing(code_hash))
			.native_value(value)
			.build_and_unwrap_contract();
		assert!(AccountInfoOf::<Test>::contains_key(&addr));

		let hold_balance = test_utils::contract_base_deposit(&addr);

		assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::System(frame_system::Event::NewAccount {
						account: account_id.clone()
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Endowed {
						account: account_id.clone(),
						free_balance: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: ALICE,
						to: account_id.clone(),
						amount: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: ALICE,
						to: account_id.clone(),
						amount: value,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Contracts(crate::Event::ContractEmitted {
						contract: addr,
						data: vec![1, 2, 3, 4],
						topics: vec![H256::repeat_byte(42)],
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Contracts(crate::Event::Instantiated {
						deployer: ALICE_ADDR,
						contract: addr
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferAndHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: ALICE,
						dest: account_id.clone(),
						transferred: hold_balance,
					}),
					topics: vec![],
				},
			]
		);
	});
}

#[test]
fn create1_address_from_extrinsic() {
	let (binary, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		assert_ok!(Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			binary.clone(),
			deposit_limit::<Test>(),
		));

		assert_eq!(System::account_nonce(&ALICE), 0);
		System::inc_account_nonce(&ALICE);

		for nonce in 1..3 {
			let Contract { addr, .. } = builder::bare_instantiate(Code::Existing(code_hash))
				.salt(None)
				.build_and_unwrap_contract();
			assert!(AccountInfoOf::<Test>::contains_key(&addr));
			assert_eq!(
				addr,
				create1(&<Test as Config>::AddressMapper::to_address(&ALICE), nonce - 1)
			);
		}
		assert_eq!(System::account_nonce(&ALICE), 3);

		for nonce in 3..6 {
			let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary.clone()))
				.salt(None)
				.build_and_unwrap_contract();
			assert!(AccountInfoOf::<Test>::contains_key(&addr));
			assert_eq!(
				addr,
				create1(&<Test as Config>::AddressMapper::to_address(&ALICE), nonce - 1)
			);
		}
		assert_eq!(System::account_nonce(&ALICE), 6);
	});
}

#[test]
fn deposit_event_max_value_limit() {
	let (binary, _code_hash) = compile_module("event_size").unwrap();

	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		// Create
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(30_000)
			.build_and_unwrap_contract();

		// Call contract with allowed storage value.
		assert_ok!(builder::call(addr)
			.gas_limit(GAS_LIMIT.set_ref_time(GAS_LIMIT.ref_time() * 2)) // we are copying a huge buffer,
			.data(limits::PAYLOAD_BYTES.encode())
			.build());

		// Call contract with too large a storage value.
		assert_err_ignore_postinfo!(
			builder::call(addr).data((limits::PAYLOAD_BYTES + 1).encode()).build(),
			Error::<Test>::ValueTooLarge,
		);
	});
}

// Fail out of fuel (ref_time weight) in the engine.
#[test]
fn run_out_of_fuel_engine() {
	let (binary, _code_hash) = compile_module("run_out_of_gas").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(100 * min_balance)
			.build_and_unwrap_contract();

		// Call the contract with a fixed gas limit. It must run out of gas because it just
		// loops forever.
		assert_err_ignore_postinfo!(
			builder::call(addr)
				.gas_limit(Weight::from_parts(10_000_000_000, u64::MAX))
				.build(),
			Error::<Test>::OutOfGas,
		);
	});
}

// Fail out of fuel (ref_time weight) in the host.
#[test]
fn run_out_of_fuel_host() {
	use crate::precompiles::Precompile;
	use alloy_core::sol_types::SolInterface;
	use precompiles::{INoInfo, NoInfo};

	let precompile_addr = H160(NoInfo::<Test>::MATCHER.base_address());
	let input = INoInfo::INoInfoCalls::consumeMaxGas(INoInfo::consumeMaxGasCall {}).abi_encode();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let result = builder::bare_call(precompile_addr).data(input).build().result;
		assert_err!(result, <Error<Test>>::OutOfGas);
	});
}

#[test]
fn gas_syncs_work() {
	let (code, _code_hash) = compile_module("caller_is_origin_n").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let contract = builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let result = builder::bare_call(contract.addr).data(0u32.encode()).build();
		assert_ok!(result.result);
		let engine_consumed_noop = result.gas_consumed.ref_time();

		let result = builder::bare_call(contract.addr).data(1u32.encode()).build();
		assert_ok!(result.result);
		let gas_consumed_once = result.gas_consumed.ref_time();
		let host_consumed_once = <Test as Config>::WeightInfo::seal_caller_is_origin().ref_time();
		let engine_consumed_once = gas_consumed_once - host_consumed_once - engine_consumed_noop;

		let result = builder::bare_call(contract.addr).data(2u32.encode()).build();
		assert_ok!(result.result);
		let gas_consumed_twice = result.gas_consumed.ref_time();
		let host_consumed_twice = host_consumed_once * 2;
		let engine_consumed_twice = gas_consumed_twice - host_consumed_twice - engine_consumed_noop;

		// Second contract just repeats first contract's instructions twice.
		// If runtime syncs gas with the engine properly, this should pass.
		assert_eq!(engine_consumed_twice, engine_consumed_once * 2);
	});
}

/// Check that contracts with the same account id have different trie ids.
/// Check the `Nonce` storage item for more information.
#[test]
fn instantiate_unique_trie_id() {
	let (binary, code_hash) = compile_module("self_destruct").unwrap();

	ExtBuilder::default().existential_deposit(500).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		Contracts::upload_code(RuntimeOrigin::signed(ALICE), binary, deposit_limit::<Test>())
			.unwrap();

		// Instantiate the contract and store its trie id for later comparison.
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Existing(code_hash)).build_and_unwrap_contract();
		let trie_id = get_contract(&addr).trie_id;

		// Try to instantiate it again without termination should yield an error.
		assert_err_ignore_postinfo!(
			builder::instantiate(code_hash).build(),
			<Error<Test>>::DuplicateContract,
		);

		// Terminate the contract.
		assert_ok!(builder::call(addr).build());

		// Re-Instantiate after termination.
		assert_ok!(builder::instantiate(code_hash).build());

		// Trie ids shouldn't match or we might have a collision
		assert_ne!(trie_id, get_contract(&addr).trie_id);
	});
}

#[test]
fn storage_work() {
	let (code, _code_hash) = compile_module("storage").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		builder::bare_call(addr).build_and_unwrap_result();
	});
}

#[test]
fn storage_max_value_limit() {
	let (binary, _code_hash) = compile_module("storage_size").unwrap();

	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		// Create
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(30_000)
			.build_and_unwrap_contract();
		get_contract(&addr);

		// Call contract with allowed storage value.
		assert_ok!(builder::call(addr)
			.gas_limit(GAS_LIMIT.set_ref_time(GAS_LIMIT.ref_time() * 2)) // we are copying a huge buffer
			.data(limits::PAYLOAD_BYTES.encode())
			.build());

		// Call contract with too large a storage value.
		assert_err_ignore_postinfo!(
			builder::call(addr).data((limits::PAYLOAD_BYTES + 1).encode()).build(),
			Error::<Test>::ValueTooLarge,
		);
	});
}

#[test]
fn clear_storage_on_zero_value() {
	let (code, _code_hash) = compile_module("clear_storage_on_zero_value").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		builder::bare_call(addr).build_and_unwrap_result();
	});
}

#[test]
fn transient_storage_work() {
	let (code, _code_hash) = compile_module("transient_storage").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		builder::bare_call(addr).build_and_unwrap_result();
	});
}

#[test]
fn transient_storage_limit_in_call() {
	let (binary_caller, _code_hash_caller) =
		compile_module("create_transient_storage_and_call").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("set_transient_storage").unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create both contracts: Constructors do nothing.
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller)).build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		// Call contracts with storage values within the limit.
		// Caller and Callee contracts each set a transient storage value of size 100.
		assert_ok!(builder::call(addr_caller)
			.data((100u32, 100u32, &addr_callee).encode())
			.build(),);

		// Call a contract with a storage value that is too large.
		// Limit exceeded in the caller contract.
		assert_err_ignore_postinfo!(
			builder::call(addr_caller)
				.data((4u32 * 1024u32, 200u32, &addr_callee).encode())
				.build(),
			<Error<Test>>::OutOfTransientStorage,
		);

		// Call a contract with a storage value that is too large.
		// Limit exceeded in the callee contract.
		assert_err_ignore_postinfo!(
			builder::call(addr_caller)
				.data((50u32, 4 * 1024u32, &addr_callee).encode())
				.build(),
			<Error<Test>>::ContractTrapped
		);
	});
}

#[test]
fn deploy_and_call_other_contract() {
	let (caller_binary, _caller_code_hash) = compile_module("caller_contract").unwrap();
	let (callee_binary, callee_code_hash) = compile_module("return_with_data").unwrap();
	let code_load_weight = crate::vm::code_load_weight(callee_binary.len() as u32);

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let min_balance = Contracts::min_balance();

		// Create
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let Contract { addr: caller_addr, account_id: caller_account } =
			builder::bare_instantiate(Code::Upload(caller_binary))
				.native_value(100_000)
				.build_and_unwrap_contract();

		let callee_addr = create2(
			&caller_addr,
			&callee_binary,
			&[0, 1, 34, 51, 68, 85, 102, 119], // hard coded in binary
			&[0u8; 32],
		);
		let callee_account = <Test as Config>::AddressMapper::to_account_id(&callee_addr);

		Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			callee_binary,
			deposit_limit::<Test>(),
		)
		.unwrap();

		// Drop previous events
		initialize_block(2);

		// Call BOB contract, which attempts to instantiate and call the callee contract and
		// makes various assertions on the results from those calls.
		assert_ok!(builder::call(caller_addr)
			.data(
				(callee_code_hash, code_load_weight.ref_time(), code_load_weight.proof_size())
					.encode()
			)
			.build());

		assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::System(frame_system::Event::NewAccount {
						account: callee_account.clone()
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Endowed {
						account: callee_account.clone(),
						free_balance: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: ALICE,
						to: callee_account.clone(),
						amount: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: caller_account.clone(),
						to: callee_account.clone(),
						amount: 32768 // hardcoded in binary
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: caller_account.clone(),
						to: callee_account.clone(),
						amount: 32768,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferAndHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: ALICE,
						dest: callee_account.clone(),
						transferred: 555,
					}),
					topics: vec![],
				},
			]
		);
	});
}

#[test]
fn delegate_call() {
	let (caller_binary, _caller_code_hash) = compile_module("delegate_call").unwrap();
	let (callee_binary, _callee_code_hash) = compile_module("delegate_call_lib").unwrap();

	ExtBuilder::default().existential_deposit(500).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the 'caller'
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_binary))
				.native_value(300_000)
				.build_and_unwrap_contract();

		// Instantiate the 'callee'
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_binary))
				.native_value(100_000)
				.build_and_unwrap_contract();

		assert_ok!(builder::call(caller_addr)
			.value(1337)
			.data((callee_addr, u64::MAX, u64::MAX).encode())
			.build());
	});
}

#[test]
fn delegate_call_non_existant_is_noop() {
	let (caller_binary, _caller_code_hash) = compile_module("delegate_call_simple").unwrap();

	ExtBuilder::default().existential_deposit(500).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the 'caller'
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_binary))
				.native_value(300_000)
				.build_and_unwrap_contract();

		assert_ok!(builder::call(caller_addr)
			.value(1337)
			.data((BOB_ADDR, u64::MAX, u64::MAX).encode())
			.build());

		assert_eq!(test_utils::get_balance(&BOB_FALLBACK), 0);
	});
}

#[test]
fn delegate_call_with_weight_limit() {
	let (caller_binary, _caller_code_hash) = compile_module("delegate_call").unwrap();
	let (callee_binary, _callee_code_hash) = compile_module("delegate_call_lib").unwrap();

	ExtBuilder::default().existential_deposit(500).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the 'caller'
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_binary))
				.native_value(300_000)
				.build_and_unwrap_contract();

		// Instantiate the 'callee'
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_binary))
				.native_value(100_000)
				.build_and_unwrap_contract();

		// fails, not enough weight
		assert_err!(
			builder::bare_call(caller_addr)
				.native_value(1337)
				.data((callee_addr, 100u64, 100u64).encode())
				.build()
				.result,
			Error::<Test>::ContractTrapped,
		);

		assert_ok!(builder::call(caller_addr)
			.value(1337)
			.data((callee_addr, 500_000_000u64, 100_000u64).encode())
			.build());
	});
}

#[test]
fn delegate_call_with_deposit_limit() {
	let (caller_binary, _caller_code_hash) = compile_module("delegate_call_deposit_limit").unwrap();
	let (callee_binary, _callee_code_hash) = compile_module("delegate_call_lib").unwrap();

	ExtBuilder::default().existential_deposit(500).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the 'caller'
		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(caller_binary))
				.native_value(300_000)
				.build_and_unwrap_contract();

		// Instantiate the 'callee'
		let Contract { addr: callee_addr, .. } =
			builder::bare_instantiate(Code::Upload(callee_binary))
				.native_value(100_000)
				.build_and_unwrap_contract();

		// Delegate call will write 1 storage and deposit of 2 (1 item) + 32 (bytes) is required.
		// + 32 + 16 for blake2_128concat
		// Fails, not enough deposit
		let ret = builder::bare_call(caller_addr)
			.native_value(1337)
			.data((callee_addr, 81u64).encode())
			.build_and_unwrap_result();
		assert_return_code!(ret, RuntimeReturnCode::OutOfResources);

		assert_ok!(builder::call(caller_addr)
			.value(1337)
			.data((callee_addr, 82u64).encode())
			.build());
	});
}

#[test]
fn transfer_expendable_cannot_kill_account() {
	let (binary, _code_hash) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the BOB contract.
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(1_000)
			.build_and_unwrap_contract();

		// Check that the BOB contract has been instantiated.
		get_contract(&addr);

		let account = <Test as Config>::AddressMapper::to_account_id(&addr);
		let total_balance = <Test as Config>::Currency::total_balance(&account);

		assert_eq!(
			test_utils::get_balance_on_hold(&HoldReason::StorageDepositReserve.into(), &account),
			test_utils::contract_base_deposit(&addr)
		);

		// Some or the total balance is held, so it can't be transferred.
		assert_err!(
			<<Test as Config>::Currency as Mutate<AccountId32>>::transfer(
				&account,
				&ALICE,
				total_balance,
				Preservation::Expendable,
			),
			TokenError::FundsUnavailable,
		);

		assert_eq!(<Test as Config>::Currency::total_balance(&account), total_balance);
	});
}

#[test]
fn cannot_self_destruct_through_draining() {
	let (binary, _code_hash) = compile_module("drain").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let value = 1_000;
		let min_balance = Contracts::min_balance();

		// Instantiate the BOB contract.
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(value)
			.build_and_unwrap_contract();
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);

		// Check that the BOB contract has been instantiated.
		get_contract(&addr);

		// Call BOB which makes it send all funds to the zero address
		// The contract code asserts that the transfer fails with the correct error code
		assert_ok!(builder::call(addr).build());

		// Make sure the account wasn't remove by sending all free balance away.
		assert_eq!(
			<Test as Config>::Currency::total_balance(&account),
			value + test_utils::contract_base_deposit(&addr) + min_balance,
		);
	});
}

#[test]
fn cannot_self_destruct_through_storage_refund_after_price_change() {
	let (binary, _code_hash) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();

		// Instantiate the BOB contract.
		let contract = builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();
		let info_deposit = test_utils::contract_base_deposit(&contract.addr);

		// Check that the contract has been instantiated and has the minimum balance
		assert_eq!(get_contract(&contract.addr).total_deposit(), info_deposit);
		assert_eq!(get_contract(&contract.addr).extra_deposit(), 0);
		assert_eq!(
			<Test as Config>::Currency::total_balance(&contract.account_id),
			info_deposit + min_balance
		);

		// Create 100 (16 + 32 bytes for key for blake128 concat) bytes of storage with a
		// price of per byte and a single storage item of price 2
		assert_ok!(builder::call(contract.addr).data(100u32.to_le_bytes().to_vec()).build());
		assert_eq!(get_contract(&contract.addr).total_deposit(), info_deposit + 100 + 16 + 32 + 2);

		// Increase the byte price and trigger a refund. This should not have any influence
		// because the removal is pro rata and exactly those 100 bytes should have been
		// removed as we didn't delete the key.
		DEPOSIT_PER_BYTE.with(|c| *c.borrow_mut() = 500);
		assert_ok!(builder::call(contract.addr).data(0u32.to_le_bytes().to_vec()).build());

		// Make sure the account wasn't removed by the refund
		assert_eq!(
			<Test as Config>::Currency::total_balance(&contract.account_id),
			get_contract(&contract.addr).total_deposit() + min_balance,
		);
		// + 1 because due to fixed point arithmetic we can sometimes refund
		// one unit to little
		assert_eq!(get_contract(&contract.addr).extra_deposit(), 16 + 32 + 2 + 1);
	});
}

#[test]
fn cannot_self_destruct_while_live() {
	let (binary, _code_hash) = compile_module("self_destruct").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the BOB contract.
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(100_000)
			.build_and_unwrap_contract();

		// Check that the BOB contract has been instantiated.
		get_contract(&addr);

		// Call BOB with input data, forcing it make a recursive call to itself to
		// self-destruct, resulting in a trap.
		assert_err_ignore_postinfo!(
			builder::call(addr).data(vec![0]).build(),
			Error::<Test>::ContractTrapped,
		);

		// Check that BOB is still there.
		get_contract(&addr);
	});
}

#[test]
fn self_destruct_works() {
	let (binary, code_hash) = compile_module("self_destruct").unwrap();
	ExtBuilder::default().existential_deposit(1_000).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let _ = <Test as Config>::Currency::set_balance(&DJANGO_FALLBACK, 1_000_000);
		let min_balance = Contracts::min_balance();

		// Instantiate the BOB contract.
		let contract = builder::bare_instantiate(Code::Upload(binary))
			.native_value(100_000)
			.build_and_unwrap_contract();

		let hold_balance = test_utils::contract_base_deposit(&contract.addr);

		// Check that the BOB contract has been instantiated.
		let _ = get_contract(&contract.addr);

		// Drop all previous events
		initialize_block(2);

		// Call BOB without input data which triggers termination.
		assert_matches!(builder::call(contract.addr).build(), Ok(_));

		// Check that code is still there but refcount dropped to zero.
		assert_refcount!(&code_hash, 0);

		// Check that account is gone
		assert!(get_contract_checked(&contract.addr).is_none());
		assert_eq!(<Test as Config>::Currency::total_balance(&contract.account_id), 0);

		// Check that the beneficiary (django) got remaining balance.
		assert_eq!(
			<Test as Config>::Currency::free_balance(DJANGO_FALLBACK),
			1_000_000 + 100_000 + min_balance
		);

		// Check that the Alice is missing Django's benefit. Within ALICE's total balance
		// there's also the code upload deposit held.
		assert_eq!(
			<Test as Config>::Currency::total_balance(&ALICE),
			1_000_000 - (100_000 + min_balance)
		);

		pretty_assertions::assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferOnHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: contract.account_id.clone(),
						dest: ALICE,
						amount: hold_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::System(frame_system::Event::KilledAccount {
						account: contract.account_id.clone()
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: contract.account_id.clone(),
						to: DJANGO_FALLBACK,
						amount: 100_000 + min_balance,
					}),
					topics: vec![],
				},
			],
		);
	});
}

// This tests that one contract cannot prevent another from self-destructing by sending it
// additional funds after it has been drained.
#[test]
fn destroy_contract_and_transfer_funds() {
	let (callee_binary, callee_code_hash) = compile_module("self_destruct").unwrap();
	let (caller_binary, _caller_code_hash) = compile_module("destroy_and_transfer").unwrap();

	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		// Create code hash for bob to instantiate
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			callee_binary.clone(),
			deposit_limit::<Test>(),
		)
		.unwrap();

		// This deploys the BOB contract, which in turn deploys the CHARLIE contract during
		// construction.
		let Contract { addr: addr_bob, .. } =
			builder::bare_instantiate(Code::Upload(caller_binary))
				.native_value(200_000)
				.data(callee_code_hash.as_ref().to_vec())
				.build_and_unwrap_contract();

		// Check that the CHARLIE contract has been instantiated.
		let salt = [47; 32]; // hard coded in fixture.
		let addr_charlie = create2(&addr_bob, &callee_binary, &[], &salt);
		get_contract(&addr_charlie);

		// Call BOB, which calls CHARLIE, forcing CHARLIE to self-destruct.
		assert_ok!(builder::call(addr_bob).data(addr_charlie.encode()).build());

		// Check that CHARLIE has moved on to the great beyond (ie. died).
		assert!(get_contract_checked(&addr_charlie).is_none());
	});
}

#[test]
fn cannot_self_destruct_in_constructor() {
	let (binary, _) = compile_module("self_destructing_constructor").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Fail to instantiate the BOB because the constructor calls seal_terminate.
		assert_err_ignore_postinfo!(
			builder::instantiate_with_code(binary).value(100_000).build(),
			Error::<Test>::TerminatedInConstructor,
		);
	});
}

#[test]
fn crypto_hashes() {
	let (binary, _code_hash) = compile_module("crypto_hashes").unwrap();

	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the CRYPTO_HASHES contract.
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(100_000)
			.build_and_unwrap_contract();
		// Perform the call.
		let input = b"_DEAD_BEEF";
		use sp_io::hashing::*;
		// Wraps a hash function into a more dynamic form usable for testing.
		macro_rules! dyn_hash_fn {
			($name:ident) => {
				Box::new(|input| $name(input).as_ref().to_vec().into_boxed_slice())
			};
		}
		// All hash functions and their associated output byte lengths.
		let test_cases: &[(u8, Box<dyn Fn(&[u8]) -> Box<[u8]>>, usize)] = &[
			(2, dyn_hash_fn!(keccak_256), 32),
			(3, dyn_hash_fn!(blake2_256), 32),
			(4, dyn_hash_fn!(blake2_128), 16),
		];
		// Test the given hash functions for the input: "_DEAD_BEEF"
		for (n, hash_fn, expected_size) in test_cases.iter() {
			let mut params = vec![*n];
			params.extend_from_slice(input);
			let result = builder::bare_call(addr).data(params).build_and_unwrap_result();
			assert!(!result.did_revert());
			let expected = hash_fn(input.as_ref());
			assert_eq!(&result.data[..*expected_size], &*expected);
		}
	})
}

#[test]
fn transfer_return_code() {
	let (binary, _code_hash) = compile_module("transfer_return_code").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);

		let contract = builder::bare_instantiate(Code::Upload(binary))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		// Contract has only the minimal balance so any transfer will fail.
		<Test as Config>::Currency::set_balance(&contract.account_id, min_balance);
		let result = builder::bare_call(contract.addr).build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::TransferFailed);
	});
}

#[test]
fn call_return_code() {
	use test_utils::u256_bytes;

	let (caller_code, _caller_hash) = compile_module("call_return_code").unwrap();
	let (callee_code, _callee_hash) = compile_module("ok_trap_revert").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);
		let _ = <Test as Config>::Currency::set_balance(&CHARLIE, 1000 * min_balance);

		let bob = builder::bare_instantiate(Code::Upload(caller_code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		// BOB cannot pay the ed which is needed to pull DJANGO into existence
		// this does trap the caller instead of returning an error code
		// reasoning is that this error state does not exist on eth where
		// ed does not exist. We hide this fact from the contract.
		let result = builder::bare_call(bob.addr)
			.data((DJANGO_ADDR, u256_bytes(1)).encode())
			.origin(RuntimeOrigin::signed(BOB))
			.build();
		assert_err!(result.result, <Error<Test>>::StorageDepositNotEnoughFunds);

		// Contract calls into Django which is no valid contract
		// This will be a balance transfer into a new account
		// with more than the contract has which will make the transfer fail
		let value = Pallet::<Test>::convert_native_to_evm(min_balance * 200);
		let result = builder::bare_call(bob.addr)
			.data(
				AsRef::<[u8]>::as_ref(&DJANGO_ADDR)
					.iter()
					.chain(&value.to_little_endian())
					.cloned()
					.collect(),
			)
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::TransferFailed);

		// Sending below the minimum balance should result in success.
		// The ED is charged from the call origin.
		let alice_before = test_utils::get_balance(&ALICE_FALLBACK);
		assert_eq!(test_utils::get_balance(&DJANGO_FALLBACK), 0);

		let value = Pallet::<Test>::convert_native_to_evm(1u64);
		let result = builder::bare_call(bob.addr)
			.data(
				AsRef::<[u8]>::as_ref(&DJANGO_ADDR)
					.iter()
					.chain(&value.to_little_endian())
					.cloned()
					.collect(),
			)
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::Success);
		assert_eq!(test_utils::get_balance(&DJANGO_FALLBACK), min_balance + 1);
		assert_eq!(test_utils::get_balance(&ALICE_FALLBACK), alice_before - min_balance);

		let django = builder::bare_instantiate(Code::Upload(callee_code))
			.origin(RuntimeOrigin::signed(CHARLIE))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		// Sending more than the contract has will make the transfer fail.
		let value = Pallet::<Test>::convert_native_to_evm(min_balance * 300);
		let result = builder::bare_call(bob.addr)
			.data(
				AsRef::<[u8]>::as_ref(&django.addr)
					.iter()
					.chain(&value.to_little_endian())
					.chain(&0u32.to_le_bytes())
					.cloned()
					.collect(),
			)
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::TransferFailed);

		// Contract has enough balance but callee reverts because "1" is passed.
		<Test as Config>::Currency::set_balance(&bob.account_id, min_balance + 1000);
		let value = Pallet::<Test>::convert_native_to_evm(5u64);
		let result = builder::bare_call(bob.addr)
			.data(
				AsRef::<[u8]>::as_ref(&django.addr)
					.iter()
					.chain(&value.to_little_endian())
					.chain(&1u32.to_le_bytes())
					.cloned()
					.collect(),
			)
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::CalleeReverted);

		// Contract has enough balance but callee traps because "2" is passed.
		let result = builder::bare_call(bob.addr)
			.data(
				AsRef::<[u8]>::as_ref(&django.addr)
					.iter()
					.chain(&value.to_little_endian())
					.chain(&2u32.to_le_bytes())
					.cloned()
					.collect(),
			)
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::CalleeTrapped);
	});
}

#[test]
fn instantiate_return_code() {
	let (caller_code, _caller_hash) = compile_module("instantiate_return_code").unwrap();
	let (callee_code, callee_hash) = compile_module("ok_trap_revert").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);
		let _ = <Test as Config>::Currency::set_balance(&CHARLIE, 1000 * min_balance);
		let callee_hash = callee_hash.as_ref().to_vec();

		assert_ok!(builder::instantiate_with_code(callee_code).value(min_balance * 100).build());

		let contract = builder::bare_instantiate(Code::Upload(caller_code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		// bob cannot pay the ED to create the contract as he has no money
		// this traps the caller rather than returning an error
		let result = builder::bare_call(contract.addr)
			.data(callee_hash.iter().chain(&0u32.to_le_bytes()).cloned().collect())
			.origin(RuntimeOrigin::signed(BOB))
			.build();
		assert_err!(result.result, <Error<Test>>::StorageDepositNotEnoughFunds);

		// Contract has only the minimal balance so any transfer will fail.
		<Test as Config>::Currency::set_balance(&contract.account_id, min_balance);
		let result = builder::bare_call(contract.addr)
			.data(callee_hash.iter().chain(&0u32.to_le_bytes()).cloned().collect())
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::TransferFailed);

		// Contract has enough balance but the passed code hash is invalid
		<Test as Config>::Currency::set_balance(&contract.account_id, min_balance + 10_000);
		let result = builder::bare_call(contract.addr).data(vec![0; 36]).build();
		assert_err!(result.result, <Error<Test>>::CodeNotFound);

		// Contract has enough balance but callee reverts because "1" is passed.
		let result = builder::bare_call(contract.addr)
			.data(callee_hash.iter().chain(&1u32.to_le_bytes()).cloned().collect())
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::CalleeReverted);

		// Contract has enough balance but callee traps because "2" is passed.
		let result = builder::bare_call(contract.addr)
			.data(callee_hash.iter().chain(&2u32.to_le_bytes()).cloned().collect())
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::CalleeTrapped);

		// Contract instantiation succeeds
		let result = builder::bare_call(contract.addr)
			.data(callee_hash.iter().chain(&0u32.to_le_bytes()).cloned().collect())
			.build_and_unwrap_result();
		assert_return_code!(result, 0);

		// Contract instantiation fails because the same salt is being used again.
		let result = builder::bare_call(contract.addr)
			.data(callee_hash.iter().chain(&0u32.to_le_bytes()).cloned().collect())
			.build_and_unwrap_result();
		assert_return_code!(result, RuntimeReturnCode::DuplicateContractAddress);
	});
}

#[test]
fn lazy_removal_works() {
	let (code, _hash) = compile_module("self_destruct").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);

		let contract = builder::bare_instantiate(Code::Upload(code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		let info = get_contract(&contract.addr);
		let trie = &info.child_trie_info();

		// Put value into the contracts child trie
		child::put(trie, &[99], &42);

		// Terminate the contract
		assert_ok!(builder::call(contract.addr).build());

		// Contract info should be gone
		assert!(!<AccountInfoOf::<Test>>::contains_key(&contract.addr));

		// But value should be still there as the lazy removal did not run, yet.
		assert_matches!(child::get(trie, &[99]), Some(42));

		// Run the lazy removal
		Contracts::on_idle(System::block_number(), Weight::MAX);

		// Value should be gone now
		assert_matches!(child::get::<i32>(trie, &[99]), None);
	});
}

#[test]
fn lazy_batch_removal_works() {
	let (code, _hash) = compile_module("self_destruct").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);
		let mut tries: Vec<child::ChildInfo> = vec![];

		for i in 0..3u8 {
			let contract = builder::bare_instantiate(Code::Upload(code.clone()))
				.native_value(min_balance * 100)
				.salt(Some([i; 32]))
				.build_and_unwrap_contract();

			let info = get_contract(&contract.addr);
			let trie = &info.child_trie_info();

			// Put value into the contracts child trie
			child::put(trie, &[99], &42);

			// Terminate the contract. Contract info should be gone, but value should be still
			// there as the lazy removal did not run, yet.
			assert_ok!(builder::call(contract.addr).build());

			assert!(!<AccountInfoOf::<Test>>::contains_key(&contract.addr));
			assert_matches!(child::get(trie, &[99]), Some(42));

			tries.push(trie.clone())
		}

		// Run single lazy removal
		Contracts::on_idle(System::block_number(), Weight::MAX);

		// The single lazy removal should have removed all queued tries
		for trie in tries.iter() {
			assert_matches!(child::get::<i32>(trie, &[99]), None);
		}
	});
}

#[test]
fn ref_time_left_api_works() {
	let (code, _) = compile_module("ref_time_left").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor calls ref_time_left twice and asserts it to decrease
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: It echoes back the ref_time returned by the ref_time_left API.
		let received = builder::bare_call(addr).build_and_unwrap_result();
		assert_eq!(received.flags, ReturnFlags::empty());

		let returned_value = u64::from_le_bytes(received.data[..8].try_into().unwrap());
		assert!(returned_value > 0);
		assert!(returned_value < GAS_LIMIT.ref_time());
	});
}

#[test]
fn lazy_removal_partial_remove_works() {
	let (code, _hash) = compile_module("self_destruct").unwrap();

	// We create a contract with some extra keys above the weight limit
	let extra_keys = 7u32;
	let mut meter = WeightMeter::with_limit(Weight::from_parts(5_000_000_000, 100 * 1024));
	let (weight_per_key, max_keys) = ContractInfo::<Test>::deletion_budget(&meter);
	let vals: Vec<_> = (0..max_keys + extra_keys)
		.map(|i| (blake2_256(&i.encode()), (i as u32), (i as u32).encode()))
		.collect();

	let mut ext = ExtBuilder::default().existential_deposit(50).build();

	let trie = ext.execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		let info = get_contract(&addr);

		// Put value into the contracts child trie
		for val in &vals {
			info.write(&Key::Fix(val.0), Some(val.2.clone()), None, false).unwrap();
		}
		AccountInfo::<Test>::insert_contract(&addr, info.clone());

		// Terminate the contract
		assert_ok!(builder::call(addr).build());

		// Contract info should be gone
		assert!(!<AccountInfoOf::<Test>>::contains_key(&addr));

		let trie = info.child_trie_info();

		// But value should be still there as the lazy removal did not run, yet.
		for val in &vals {
			assert_eq!(child::get::<u32>(&trie, &blake2_256(&val.0)), Some(val.1));
		}

		trie.clone()
	});

	// The lazy removal limit only applies to the backend but not to the overlay.
	// This commits all keys from the overlay to the backend.
	ext.commit_all().unwrap();

	ext.execute_with(|| {
		// Run the lazy removal
		ContractInfo::<Test>::process_deletion_queue_batch(&mut meter);

		// Weight should be exhausted because we could not even delete all keys
		assert!(!meter.can_consume(weight_per_key));

		let mut num_deleted = 0u32;
		let mut num_remaining = 0u32;

		for val in &vals {
			match child::get::<u32>(&trie, &blake2_256(&val.0)) {
				None => num_deleted += 1,
				Some(x) if x == val.1 => num_remaining += 1,
				Some(_) => panic!("Unexpected value in contract storage"),
			}
		}

		// All but one key is removed
		assert_eq!(num_deleted + num_remaining, vals.len() as u32);
		assert_eq!(num_deleted, max_keys);
		assert_eq!(num_remaining, extra_keys);
	});
}

#[test]
fn lazy_removal_does_no_run_on_low_remaining_weight() {
	let (code, _hash) = compile_module("self_destruct").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		let info = get_contract(&addr);
		let trie = &info.child_trie_info();

		// Put value into the contracts child trie
		child::put(trie, &[99], &42);

		// Terminate the contract
		assert_ok!(builder::call(addr).build());

		// Contract info should be gone
		assert!(!<AccountInfoOf::<Test>>::contains_key(&addr));

		// But value should be still there as the lazy removal did not run, yet.
		assert_matches!(child::get(trie, &[99]), Some(42));

		// Assign a remaining weight which is too low for a successful deletion of the contract
		let low_remaining_weight =
			<<Test as Config>::WeightInfo as WeightInfo>::on_process_deletion_queue_batch();

		// Run the lazy removal
		Contracts::on_idle(System::block_number(), low_remaining_weight);

		// Value should still be there, since remaining weight was too low for removal
		assert_matches!(child::get::<i32>(trie, &[99]), Some(42));

		// Run the lazy removal while deletion_queue is not full
		Contracts::on_initialize(System::block_number());

		// Value should still be there, since deletion_queue was not full
		assert_matches!(child::get::<i32>(trie, &[99]), Some(42));

		// Run on_idle with max remaining weight, this should remove the value
		Contracts::on_idle(System::block_number(), Weight::MAX);

		// Value should be gone
		assert_matches!(child::get::<i32>(trie, &[99]), None);
	});
}

#[test]
fn lazy_removal_does_not_use_all_weight() {
	let (code, _hash) = compile_module("self_destruct").unwrap();

	let mut meter = WeightMeter::with_limit(Weight::from_parts(5_000_000_000, 100 * 1024));
	let mut ext = ExtBuilder::default().existential_deposit(50).build();

	let (trie, vals, weight_per_key) = ext.execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		let info = get_contract(&addr);
		let (weight_per_key, max_keys) = ContractInfo::<Test>::deletion_budget(&meter);
		assert!(max_keys > 0);

		// We create a contract with one less storage item than we can remove within the limit
		let vals: Vec<_> = (0..max_keys - 1)
			.map(|i| (blake2_256(&i.encode()), (i as u32), (i as u32).encode()))
			.collect();

		// Put value into the contracts child trie
		for val in &vals {
			info.write(&Key::Fix(val.0), Some(val.2.clone()), None, false).unwrap();
		}
		AccountInfo::<Test>::insert_contract(&addr, info.clone());

		// Terminate the contract
		assert_ok!(builder::call(addr).build());

		// Contract info should be gone
		assert!(!<AccountInfoOf::<Test>>::contains_key(&addr));

		let trie = info.child_trie_info();

		// But value should be still there as the lazy removal did not run, yet.
		for val in &vals {
			assert_eq!(child::get::<u32>(&trie, &blake2_256(&val.0)), Some(val.1));
		}

		(trie, vals, weight_per_key)
	});

	// The lazy removal limit only applies to the backend but not to the overlay.
	// This commits all keys from the overlay to the backend.
	ext.commit_all().unwrap();

	ext.execute_with(|| {
		// Run the lazy removal
		ContractInfo::<Test>::process_deletion_queue_batch(&mut meter);
		let base_weight =
			<<Test as Config>::WeightInfo as WeightInfo>::on_process_deletion_queue_batch();
		assert_eq!(meter.consumed(), weight_per_key.mul(vals.len() as _) + base_weight);

		// All the keys are removed
		for val in vals {
			assert_eq!(child::get::<u32>(&trie, &blake2_256(&val.0)), None);
		}
	});
}

#[test]
fn deletion_queue_ring_buffer_overflow() {
	let (code, _hash) = compile_module("self_destruct").unwrap();
	let mut ext = ExtBuilder::default().existential_deposit(50).build();

	// setup the deletion queue with custom counters
	ext.execute_with(|| {
		let queue = DeletionQueueManager::from_test_values(u32::MAX - 1, u32::MAX - 1);
		<DeletionQueueCounter<Test>>::set(queue);
	});

	// commit the changes to the storage
	ext.commit_all().unwrap();

	ext.execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);
		let mut tries: Vec<child::ChildInfo> = vec![];

		// add 3 contracts to the deletion queue
		for i in 0..3u8 {
			let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
				.native_value(min_balance * 100)
				.salt(Some([i; 32]))
				.build_and_unwrap_contract();

			let info = get_contract(&addr);
			let trie = &info.child_trie_info();

			// Put value into the contracts child trie
			child::put(trie, &[99], &42);

			// Terminate the contract. Contract info should be gone, but value should be still
			// there as the lazy removal did not run, yet.
			assert_ok!(builder::call(addr).build());

			assert!(!<AccountInfoOf::<Test>>::contains_key(&addr));
			assert_matches!(child::get(trie, &[99]), Some(42));

			tries.push(trie.clone())
		}

		// Run single lazy removal
		Contracts::on_idle(System::block_number(), Weight::MAX);

		// The single lazy removal should have removed all queued tries
		for trie in tries.iter() {
			assert_matches!(child::get::<i32>(trie, &[99]), None);
		}

		// insert and delete counter values should go from u32::MAX - 1 to 1
		assert_eq!(<DeletionQueueCounter<Test>>::get().as_test_tuple(), (1, 1));
	})
}
#[test]
fn refcounter() {
	let (binary, code_hash) = compile_module("self_destruct").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();

		// Create two contracts with the same code and check that they do in fact share it.
		let Contract { addr: addr0, .. } = builder::bare_instantiate(Code::Upload(binary.clone()))
			.native_value(min_balance * 100)
			.salt(Some([0; 32]))
			.build_and_unwrap_contract();
		let Contract { addr: addr1, .. } = builder::bare_instantiate(Code::Upload(binary.clone()))
			.native_value(min_balance * 100)
			.salt(Some([1; 32]))
			.build_and_unwrap_contract();
		assert_refcount!(code_hash, 2);

		// Sharing should also work with the usual instantiate call
		let Contract { addr: addr2, .. } = builder::bare_instantiate(Code::Existing(code_hash))
			.native_value(min_balance * 100)
			.salt(Some([2; 32]))
			.build_and_unwrap_contract();
		assert_refcount!(code_hash, 3);

		// Terminating one contract should decrement the refcount
		assert_ok!(builder::call(addr0).build());
		assert_refcount!(code_hash, 2);

		// remove another one
		assert_ok!(builder::call(addr1).build());
		assert_refcount!(code_hash, 1);

		// Pristine code should still be there
		PristineCode::<Test>::get(code_hash).unwrap();

		// remove the last contract
		assert_ok!(builder::call(addr2).build());
		assert_refcount!(code_hash, 0);

		// refcount is `0` but code should still exists because it needs to be removed manually
		assert!(crate::PristineCode::<Test>::contains_key(&code_hash));
	});
}

#[test]
fn gas_estimation_for_subcalls() {
	let (caller_code, _caller_hash) = compile_module("call_with_limit").unwrap();
	let (dummy_code, _callee_hash) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 2_000 * min_balance);

		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(caller_code))
				.native_value(min_balance * 100)
				.build_and_unwrap_contract();

		let Contract { addr: addr_dummy, .. } = builder::bare_instantiate(Code::Upload(dummy_code))
			.native_value(min_balance * 100)
			.build_and_unwrap_contract();

		// Run the test for all of those weight limits for the subcall
		let weights = [
			Weight::MAX,
			GAS_LIMIT,
			GAS_LIMIT * 2,
			GAS_LIMIT / 5,
			Weight::from_parts(u64::MAX, GAS_LIMIT.proof_size()),
			Weight::from_parts(GAS_LIMIT.ref_time(), u64::MAX),
		];

		let (sub_addr, sub_input) = (addr_dummy.as_ref(), vec![]);

		for weight in weights {
			let input: Vec<u8> = sub_addr
				.iter()
				.cloned()
				.chain(weight.ref_time().to_le_bytes())
				.chain(weight.proof_size().to_le_bytes())
				.chain(sub_input.clone())
				.collect();

			// Call in order to determine the gas that is required for this call
			let result_orig = builder::bare_call(addr_caller).data(input.clone()).build();
			assert_ok!(&result_orig.result);
			assert_eq!(result_orig.gas_required, result_orig.gas_consumed);

			// Make the same call using the estimated gas. Should succeed.
			let result = builder::bare_call(addr_caller)
				.gas_limit(result_orig.gas_required)
				.storage_deposit_limit(result_orig.storage_deposit.charge_or_zero().into())
				.data(input.clone())
				.build();
			assert_ok!(&result.result);

			// Check that it fails with too little ref_time
			let result = builder::bare_call(addr_caller)
				.gas_limit(result_orig.gas_required.sub_ref_time(1))
				.storage_deposit_limit(result_orig.storage_deposit.charge_or_zero().into())
				.data(input.clone())
				.build();
			assert_err!(result.result, <Error<Test>>::OutOfGas);

			// Check that it fails with too little proof_size
			let result = builder::bare_call(addr_caller)
				.gas_limit(result_orig.gas_required.sub_proof_size(1))
				.storage_deposit_limit(result_orig.storage_deposit.charge_or_zero().into())
				.data(input.clone())
				.build();
			assert_err!(result.result, <Error<Test>>::OutOfGas);
		}
	});
}

#[test]
fn call_runtime_reentrancy_guarded() {
	use crate::precompiles::Precompile;
	use alloy_core::sol_types::SolInterface;
	use precompiles::{INoInfo, NoInfo};

	let precompile_addr = H160(NoInfo::<Test>::MATCHER.base_address());

	let (callee_code, _callee_hash) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let min_balance = Contracts::min_balance();
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1000 * min_balance);
		let _ = <Test as Config>::Currency::set_balance(&CHARLIE, 1000 * min_balance);

		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(callee_code))
				.native_value(min_balance * 100)
				.salt(Some([1; 32]))
				.build_and_unwrap_contract();

		// Call pallet_revive call() dispatchable
		let call = RuntimeCall::Contracts(crate::Call::call {
			dest: addr_callee,
			value: 0,
			gas_limit: GAS_LIMIT / 3,
			storage_deposit_limit: deposit_limit::<Test>(),
			data: vec![],
		})
		.encode();

		// Call runtime to re-enter back to contracts engine by
		// calling dummy contract
		let result = builder::bare_call(precompile_addr)
			.data(
				INoInfo::INoInfoCalls::callRuntime(INoInfo::callRuntimeCall { call: call.into() })
					.abi_encode(),
			)
			.build();
		// Call to runtime should fail because of the re-entrancy guard
		assert_err!(result.result, <Error<Test>>::ReenteredPallet);
	});
}

#[test]
fn sr25519_verify() {
	let (binary, _code_hash) = compile_module("sr25519_verify").unwrap();

	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the sr25519_verify contract.
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(100_000)
			.build_and_unwrap_contract();

		let call_with = |message: &[u8; 11]| {
			// Alice's signature for "hello world"
			#[rustfmt::skip]
			let signature: [u8; 64] = [
				184, 49, 74, 238, 78, 165, 102, 252, 22, 92, 156, 176, 124, 118, 168, 116, 247,
				99, 0, 94, 2, 45, 9, 170, 73, 222, 182, 74, 60, 32, 75, 64, 98, 174, 69, 55, 83,
				85, 180, 98, 208, 75, 231, 57, 205, 62, 4, 105, 26, 136, 172, 17, 123, 99, 90, 255,
				228, 54, 115, 63, 30, 207, 205, 131,
			];

			// Alice's public key
			#[rustfmt::skip]
			let public_key: [u8; 32] = [
				212, 53, 147, 199, 21, 253, 211, 28, 97, 20, 26, 189, 4, 169, 159, 214, 130, 44,
				133, 88, 133, 76, 205, 227, 154, 86, 132, 231, 165, 109, 162, 125,
			];

			let mut params = vec![];
			params.extend_from_slice(&signature);
			params.extend_from_slice(&public_key);
			params.extend_from_slice(message);

			builder::bare_call(addr).data(params).build_and_unwrap_result()
		};

		// verification should succeed for "hello world"
		assert_return_code!(call_with(&b"hello world"), RuntimeReturnCode::Success);

		// verification should fail for other messages
		assert_return_code!(call_with(&b"hello worlD"), RuntimeReturnCode::Sr25519VerifyFailed);
	});
}

#[test]
fn upload_code_works() {
	let (binary, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Drop previous events
		initialize_block(2);

		assert!(!PristineCode::<Test>::contains_key(&code_hash));
		assert_ok!(Contracts::upload_code(RuntimeOrigin::signed(ALICE), binary, 1_000,));
		// Ensure the contract was stored and get expected deposit amount to be reserved.
		expected_deposit(ensure_stored(code_hash));
	});
}

#[test]
fn upload_code_limit_too_low() {
	let (binary, _code_hash) = compile_module("dummy").unwrap();
	let deposit_expected = expected_deposit(binary.len());
	let deposit_insufficient = deposit_expected.saturating_sub(1);

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Drop previous events
		initialize_block(2);

		assert_noop!(
			Contracts::upload_code(RuntimeOrigin::signed(ALICE), binary, deposit_insufficient,),
			<Error<Test>>::StorageDepositLimitExhausted,
		);

		assert_eq!(System::events(), vec![]);
	});
}

#[test]
fn upload_code_not_enough_balance() {
	let (binary, _code_hash) = compile_module("dummy").unwrap();
	let deposit_expected = expected_deposit(binary.len());
	let deposit_insufficient = deposit_expected.saturating_sub(1);

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, deposit_insufficient);

		// Drop previous events
		initialize_block(2);

		assert_noop!(
			Contracts::upload_code(RuntimeOrigin::signed(ALICE), binary, 1_000,),
			<Error<Test>>::StorageDepositNotEnoughFunds,
		);

		assert_eq!(System::events(), vec![]);
	});
}

#[test]
fn remove_code_works() {
	let (binary, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Drop previous events
		initialize_block(2);

		assert_ok!(Contracts::upload_code(RuntimeOrigin::signed(ALICE), binary, 1_000,));
		// Ensure the contract was stored and get expected deposit amount to be reserved.
		expected_deposit(ensure_stored(code_hash));
		assert_ok!(Contracts::remove_code(RuntimeOrigin::signed(ALICE), code_hash));
	});
}

#[test]
fn remove_code_wrong_origin() {
	let (binary, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Drop previous events
		initialize_block(2);

		assert_ok!(Contracts::upload_code(RuntimeOrigin::signed(ALICE), binary, 1_000,));
		// Ensure the contract was stored and get expected deposit amount to be reserved.
		expected_deposit(ensure_stored(code_hash));

		assert_noop!(
			Contracts::remove_code(RuntimeOrigin::signed(BOB), code_hash),
			sp_runtime::traits::BadOrigin,
		);
	});
}

#[test]
fn remove_code_in_use() {
	let (binary, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		assert_ok!(builder::instantiate_with_code(binary).build());

		// Drop previous events
		initialize_block(2);

		assert_noop!(
			Contracts::remove_code(RuntimeOrigin::signed(ALICE), code_hash),
			<Error<Test>>::CodeInUse,
		);

		assert_eq!(System::events(), vec![]);
	});
}

#[test]
fn remove_code_not_found() {
	let (_binary, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Drop previous events
		initialize_block(2);

		assert_noop!(
			Contracts::remove_code(RuntimeOrigin::signed(ALICE), code_hash),
			<Error<Test>>::CodeNotFound,
		);

		assert_eq!(System::events(), vec![]);
	});
}

#[test]
fn instantiate_with_zero_balance_works() {
	let (binary, code_hash) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();

		// Drop previous events
		initialize_block(2);

		// Instantiate the BOB contract.
		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		// Ensure the contract was stored and get expected deposit amount to be reserved.
		expected_deposit(ensure_stored(code_hash));

		// Make sure the account exists even though no free balance was send
		assert_eq!(<Test as Config>::Currency::free_balance(&account_id), min_balance);
		assert_eq!(
			<Test as Config>::Currency::total_balance(&account_id),
			min_balance + test_utils::contract_base_deposit(&addr)
		);

		assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Held {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::CodeUploadDepositReserve,
						),
						who: ALICE,
						amount: 776,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::System(frame_system::Event::NewAccount {
						account: account_id.clone(),
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Endowed {
						account: account_id.clone(),
						free_balance: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: ALICE,
						to: account_id.clone(),
						amount: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Contracts(crate::Event::Instantiated {
						deployer: ALICE_ADDR,
						contract: addr,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferAndHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: ALICE,
						dest: account_id,
						transferred: 336,
					}),
					topics: vec![],
				},
			]
		);
	});
}

#[test]
fn instantiate_with_below_existential_deposit_works() {
	let (binary, code_hash) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();
		let value = 50;

		// Drop previous events
		initialize_block(2);

		// Instantiate the BOB contract.
		let Contract { addr, account_id } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(value)
			.build_and_unwrap_contract();

		// Ensure the contract was stored and get expected deposit amount to be reserved.
		expected_deposit(ensure_stored(code_hash));
		// Make sure the account exists even though not enough free balance was send
		assert_eq!(<Test as Config>::Currency::free_balance(&account_id), min_balance + value);
		assert_eq!(
			<Test as Config>::Currency::total_balance(&account_id),
			min_balance + value + test_utils::contract_base_deposit(&addr)
		);

		assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Held {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::CodeUploadDepositReserve,
						),
						who: ALICE,
						amount: 776,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::System(frame_system::Event::NewAccount {
						account: account_id.clone()
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Endowed {
						account: account_id.clone(),
						free_balance: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: ALICE,
						to: account_id.clone(),
						amount: min_balance,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: ALICE,
						to: account_id.clone(),
						amount: 50,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Contracts(crate::Event::Instantiated {
						deployer: ALICE_ADDR,
						contract: addr,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferAndHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: ALICE,
						dest: account_id.clone(),
						transferred: 336,
					}),
					topics: vec![],
				},
			]
		);
	});
}

#[test]
fn storage_deposit_works() {
	let (binary, _code_hash) = compile_module("multi_store").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		let mut deposit = test_utils::contract_base_deposit(&addr);

		// Drop previous events
		initialize_block(2);

		// Create storage
		assert_ok!(builder::call(addr).value(42).data((50u32, 20u32).encode()).build());
		// 4 is for creating 2 storage items
		// 48 is for each of the keys
		let charged0 = 4 + 50 + 20 + 48 + 48;
		deposit += charged0;
		assert_eq!(get_contract(&addr).total_deposit(), deposit);

		// Add more storage (but also remove some)
		assert_ok!(builder::call(addr).data((100u32, 10u32).encode()).build());
		let charged1 = 50 - 10;
		deposit += charged1;
		assert_eq!(get_contract(&addr).total_deposit(), deposit);

		// Remove more storage (but also add some)
		assert_ok!(builder::call(addr).data((10u32, 20u32).encode()).build());
		// -1 for numeric instability
		let refunded0 = 90 - 10 - 1;
		deposit -= refunded0;
		assert_eq!(get_contract(&addr).total_deposit(), deposit);

		assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::Transfer {
						from: ALICE,
						to: account_id.clone(),
						amount: 42,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferAndHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: ALICE,
						dest: account_id.clone(),
						transferred: charged0,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferAndHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: ALICE,
						dest: account_id.clone(),
						transferred: charged1,
					}),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::Balances(pallet_balances::Event::TransferOnHold {
						reason: <Test as Config>::RuntimeHoldReason::Contracts(
							HoldReason::StorageDepositReserve,
						),
						source: account_id.clone(),
						dest: ALICE,
						amount: refunded0,
					}),
					topics: vec![],
				},
			]
		);
	});
}

#[test]
fn storage_deposit_callee_works() {
	let (binary_caller, _code_hash_caller) = compile_module("call").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create both contracts: Constructors do nothing.
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller)).build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		assert_ok!(builder::call(addr_caller).data((100u32, &addr_callee).encode()).build());

		let callee = get_contract(&addr_callee);
		let deposit = DepositPerByte::get() * 100 + DepositPerItem::get() * 1 + 48;

		assert_eq!(Pallet::<Test>::evm_balance(&addr_caller), U256::zero());
		assert_eq!(
			callee.total_deposit(),
			deposit + test_utils::contract_base_deposit(&addr_callee)
		);
	});
}

#[test]
fn set_code_extrinsic() {
	let (binary, code_hash) = compile_module("dummy").unwrap();
	let (new_binary, new_code_hash) = compile_module("crypto_hashes").unwrap();

	assert_ne!(code_hash, new_code_hash);

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		assert_ok!(Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			new_binary,
			deposit_limit::<Test>(),
		));

		// Drop previous events
		initialize_block(2);

		assert_eq!(get_contract(&addr).code_hash, code_hash);
		assert_refcount!(&code_hash, 1);
		assert_refcount!(&new_code_hash, 0);

		// only root can execute this extrinsic
		assert_noop!(
			Contracts::set_code(RuntimeOrigin::signed(ALICE), addr, new_code_hash),
			sp_runtime::traits::BadOrigin,
		);
		assert_eq!(get_contract(&addr).code_hash, code_hash);
		assert_refcount!(&code_hash, 1);
		assert_refcount!(&new_code_hash, 0);
		assert_eq!(System::events(), vec![]);

		// contract must exist
		assert_noop!(
			Contracts::set_code(RuntimeOrigin::root(), BOB_ADDR, new_code_hash),
			<Error<Test>>::ContractNotFound,
		);
		assert_eq!(get_contract(&addr).code_hash, code_hash);
		assert_refcount!(&code_hash, 1);
		assert_refcount!(&new_code_hash, 0);
		assert_eq!(System::events(), vec![]);

		// new code hash must exist
		assert_noop!(
			Contracts::set_code(RuntimeOrigin::root(), addr, Default::default()),
			<Error<Test>>::CodeNotFound,
		);
		assert_eq!(get_contract(&addr).code_hash, code_hash);
		assert_refcount!(&code_hash, 1);
		assert_refcount!(&new_code_hash, 0);
		assert_eq!(System::events(), vec![]);

		// successful call
		assert_ok!(Contracts::set_code(RuntimeOrigin::root(), addr, new_code_hash));
		assert_eq!(get_contract(&addr).code_hash, new_code_hash);
		assert_refcount!(&code_hash, 0);
		assert_refcount!(&new_code_hash, 1);
	});
}

#[test]
fn slash_cannot_kill_account() {
	let (binary, _code_hash) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let value = 700;
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();

		let Contract { addr, account_id } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(value)
			.build_and_unwrap_contract();

		// Drop previous events
		initialize_block(2);

		let info_deposit = test_utils::contract_base_deposit(&addr);

		assert_eq!(
			test_utils::get_balance_on_hold(&HoldReason::StorageDepositReserve.into(), &account_id),
			info_deposit
		);

		assert_eq!(
			<Test as Config>::Currency::total_balance(&account_id),
			info_deposit + value + min_balance
		);

		// Try to destroy the account of the contract by slashing the total balance.
		// The account does not get destroyed because slashing only affects the balance held
		// under certain `reason`. Slashing can for example happen if the contract takes part
		// in staking.
		let _ = <Test as Config>::Currency::slash(
			&HoldReason::StorageDepositReserve.into(),
			&account_id,
			<Test as Config>::Currency::total_balance(&account_id),
		);

		// Slashing only removed the balance held.
		assert_eq!(<Test as Config>::Currency::total_balance(&account_id), value + min_balance);
	});
}

#[test]
fn contract_reverted() {
	let (binary, code_hash) = compile_module("return_with_data").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let flags = ReturnFlags::REVERT;
		let buffer = [4u8, 8, 15, 16, 23, 42];
		let input = (flags.bits(), buffer).encode();

		// We just upload the code for later use
		assert_ok!(Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			binary.clone(),
			deposit_limit::<Test>(),
		));

		// Calling extrinsic: revert leads to an error
		assert_err_ignore_postinfo!(
			builder::instantiate(code_hash).data(input.clone()).build(),
			<Error<Test>>::ContractReverted,
		);

		// Calling extrinsic: revert leads to an error
		assert_err_ignore_postinfo!(
			builder::instantiate_with_code(binary).data(input.clone()).build(),
			<Error<Test>>::ContractReverted,
		);

		// Calling directly: revert leads to success but the flags indicate the error
		// This is just a different way of transporting the error that allows the read out
		// the `data` which is only there on success. Obviously, the contract isn't
		// instantiated.
		let result = builder::bare_instantiate(Code::Existing(code_hash))
			.data(input.clone())
			.build_and_unwrap_result();
		assert_eq!(result.result.flags, flags);
		assert_eq!(result.result.data, buffer);
		assert!(!<AccountInfoOf<Test>>::contains_key(result.addr));

		// Pass empty flags and therefore successfully instantiate the contract for later use.
		let Contract { addr, .. } = builder::bare_instantiate(Code::Existing(code_hash))
			.data(ReturnFlags::empty().bits().encode())
			.build_and_unwrap_contract();

		// Calling extrinsic: revert leads to an error
		assert_err_ignore_postinfo!(
			builder::call(addr).data(input.clone()).build(),
			<Error<Test>>::ContractReverted,
		);

		// Calling directly: revert leads to success but the flags indicate the error
		let result = builder::bare_call(addr).data(input).build_and_unwrap_result();
		assert_eq!(result.flags, flags);
		assert_eq!(result.data, buffer);
	});
}

#[test]
fn set_code_hash() {
	let (binary, _) = compile_module("set_code_hash").unwrap();
	let (new_binary, new_code_hash) = compile_module("new_set_code_hash_contract").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the 'caller'
		let Contract { addr: contract_addr, .. } = builder::bare_instantiate(Code::Upload(binary))
			.native_value(300_000)
			.build_and_unwrap_contract();
		// upload new code
		assert_ok!(Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			new_binary.clone(),
			deposit_limit::<Test>(),
		));

		System::reset_events();

		// First call sets new code_hash and returns 1
		let result = builder::bare_call(contract_addr)
			.data(new_code_hash.as_ref().to_vec())
			.build_and_unwrap_result();
		assert_return_code!(result, 1);

		// Second calls new contract code that returns 2
		let result = builder::bare_call(contract_addr).build_and_unwrap_result();
		assert_return_code!(result, 2);
	});
}

#[test]
fn storage_deposit_limit_is_enforced() {
	let (binary, _code_hash) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let min_balance = Contracts::min_balance();

		// Setting insufficient storage_deposit should fail.
		assert_err!(
			builder::bare_instantiate(Code::Upload(binary.clone()))
				// expected deposit is 2 * ed + 3 for the call
				.storage_deposit_limit((2 * min_balance + 3 - 1).into())
				.build()
				.result,
			<Error<Test>>::StorageDepositLimitExhausted,
		);

		// Instantiate the BOB contract.
		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		let info_deposit = test_utils::contract_base_deposit(&addr);
		// Check that the BOB contract has been instantiated and has the minimum balance
		assert_eq!(get_contract(&addr).total_deposit(), info_deposit);
		assert_eq!(
			<Test as Config>::Currency::total_balance(&account_id),
			info_deposit + min_balance
		);

		// Create 1 byte of storage with a price of per byte,
		// setting insufficient deposit limit, as it requires 3 Balance:
		// 2 for the item added + 1 (value) + 48 (key)
		assert_err_ignore_postinfo!(
			builder::call(addr)
				.storage_deposit_limit(50)
				.data(1u32.to_le_bytes().to_vec())
				.build(),
			<Error<Test>>::StorageDepositLimitExhausted,
		);

		// now with enough limit
		assert_ok!(builder::call(addr)
			.storage_deposit_limit(51)
			.data(1u32.to_le_bytes().to_vec())
			.build());

		// Use 4 more bytes of the storage for the same item, which requires 4 Balance.
		// Should fail as DefaultDepositLimit is 3 and hence isn't enough.
		assert_err_ignore_postinfo!(
			builder::call(addr)
				.storage_deposit_limit(3)
				.data(5u32.to_le_bytes().to_vec())
				.build(),
			<Error<Test>>::StorageDepositLimitExhausted,
		);
	});
}

#[test]
fn deposit_limit_in_nested_calls() {
	let (binary_caller, _code_hash_caller) = compile_module("create_storage_and_call").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create both contracts: Constructors do nothing.
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller)).build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		// Create 100 bytes of storage with a price of per byte
		// This is 100 Balance + 2 Balance for the item
		// 48 for the key
		assert_ok!(builder::call(addr_callee)
			.storage_deposit_limit(102 + 48)
			.data(100u32.to_le_bytes().to_vec())
			.build());

		// We do not remove any storage but add a storage item of 12 bytes in the caller
		// contract. This would cost 12 + 2 + 72 = 86 Balance.
		// The nested call doesn't get a special limit, which is set by passing `u64::MAX` to it.
		// This should fail as the specified parent's limit is less than the cost: 13 <
		// 14.
		assert_err_ignore_postinfo!(
			builder::call(addr_caller)
				.storage_deposit_limit(85)
				.data((100u32, &addr_callee, U256::MAX).encode())
				.build(),
			<Error<Test>>::StorageDepositLimitExhausted,
		);

		// Now we specify the parent's limit high enough to cover the caller's storage
		// additions. However, we use a single byte more in the callee, hence the storage
		// deposit should be 87 Balance.
		// The nested call doesn't get a special limit, which is set by passing `u64::MAX` to it.
		// This should fail as the specified parent's limit is less than the cost: 86 < 87
		assert_err_ignore_postinfo!(
			builder::call(addr_caller)
				.storage_deposit_limit(86)
				.data((101u32, &addr_callee, &U256::MAX).encode())
				.build(),
			<Error<Test>>::StorageDepositLimitExhausted,
		);

		// The parents storage deposit limit doesn't matter as the sub calls limit
		// is enforced eagerly. However, we set a special deposit limit of 1 Balance for the
		// nested call. This should fail as callee adds up 2 bytes to the storage, meaning
		// that the nested call should have a deposit limit of at least 2 Balance. The
		// sub-call should be rolled back, which is covered by the next test case.
		let ret = builder::bare_call(addr_caller)
			.storage_deposit_limit(DepositLimit::Balance(u64::MAX))
			.data((102u32, &addr_callee, U256::from(1u64)).encode())
			.build_and_unwrap_result();
		assert_return_code!(ret, RuntimeReturnCode::OutOfResources);

		// Refund in the callee contract but not enough to cover the Balance required by the
		// caller. Note that if previous sub-call wouldn't roll back, this call would pass
		// making the test case fail. We don't set a special limit for the nested call here.
		assert_err_ignore_postinfo!(
			builder::call(addr_caller)
				.storage_deposit_limit(0)
				.data((87u32, &addr_callee, &U256::MAX.to_little_endian()).encode())
				.build(),
			<Error<Test>>::StorageDepositLimitExhausted,
		);

		let _ = <Test as Config>::Currency::set_balance(&ALICE, 511);

		// Require more than the sender's balance.
		// Limit the sub call to little balance so it should fail in there
		let ret = builder::bare_call(addr_caller)
			.data((416, &addr_callee, U256::from(1u64)).encode())
			.build_and_unwrap_result();
		assert_return_code!(ret, RuntimeReturnCode::OutOfResources);

		// Free up enough storage in the callee so that the caller can create a new item
		// We set the special deposit limit of 1 Balance for the nested call, which isn't
		// enforced as callee frees up storage. This should pass.
		assert_ok!(builder::call(addr_caller)
			.storage_deposit_limit(1)
			.data((0u32, &addr_callee, U256::from(1u64)).encode())
			.build());
	});
}

#[test]
fn deposit_limit_in_nested_instantiate() {
	let (binary_caller, _code_hash_caller) =
		compile_module("create_storage_and_instantiate").unwrap();
	let (binary_callee, code_hash_callee) = compile_module("store_deploy").unwrap();
	const ED: u64 = 5;
	ExtBuilder::default().existential_deposit(ED).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let _ = <Test as Config>::Currency::set_balance(&BOB, 1_000_000);
		// Create caller contract
		let Contract { addr: addr_caller, account_id: caller_id } =
			builder::bare_instantiate(Code::Upload(binary_caller))
				.native_value(10_000) // this balance is later passed to the deployed contract
				.build_and_unwrap_contract();
		// Deploy a contract to get its occupied storage size
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(binary_callee))
			.data(vec![0, 0, 0, 0])
			.build_and_unwrap_contract();

		// This is the deposit we expect to be charged just for instantiatiting the callee.
		//
		// - callee_info_len + 2 for storing the new contract info
		// - the deposit for depending on a code hash
		// - ED for deployed contract account
		// - 2 for the storage item of 0 bytes being created in the callee constructor
		// - 48 for the key
		let callee_min_deposit = {
			let callee_info_len =
				AccountInfo::<Test>::load_contract(&addr).unwrap().encoded_size() as u64;
			let code_deposit = test_utils::lockup_deposit(&code_hash_callee);
			callee_info_len + code_deposit + 2 + ED + 2 + 48
		};

		// The parent just stores an item of the passed size so at least
		// we need to pay for the item itself.
		let caller_min_deposit = callee_min_deposit + 2 + 48;

		// Fail in callee.
		//
		// We still fail in the sub call because we enforce limits on return from a contract.
		// Sub calls return first to they are checked first.
		let ret = builder::bare_call(addr_caller)
			.origin(RuntimeOrigin::signed(BOB))
			.storage_deposit_limit(DepositLimit::Balance(0))
			.data((&code_hash_callee, 100u32, &U256::MAX.to_little_endian()).encode())
			.build_and_unwrap_result();
		assert_return_code!(ret, RuntimeReturnCode::OutOfResources);
		// The charges made on instantiation should be rolled back.
		assert_eq!(<Test as Config>::Currency::free_balance(&BOB), 1_000_000);

		// Fail in the caller.
		//
		// For that we need to supply enough storage deposit so that the sub call
		// succeeds but the parent call runs out of storage.
		let ret = builder::bare_call(addr_caller)
			.origin(RuntimeOrigin::signed(BOB))
			.storage_deposit_limit(DepositLimit::Balance(callee_min_deposit))
			.data((&code_hash_callee, 0u32, &U256::MAX.to_little_endian()).encode())
			.build();
		assert_err!(ret.result, <Error<Test>>::StorageDepositLimitExhausted);
		// The charges made on the instantiation should be rolled back.
		assert_eq!(<Test as Config>::Currency::free_balance(&BOB), 1_000_000);

		// Fail in the callee with bytes.
		//
		// Same as above but stores one byte in both caller and callee.
		let ret = builder::bare_call(addr_caller)
			.origin(RuntimeOrigin::signed(BOB))
			.storage_deposit_limit(DepositLimit::Balance(caller_min_deposit + 1))
			.data((&code_hash_callee, 1u32, U256::from(callee_min_deposit)).encode())
			.build_and_unwrap_result();
		assert_return_code!(ret, RuntimeReturnCode::OutOfResources);
		// The charges made on the instantiation should be rolled back.
		assert_eq!(<Test as Config>::Currency::free_balance(&BOB), 1_000_000);

		// Fail in the caller with bytes.
		//
		// Same as above but stores one byte in both caller and callee.
		let ret = builder::bare_call(addr_caller)
			.origin(RuntimeOrigin::signed(BOB))
			.storage_deposit_limit(DepositLimit::Balance(callee_min_deposit + 1))
			.data((&code_hash_callee, 1u32, U256::from(callee_min_deposit + 1)).encode())
			.build();
		assert_err!(ret.result, <Error<Test>>::StorageDepositLimitExhausted);
		// The charges made on the instantiation should be rolled back.
		assert_eq!(<Test as Config>::Currency::free_balance(&BOB), 1_000_000);

		// Set enough deposit limit for the child instantiate. This should succeed.
		let result = builder::bare_call(addr_caller)
			.origin(RuntimeOrigin::signed(BOB))
			.storage_deposit_limit((caller_min_deposit + 2).into())
			.data((&code_hash_callee, 1u32, U256::from(callee_min_deposit + 1)).encode())
			.build();

		let returned = result.result.unwrap();
		assert!(!returned.did_revert());

		// All balance of the caller except ED has been transferred to the callee.
		// No deposit has been taken from it.
		assert_eq!(<Test as Config>::Currency::free_balance(&caller_id), ED);
		// Get address of the deployed contract.
		let addr_callee = H160::from_slice(&returned.data[0..20]);
		let callee_account_id = <Test as Config>::AddressMapper::to_account_id(&addr_callee);
		// 10_000 should be sent to callee from the caller contract, plus ED to be sent from the
		// origin.
		assert_eq!(<Test as Config>::Currency::free_balance(&callee_account_id), 10_000 + ED);
		// The origin should be charged with what the outer call consumed
		assert_eq!(
			<Test as Config>::Currency::free_balance(&BOB),
			1_000_000 - (caller_min_deposit + 2),
		);
		assert_eq!(result.storage_deposit.charge_or_zero(), (caller_min_deposit + 2))
	});
}

#[test]
fn deposit_limit_honors_liquidity_restrictions() {
	let (binary, _code_hash) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let bobs_balance = 1_000;
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let _ = <Test as Config>::Currency::set_balance(&BOB, bobs_balance);
		let min_balance = Contracts::min_balance();

		// Instantiate the BOB contract.
		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		let info_deposit = test_utils::contract_base_deposit(&addr);
		// Check that the contract has been instantiated and has the minimum balance
		assert_eq!(get_contract(&addr).total_deposit(), info_deposit);
		assert_eq!(
			<Test as Config>::Currency::total_balance(&account_id),
			info_deposit + min_balance
		);

		// check that the hold is honored
		<Test as Config>::Currency::hold(
			&HoldReason::CodeUploadDepositReserve.into(),
			&BOB,
			bobs_balance - min_balance,
		)
		.unwrap();
		assert_err_ignore_postinfo!(
			builder::call(addr)
				.origin(RuntimeOrigin::signed(BOB))
				.storage_deposit_limit(10_000)
				.data(100u32.to_le_bytes().to_vec())
				.build(),
			<Error<Test>>::StorageDepositNotEnoughFunds,
		);
		assert_eq!(<Test as Config>::Currency::free_balance(&BOB), min_balance);
	});
}

#[test]
fn deposit_limit_honors_existential_deposit() {
	let (binary, _code_hash) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let _ = <Test as Config>::Currency::set_balance(&BOB, 300);
		let min_balance = Contracts::min_balance();

		// Instantiate the BOB contract.
		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		let info_deposit = test_utils::contract_base_deposit(&addr);

		// Check that the contract has been instantiated and has the minimum balance
		assert_eq!(get_contract(&addr).total_deposit(), info_deposit);
		assert_eq!(
			<Test as Config>::Currency::total_balance(&account_id),
			min_balance + info_deposit
		);

		// check that the deposit can't bring the account below the existential deposit
		assert_err_ignore_postinfo!(
			builder::call(addr)
				.origin(RuntimeOrigin::signed(BOB))
				.storage_deposit_limit(10_000)
				.data(100u32.to_le_bytes().to_vec())
				.build(),
			<Error<Test>>::StorageDepositNotEnoughFunds,
		);
		assert_eq!(<Test as Config>::Currency::free_balance(&BOB), 300);
	});
}

#[test]
fn native_dependency_deposit_works() {
	let (binary, code_hash) = compile_module("set_code_hash").unwrap();
	let (dummy_binary, dummy_code_hash) = compile_module("dummy").unwrap();

	// Test with both existing and uploaded code
	for code in [Code::Upload(binary.clone()), Code::Existing(code_hash)] {
		ExtBuilder::default().build().execute_with(|| {
			let _ = Balances::set_balance(&ALICE, 1_000_000);
			let lockup_deposit_percent = CodeHashLockupDepositPercent::get();

			// Upload the dummy contract,
			Contracts::upload_code(
				RuntimeOrigin::signed(ALICE),
				dummy_binary.clone(),
				deposit_limit::<Test>(),
			)
			.unwrap();

			// Upload `set_code_hash` contracts if using Code::Existing.
			let add_upload_deposit = match code {
				Code::Existing(_) => {
					Contracts::upload_code(
						RuntimeOrigin::signed(ALICE),
						binary.clone(),
						deposit_limit::<Test>(),
					)
					.unwrap();
					false
				},
				Code::Upload(_) => true,
			};

			// Instantiate the set_code_hash contract.
			let res = builder::bare_instantiate(code).build();

			let addr = res.result.unwrap().addr;
			let account_id = <Test as Config>::AddressMapper::to_account_id(&addr);
			let base_deposit = test_utils::contract_base_deposit(&addr);
			let upload_deposit = test_utils::get_code_deposit(&code_hash);
			let extra_deposit = add_upload_deposit.then(|| upload_deposit).unwrap_or_default();

			assert_eq!(
				res.storage_deposit.charge_or_zero(),
				extra_deposit + base_deposit + Contracts::min_balance()
			);

			// call set_code_hash
			builder::bare_call(addr)
				.data(dummy_code_hash.encode())
				.build_and_unwrap_result();

			// Check updated storage_deposit due to code size changes
			let deposit_diff = lockup_deposit_percent
				.mul_ceil(test_utils::get_code_deposit(&code_hash)) -
				lockup_deposit_percent.mul_ceil(test_utils::get_code_deposit(&dummy_code_hash));
			let new_base_deposit = test_utils::contract_base_deposit(&addr);
			assert_ne!(deposit_diff, 0);
			assert_eq!(base_deposit - new_base_deposit, deposit_diff);

			assert_eq!(
				test_utils::get_balance_on_hold(
					&HoldReason::StorageDepositReserve.into(),
					&account_id
				),
				new_base_deposit
			);
		});
	}
}

#[test]
fn block_hash_works() {
	let (code, _) = compile_module("block_hash").unwrap();

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// The genesis config sets to the block number to 1
		let block_hash = [1; 32];
		frame_system::BlockHash::<Test>::insert(
			&crate::BlockNumberFor::<Test>::from(0u32),
			<Test as frame_system::Config>::Hash::from(&block_hash),
		);
		assert_ok!(builder::call(addr)
			.data((U256::zero(), H256::from(block_hash)).encode())
			.build());

		// A block number out of range returns the zero value
		assert_ok!(builder::call(addr).data((U256::from(1), H256::zero()).encode()).build());
	});
}

#[test]
fn block_author_works() {
	let (code, _) = compile_module("block_author").unwrap();

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// The fixture asserts the input to match the find_author API method output.
		assert_ok!(builder::call(addr).data(EVE_ADDR.encode()).build());
	});
}

#[test]
fn root_cannot_upload_code() {
	let (binary, _) = compile_module("dummy").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		assert_noop!(
			Contracts::upload_code(RuntimeOrigin::root(), binary, deposit_limit::<Test>()),
			DispatchError::BadOrigin,
		);
	});
}

#[test]
fn root_cannot_remove_code() {
	let (_, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		assert_noop!(
			Contracts::remove_code(RuntimeOrigin::root(), code_hash),
			DispatchError::BadOrigin,
		);
	});
}

#[test]
fn signed_cannot_set_code() {
	let (_, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		assert_noop!(
			Contracts::set_code(RuntimeOrigin::signed(ALICE), BOB_ADDR, code_hash),
			DispatchError::BadOrigin,
		);
	});
}

#[test]
fn none_cannot_call_code() {
	ExtBuilder::default().build().execute_with(|| {
		assert_err_ignore_postinfo!(
			builder::call(BOB_ADDR).origin(RuntimeOrigin::none()).build(),
			DispatchError::BadOrigin,
		);
	});
}

#[test]
fn root_can_call() {
	let (binary, _) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		// Call the contract.
		assert_ok!(builder::call(addr).origin(RuntimeOrigin::root()).build());
	});
}

#[test]
fn root_cannot_instantiate_with_code() {
	let (binary, _) = compile_module("dummy").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		assert_err_ignore_postinfo!(
			builder::instantiate_with_code(binary).origin(RuntimeOrigin::root()).build(),
			DispatchError::BadOrigin
		);
	});
}

#[test]
fn root_cannot_instantiate() {
	let (_, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		assert_err_ignore_postinfo!(
			builder::instantiate(code_hash).origin(RuntimeOrigin::root()).build(),
			DispatchError::BadOrigin
		);
	});
}

#[test]
fn only_upload_origin_can_upload() {
	let (binary, _) = compile_module("dummy").unwrap();
	UploadAccount::set(Some(ALICE));
	ExtBuilder::default().build().execute_with(|| {
		let _ = Balances::set_balance(&ALICE, 1_000_000);
		let _ = Balances::set_balance(&BOB, 1_000_000);

		assert_err!(
			Contracts::upload_code(RuntimeOrigin::root(), binary.clone(), deposit_limit::<Test>(),),
			DispatchError::BadOrigin
		);

		assert_err!(
			Contracts::upload_code(
				RuntimeOrigin::signed(BOB),
				binary.clone(),
				deposit_limit::<Test>(),
			),
			DispatchError::BadOrigin
		);

		// Only alice is allowed to upload contract code.
		assert_ok!(Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			binary.clone(),
			deposit_limit::<Test>(),
		));
	});
}

#[test]
fn only_instantiation_origin_can_instantiate() {
	let (code, code_hash) = compile_module("dummy").unwrap();
	InstantiateAccount::set(Some(ALICE));
	ExtBuilder::default().build().execute_with(|| {
		let _ = Balances::set_balance(&ALICE, 1_000_000);
		let _ = Balances::set_balance(&BOB, 1_000_000);

		assert_err_ignore_postinfo!(
			builder::instantiate_with_code(code.clone())
				.origin(RuntimeOrigin::root())
				.build(),
			DispatchError::BadOrigin
		);

		assert_err_ignore_postinfo!(
			builder::instantiate_with_code(code.clone())
				.origin(RuntimeOrigin::signed(BOB))
				.build(),
			DispatchError::BadOrigin
		);

		// Only Alice can instantiate
		assert_ok!(builder::instantiate_with_code(code).build());

		// Bob cannot instantiate with either `instantiate_with_code` or `instantiate`.
		assert_err_ignore_postinfo!(
			builder::instantiate(code_hash).origin(RuntimeOrigin::signed(BOB)).build(),
			DispatchError::BadOrigin
		);
	});
}

#[test]
fn balance_of_api() {
	let (binary, _code_hash) = compile_module("balance_of").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = Balances::set_balance(&ALICE, 1_000_000);
		let _ = Balances::set_balance(&ALICE_FALLBACK, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(binary.to_vec())).build_and_unwrap_contract();

		// The fixture asserts a non-zero returned free balance of the account;
		// The ALICE_FALLBACK account is endowed;
		// Hence we should not revert
		assert_ok!(builder::call(addr).data(ALICE_ADDR.0.to_vec()).build());

		// The fixture asserts a non-zero returned free balance of the account;
		// The ETH_BOB account is not endowed;
		// Hence we should revert
		assert_err_ignore_postinfo!(
			builder::call(addr).data(BOB_ADDR.0.to_vec()).build(),
			<Error<Test>>::ContractTrapped
		);
	});
}

#[test]
fn balance_api_returns_free_balance() {
	let (binary, _code_hash) = compile_module("balance").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Instantiate the BOB contract without any extra balance.
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(binary.to_vec())).build_and_unwrap_contract();

		let value = 0;
		// Call BOB which makes it call the balance runtime API.
		// The contract code asserts that the returned balance is 0.
		assert_ok!(builder::call(addr).value(value).build());

		let value = 1;
		// Calling with value will trap the contract.
		assert_err_ignore_postinfo!(
			builder::call(addr).value(value).build(),
			<Error<Test>>::ContractTrapped
		);
	});
}

#[test]
fn gas_consumed_is_linear_for_nested_calls() {
	let (code, _code_hash) = compile_module("recurse").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let [gas_0, gas_1, gas_2, gas_max] = {
			[0u32, 1u32, 2u32, limits::CALL_STACK_DEPTH]
				.iter()
				.map(|i| {
					let result = builder::bare_call(addr).data(i.encode()).build();
					assert_ok!(result.result);
					result.gas_consumed
				})
				.collect::<Vec<_>>()
				.try_into()
				.unwrap()
		};

		let gas_per_recursion = gas_2.checked_sub(&gas_1).unwrap();
		assert_eq!(gas_max, gas_0 + gas_per_recursion * limits::CALL_STACK_DEPTH as u64);
	});
}

#[test]
fn read_only_call_cannot_store() {
	let (binary_caller, _code_hash_caller) = compile_module("read_only_call").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create both contracts: Constructors do nothing.
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller)).build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		// Read-only call fails when modifying storage.
		assert_err_ignore_postinfo!(
			builder::call(addr_caller).data((&addr_callee, 100u32).encode()).build(),
			<Error<Test>>::ContractTrapped
		);
	});
}

#[test]
fn read_only_call_cannot_transfer() {
	let (binary_caller, _code_hash_caller) = compile_module("call_with_flags_and_value").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create both contracts: Constructors do nothing.
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller)).build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		// Read-only call fails when a non-zero value is set.
		assert_err_ignore_postinfo!(
			builder::call(addr_caller)
				.data(
					(addr_callee, pallet_revive_uapi::CallFlags::READ_ONLY.bits(), 100u64).encode()
				)
				.build(),
			<Error<Test>>::StateChangeDenied
		);
	});
}

#[test]
fn read_only_subsequent_call_cannot_store() {
	let (binary_read_only_caller, _code_hash_caller) = compile_module("read_only_call").unwrap();
	let (binary_caller, _code_hash_caller) = compile_module("call_with_flags_and_value").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("store_call").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create contracts: Constructors do nothing.
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_read_only_caller))
				.build_and_unwrap_contract();
		let Contract { addr: addr_subsequent_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller)).build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		// Subsequent call input.
		let input = (&addr_callee, pallet_revive_uapi::CallFlags::empty().bits(), 0u64, 100u32);

		// Read-only call fails when modifying storage.
		assert_err_ignore_postinfo!(
			builder::call(addr_caller)
				.data((&addr_subsequent_caller, input).encode())
				.build(),
			<Error<Test>>::ContractTrapped
		);
	});
}

#[test]
fn read_only_call_works() {
	let (binary_caller, _code_hash_caller) = compile_module("read_only_call").unwrap();
	let (binary_callee, _code_hash_callee) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create both contracts: Constructors do nothing.
		let Contract { addr: addr_caller, .. } =
			builder::bare_instantiate(Code::Upload(binary_caller)).build_and_unwrap_contract();
		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		assert_ok!(builder::call(addr_caller).data(addr_callee.encode()).build());
	});
}

#[test]
fn create1_with_value_works() {
	let (code, code_hash) = compile_module("create1_with_value").unwrap();
	let value = 42;
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create the contract: Constructor does nothing.
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: Deploys itself using create1 and the expected value
		assert_ok!(builder::call(addr).value(value).data(code_hash.encode()).build());

		// We should see the expected balance at the expected account
		let address = crate::address::create1(&addr, 1);
		let account_id = <Test as Config>::AddressMapper::to_account_id(&address);
		let usable_balance = <Test as Config>::Currency::usable_balance(&account_id);
		assert_eq!(usable_balance, value);
	});
}

#[test]
fn gas_price_api_works() {
	let (code, _) = compile_module("gas_price").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: It echoes back the value returned by the gas price API.
		let received = builder::bare_call(addr).build_and_unwrap_result();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(u64::from_le_bytes(received.data[..].try_into().unwrap()), u64::from(GAS_PRICE));
	});
}

#[test]
fn base_fee_api_works() {
	let (code, _) = compile_module("base_fee").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: It echoes back the value returned by the base fee API.
		let received = builder::bare_call(addr).build_and_unwrap_result();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(U256::from_little_endian(received.data[..].try_into().unwrap()), U256::zero());
	});
}

#[test]
fn call_data_size_api_works() {
	let (code, _) = compile_module("call_data_size").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: It echoes back the value returned by the call data size API.
		let received = builder::bare_call(addr).build_and_unwrap_result();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(u64::from_le_bytes(received.data.try_into().unwrap()), 0);

		let received = builder::bare_call(addr).data(vec![1; 256]).build_and_unwrap_result();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(u64::from_le_bytes(received.data.try_into().unwrap()), 256);
	});
}

#[test]
fn call_data_copy_api_works() {
	let (code, _) = compile_module("call_data_copy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call fixture: Expects an input of [255; 32] and executes tests.
		assert_ok!(builder::call(addr).data(vec![255; 32]).build());
	});
}

#[test]
fn static_data_limit_is_enforced() {
	let (oom_rw_trailing, _) = compile_module("oom_rw_trailing").unwrap();
	let (oom_rw_included, _) = compile_module("oom_rw_included").unwrap();
	let (oom_ro, _) = compile_module("oom_ro").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = Balances::set_balance(&ALICE, 1_000_000);

		assert_err!(
			Contracts::upload_code(
				RuntimeOrigin::signed(ALICE),
				oom_rw_trailing,
				deposit_limit::<Test>(),
			),
			<Error<Test>>::StaticMemoryTooLarge
		);

		assert_err!(
			Contracts::upload_code(
				RuntimeOrigin::signed(ALICE),
				oom_rw_included,
				deposit_limit::<Test>(),
			),
			<Error<Test>>::BlobTooLarge
		);

		assert_err!(
			Contracts::upload_code(RuntimeOrigin::signed(ALICE), oom_ro, deposit_limit::<Test>(),),
			<Error<Test>>::BlobTooLarge
		);
	});
}

#[test]
fn call_diverging_out_len_works() {
	let (code, _) = compile_module("call_diverging_out_len").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create the contract: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: It will issue calls and deploys, asserting on
		// correct output if the supplied output length was smaller than
		// than what the callee returned.
		assert_ok!(builder::call(addr).build());
	});
}

#[test]
fn chain_id_works() {
	let (code, _) = compile_module("chain_id").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let chain_id = U256::from(<Test as Config>::ChainId::get());
		let received = builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_result();
		assert_eq!(received.result.data, chain_id.encode());
	});
}

#[test]
fn call_data_load_api_works() {
	let (code, _) = compile_module("call_data_load").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: It reads a byte for the offset and then returns
		// what call data load returned using this byte as the offset.
		let input = (3u8, U256::max_value(), U256::max_value()).encode();
		let received = builder::bare_call(addr).data(input).build().result.unwrap();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(U256::from_little_endian(&received.data), U256::max_value());

		// Edge case
		let input = (2u8, U256::from(255).to_big_endian()).encode();
		let received = builder::bare_call(addr).data(input).build().result.unwrap();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(U256::from_little_endian(&received.data), U256::from(65280));

		// Edge case
		let received = builder::bare_call(addr).data(vec![1]).build().result.unwrap();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(U256::from_little_endian(&received.data), U256::zero());

		// OOB case
		let input = (42u8).encode();
		let received = builder::bare_call(addr).data(input).build().result.unwrap();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(U256::from_little_endian(&received.data), U256::zero());

		// No calldata should return the zero value
		let received = builder::bare_call(addr).build().result.unwrap();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(U256::from_little_endian(&received.data), U256::zero());
	});
}

#[test]
fn return_data_api_works() {
	let (code_return_data_api, _) = compile_module("return_data_api").unwrap();
	let (code_return_with_data, hash_return_with_data) =
		compile_module("return_with_data").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Upload the io echoing fixture for later use
		assert_ok!(Contracts::upload_code(
			RuntimeOrigin::signed(ALICE),
			code_return_with_data,
			deposit_limit::<Test>(),
		));

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code_return_data_api))
			.build_and_unwrap_contract();

		// Call the contract: It will issue calls and deploys, asserting on
		assert_ok!(builder::call(addr)
			.value(10 * 1024)
			.data(hash_return_with_data.encode())
			.build());
	});
}

#[test]
fn immutable_data_works() {
	let (code, _) = compile_module("immutable_data").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let data = [0xfe; 8];

		// Create fixture: Constructor sets the immtuable data
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.data(data.to_vec())
			.build_and_unwrap_contract();

		let contract = test_utils::get_contract(&addr);
		let account = <Test as Config>::AddressMapper::to_account_id(&addr);
		let actual_deposit =
			test_utils::get_balance_on_hold(&HoldReason::StorageDepositReserve.into(), &account);

		assert_eq!(contract.immutable_data_len(), data.len() as u32);

		// Storing immmutable data charges storage deposit; verify it explicitly.
		assert_eq!(actual_deposit, test_utils::contract_base_deposit(&addr));

		// make sure it is also recorded in the base deposit
		assert_eq!(
			test_utils::get_balance_on_hold(&HoldReason::StorageDepositReserve.into(), &account),
			contract.storage_base_deposit(),
		);

		// Call the contract: Asserts the input to equal the immutable data
		assert_ok!(builder::call(addr).data(data.to_vec()).build());
	});
}

#[test]
fn sbrk_cannot_be_deployed() {
	let (code, _) = compile_module("sbrk").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = Balances::set_balance(&ALICE, 1_000_000);

		assert_err!(
			Contracts::upload_code(
				RuntimeOrigin::signed(ALICE),
				code.clone(),
				deposit_limit::<Test>(),
			),
			<Error<Test>>::InvalidInstruction
		);

		assert_err!(
			builder::bare_instantiate(Code::Upload(code)).build().result,
			<Error<Test>>::InvalidInstruction
		);
	});
}

#[test]
fn overweight_basic_block_cannot_be_deployed() {
	let (code, _) = compile_module("basic_block").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = Balances::set_balance(&ALICE, 1_000_000);

		assert_err!(
			Contracts::upload_code(
				RuntimeOrigin::signed(ALICE),
				code.clone(),
				deposit_limit::<Test>(),
			),
			<Error<Test>>::BasicBlockTooLarge
		);

		assert_err!(
			builder::bare_instantiate(Code::Upload(code)).build().result,
			<Error<Test>>::BasicBlockTooLarge
		);
	});
}

#[test]
fn origin_api_works() {
	let (code, _) = compile_module("origin").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: Asserts the origin API to work as expected
		assert_ok!(builder::call(addr).build());
	});
}

#[test]
fn to_account_id_works() {
	let (code_hash_code, _) = compile_module("to_account_id").unwrap();

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		let _ = <Test as Config>::Currency::set_balance(&EVE, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code_hash_code)).build_and_unwrap_contract();

		// mapped account
		<Pallet<Test>>::map_account(RuntimeOrigin::signed(EVE)).unwrap();
		let expected_mapped_account_id = &<Test as Config>::AddressMapper::to_account_id(&EVE_ADDR);
		assert_ne!(
			expected_mapped_account_id.encode()[20..32],
			[0xEE; 12],
			"fallback suffix found where none should be"
		);
		assert_ok!(builder::call(addr)
			.data((EVE_ADDR, expected_mapped_account_id).encode())
			.build());

		// fallback for unmapped accounts
		let expected_fallback_account_id =
			&<Test as Config>::AddressMapper::to_account_id(&BOB_ADDR);
		assert_eq!(
			expected_fallback_account_id.encode()[20..32],
			[0xEE; 12],
			"no fallback suffix found where one should be"
		);
		assert_ok!(builder::call(addr)
			.data((BOB_ADDR, expected_fallback_account_id).encode())
			.build());
	});
}

#[test]
fn code_hash_works() {
	use crate::precompiles::{Precompile, EVM_REVERT};
	use precompiles::NoInfo;

	let builtin_precompile = H160(NoInfo::<Test>::MATCHER.base_address());
	let primitive_precompile = H160::from_low_u64_be(1);

	let (code_hash_code, self_code_hash) = compile_module("code_hash").unwrap();
	let (dummy_code, code_hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code_hash_code)).build_and_unwrap_contract();
		let Contract { addr: dummy_addr, .. } =
			builder::bare_instantiate(Code::Upload(dummy_code)).build_and_unwrap_contract();

		// code hash of dummy contract
		assert_ok!(builder::call(addr).data((dummy_addr, code_hash).encode()).build());
		// code hash of itself
		assert_ok!(builder::call(addr).data((addr, self_code_hash).encode()).build());
		// code hash of primitive pre-compile (exist but have no bytecode)
		assert_ok!(builder::call(addr)
			.data((primitive_precompile, crate::exec::EMPTY_CODE_HASH).encode())
			.build());
		// code hash of normal pre-compile (do have a bytecode)
		assert_ok!(builder::call(addr)
			.data((builtin_precompile, sp_io::hashing::keccak_256(&EVM_REVERT)).encode())
			.build());

		// EOA doesn't exists
		assert_err!(
			builder::bare_call(addr)
				.data((BOB_ADDR, crate::exec::EMPTY_CODE_HASH).encode())
				.build()
				.result,
			Error::<Test>::ContractTrapped
		);
		// non-existing will return zero
		assert_ok!(builder::call(addr).data((BOB_ADDR, H256::zero()).encode()).build());

		// create EOA
		let _ = <Test as Config>::Currency::set_balance(
			&<Test as Config>::AddressMapper::to_account_id(&BOB_ADDR),
			1_000_000,
		);

		// EOA returns empty code hash
		assert_ok!(builder::call(addr)
			.data((BOB_ADDR, crate::exec::EMPTY_CODE_HASH).encode())
			.build());
	});
}

#[test]
fn code_size_works() {
	let (tester_code, _) = compile_module("extcodesize").unwrap();
	let tester_code_len = tester_code.len() as u64;

	let (dummy_code, _) = compile_module("dummy").unwrap();
	let dummy_code_len = dummy_code.len() as u64;

	ExtBuilder::default().existential_deposit(1).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		let Contract { addr: tester_addr, .. } =
			builder::bare_instantiate(Code::Upload(tester_code)).build_and_unwrap_contract();
		let Contract { addr: dummy_addr, .. } =
			builder::bare_instantiate(Code::Upload(dummy_code)).build_and_unwrap_contract();

		// code size of another contract address
		assert_ok!(builder::call(tester_addr).data((dummy_addr, dummy_code_len).encode()).build());

		// code size of own contract address
		assert_ok!(builder::call(tester_addr)
			.data((tester_addr, tester_code_len).encode())
			.build());

		// code size of non contract accounts
		assert_ok!(builder::call(tester_addr).data(([8u8; 20], 0u64).encode()).build());
	});
}

#[test]
fn origin_must_be_mapped() {
	let (code, hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		<Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		<Test as Config>::Currency::set_balance(&EVE, 1_000_000);

		let eve = RuntimeOrigin::signed(EVE);

		// alice can instantiate as she doesn't need a mapping
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// without a mapping eve can neither call nor instantiate
		assert_err!(
			builder::bare_call(addr).origin(eve.clone()).build().result,
			<Error<Test>>::AccountUnmapped
		);
		assert_err!(
			builder::bare_instantiate(Code::Existing(hash))
				.origin(eve.clone())
				.build()
				.result,
			<Error<Test>>::AccountUnmapped
		);

		// after mapping eve is usable as an origin
		<Pallet<Test>>::map_account(eve.clone()).unwrap();
		assert_ok!(builder::bare_call(addr).origin(eve.clone()).build().result);
		assert_ok!(builder::bare_instantiate(Code::Existing(hash)).origin(eve).build().result);
	});
}

#[test]
fn mapped_address_works() {
	let (code, _) = compile_module("terminate_and_send_to_argument").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		<Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// without a mapping everything will be send to the fallback account
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code.clone())).build_and_unwrap_contract();
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE_FALLBACK), 0);
		builder::bare_call(addr).data(EVE_ADDR.encode()).build_and_unwrap_result();
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE_FALLBACK), 100);

		// after mapping it will be sent to the real eve account
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();
		// need some balance to pay for the map deposit
		<Test as Config>::Currency::set_balance(&EVE, 1_000);
		<Pallet<Test>>::map_account(RuntimeOrigin::signed(EVE)).unwrap();
		builder::bare_call(addr).data(EVE_ADDR.encode()).build_and_unwrap_result();
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE_FALLBACK), 100);
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE), 1_100);
	});
}

#[test]
fn recovery_works() {
	let (code, _) = compile_module("terminate_and_send_to_argument").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		<Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// eve puts her AccountId20 as argument to terminate but forgot to register
		// her AccountId32 first so now the funds are trapped in her fallback account
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code.clone())).build_and_unwrap_contract();
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE), 0);
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE_FALLBACK), 0);
		builder::bare_call(addr).data(EVE_ADDR.encode()).build_and_unwrap_result();
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE_FALLBACK), 100);
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE), 0);

		let call = RuntimeCall::Balances(pallet_balances::Call::transfer_all {
			dest: EVE,
			keep_alive: false,
		});

		// she now uses the recovery function to move all funds from the fallback
		// account to her real account
		<Pallet<Test>>::dispatch_as_fallback_account(RuntimeOrigin::signed(EVE), Box::new(call))
			.unwrap();
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE_FALLBACK), 0);
		assert_eq!(<Test as Config>::Currency::total_balance(&EVE), 100);
	});
}

#[test]
fn skip_transfer_works() {
	let (code_caller, _) = compile_module("call").unwrap();
	let (code, _) = compile_module("store_call").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		<Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		<Test as Config>::Currency::set_balance(&BOB, 0);

		// when gas is some (transfers enabled): bob has no money: fail
		assert_err!(
			Pallet::<Test>::dry_run_eth_transact(
				GenericTransaction {
					from: Some(BOB_ADDR),
					input: code.clone().into(),
					gas: Some(1u32.into()),
					..Default::default()
				},
				Weight::MAX,
				|_, _| 0u64,
			),
			EthTransactError::Message(format!(
				"insufficient funds for gas * price + value: address {BOB_ADDR:?} have 0 (supplied gas 1)"
			))
		);

		// no gas specified (all transfers are skipped): even without money bob can deploy
		assert_ok!(Pallet::<Test>::dry_run_eth_transact(
			GenericTransaction {
				from: Some(BOB_ADDR),
				input: code.clone().into(),
				..Default::default()
			},
			Weight::MAX,
			|_, _| 0u64,
		));

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let Contract { addr: caller_addr, .. } =
			builder::bare_instantiate(Code::Upload(code_caller)).build_and_unwrap_contract();

		// call directly: fails with enabled transfers
		assert_err!(
			Pallet::<Test>::dry_run_eth_transact(
				GenericTransaction {
					from: Some(BOB_ADDR),
					to: Some(addr),
					input: 0u32.encode().into(),
					gas: Some(1u32.into()),
					..Default::default()
				},
				Weight::MAX,
				|_, _| 0u64,
			),
			EthTransactError::Message(format!(
				"insufficient funds for gas * price + value: address {BOB_ADDR:?} have 0 (supplied gas 1)"
			))
		);

		// fails to call through other contract
		// we didn't roll back the storage changes done by the previous
		// call. So the item already exists. We simply increase the size of
		// the storage item to incur some deposits (which bob can't pay).
		assert!(Pallet::<Test>::dry_run_eth_transact(
			GenericTransaction {
				from: Some(BOB_ADDR),
				to: Some(caller_addr),
				input: (1u32, &addr).encode().into(),
				gas: Some(1u32.into()),
				..Default::default()
			},
			Weight::MAX,
			|_, _| 0u64,
		)
		.is_err(),);

		// works when no gas is specified (skip transfer)
		assert_ok!(Pallet::<Test>::dry_run_eth_transact(
			GenericTransaction {
				from: Some(BOB_ADDR),
				to: Some(addr),
				input: 2u32.encode().into(),
				..Default::default()
			},
			Weight::MAX,
			|_, _| 0u64,
		));

		// call through contract works when transfers are skipped
		assert_ok!(Pallet::<Test>::dry_run_eth_transact(
			GenericTransaction {
				from: Some(BOB_ADDR),
				to: Some(caller_addr),
				input: (3u32, &addr).encode().into(),
				..Default::default()
			},
			Weight::MAX,
			|_, _| 0u64,
		));

		// works with transfers enabled if we don't incur a storage cost
		// we shrink the item so its actually a refund
		assert_ok!(Pallet::<Test>::dry_run_eth_transact(
			GenericTransaction {
				from: Some(BOB_ADDR),
				to: Some(caller_addr),
				input: (2u32, &addr).encode().into(),
				gas: Some(1u32.into()),
				..Default::default()
			},
			Weight::MAX,
			|_, _| 0u64,
		));

		// fails when trying to increase the storage item size
		assert!(Pallet::<Test>::dry_run_eth_transact(
			GenericTransaction {
				from: Some(BOB_ADDR),
				to: Some(caller_addr),
				input: (3u32, &addr).encode().into(),
				gas: Some(1u32.into()),
				..Default::default()
			},
			Weight::MAX,
			|_, _| 0u64,
		)
		.is_err());
	});
}

#[test]
fn gas_limit_api_works() {
	let (code, _) = compile_module("gas_limit").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		// Create fixture: Constructor does nothing
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Call the contract: It echoes back the value returned by the gas limit API.
		let received = builder::bare_call(addr).build_and_unwrap_result();
		assert_eq!(received.flags, ReturnFlags::empty());
		assert_eq!(
			u64::from_le_bytes(received.data[..].try_into().unwrap()),
			<Test as frame_system::Config>::BlockWeights::get().max_block.ref_time()
		);
	});
}

#[test]
fn unknown_syscall_rejected() {
	let (code, _) = compile_module("unknown_syscall").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		<Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		assert_err!(
			builder::bare_instantiate(Code::Upload(code)).build().result,
			<Error<Test>>::CodeRejected,
		)
	});
}

#[test]
fn unstable_interface_rejected() {
	let (code, _) = compile_module("unstable_interface").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		<Test as Config>::Currency::set_balance(&ALICE, 1_000_000);

		Test::set_unstable_interface(false);
		assert_err!(
			builder::bare_instantiate(Code::Upload(code.clone())).build().result,
			<Error<Test>>::CodeRejected,
		);

		Test::set_unstable_interface(true);
		assert_ok!(builder::bare_instantiate(Code::Upload(code)).build().result);
	});
}

#[test]
fn tracing_works_for_transfers() {
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000);
		let mut tracer = CallTracer::new(Default::default(), |_| U256::zero());
		trace(&mut tracer, || {
			builder::bare_call(BOB_ADDR).evm_value(10.into()).build_and_unwrap_result();
		});

		let trace = tracer.collect_trace();
		assert_eq!(
			trace,
			Some(CallTrace {
				from: ALICE_ADDR,
				to: BOB_ADDR,
				value: Some(U256::from(10)),
				call_type: CallType::Call,
				..Default::default()
			})
		)
	});
}

#[test]
fn call_tracing_works() {
	use crate::evm::*;
	use CallType::*;
	let (code, _code_hash) = compile_module("tracing").unwrap();
	let (binary_callee, _) = compile_module("tracing_callee").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000);

		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(binary_callee)).build_and_unwrap_contract();

		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).evm_value(10_000_000.into()).build_and_unwrap_contract();


		let tracer_configs = vec![
			 CallTracerConfig{ with_logs: false, only_top_call: false},
			 CallTracerConfig{ with_logs: false, only_top_call: false},
			 CallTracerConfig{ with_logs: false, only_top_call: true},
		];

		// Verify that the first trace report the same weight reported by bare_call
		// TODO: fix tracing ( https://github.com/paritytech/polkadot-sdk/issues/8362 )
		/*
		let mut tracer = CallTracer::new(false, |w| w);
		let gas_used = trace(&mut tracer, || {
			builder::bare_call(addr).data((3u32, addr_callee).encode()).build().gas_consumed
		});
		let trace = tracer.collect_trace().unwrap();
		assert_eq!(&trace.gas_used, &gas_used);
		*/

		// Discarding gas usage, check that traces reported are correct
		for config in tracer_configs {
			let logs = if config.with_logs {
				vec![
					CallLog {
						address: addr,
						topics: Default::default(),
						data: b"before".to_vec().into(),
						position: 0,
					},
					CallLog {
						address: addr,
						topics: Default::default(),
						data: b"after".to_vec().into(),
						position: 1,
					},
				]
			} else {
				vec![]
			};

			let calls = if config.only_top_call {
				vec![]
			} else {
				vec![
						CallTrace {
							from: addr,
							to: addr_callee,
							input: 2u32.encode().into(),
							output: hex_literal::hex!(
										"08c379a00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000001a546869732066756e6374696f6e20616c77617973206661696c73000000000000"
									).to_vec().into(),
							revert_reason: Some("revert: This function always fails".to_string()),
							error: Some("execution reverted".to_string()),
							call_type: Call,
							value: Some(U256::from(0)),
							..Default::default()
						},
						CallTrace {
							from: addr,
							to: addr,
							input: (2u32, addr_callee).encode().into(),
							call_type: Call,
							logs: logs.clone(),
							value: Some(U256::from(0)),
							calls: vec![
								CallTrace {
									from: addr,
									to: addr_callee,
									input: 1u32.encode().into(),
									output: Default::default(),
									error: Some("ContractTrapped".to_string()),
									call_type: Call,
									value: Some(U256::from(0)),
									..Default::default()
								},
								CallTrace {
									from: addr,
									to: addr,
									input: (1u32, addr_callee).encode().into(),
									call_type: Call,
									logs: logs.clone(),
									value: Some(U256::from(0)),
									calls: vec![
										CallTrace {
											from: addr,
											to: addr_callee,
											input: 0u32.encode().into(),
											output: 0u32.to_le_bytes().to_vec().into(),
											call_type: Call,
											value: Some(U256::from(0)),
											..Default::default()
										},
										CallTrace {
											from: addr,
											to: addr,
											input: (0u32, addr_callee).encode().into(),
											call_type: Call,
											value: Some(U256::from(0)),
											calls: vec![
												CallTrace {
													from: addr,
													to: BOB_ADDR,
													value: Some(U256::from(100)),
													call_type: CallType::Call,
													..Default::default()
												}
											],
											..Default::default()
										},
									],
									..Default::default()
								},
							],
							..Default::default()
						},
					]
			};

			let mut tracer = CallTracer::new(config, |_| U256::zero());
			trace(&mut tracer, || {
				builder::bare_call(addr).data((3u32, addr_callee).encode()).build()
			});

			let trace = tracer.collect_trace();
			let expected_trace = CallTrace {
					from: ALICE_ADDR,
					to: addr,
					input: (3u32, addr_callee).encode().into(),
					call_type: Call,
					logs: logs.clone(),
					value: Some(U256::from(0)),
					calls: calls,
					..Default::default()
				};

			assert_eq!(
				trace,
				expected_trace.into(),
			);
		}
	});
}

#[test]
fn create_call_tracing_works() {
	use crate::evm::*;
	let (code, code_hash) = compile_module("create2_with_value").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000);

		let mut tracer = CallTracer::new(Default::default(), |_| U256::zero());

		let Contract { addr, .. } = trace(&mut tracer, || {
			builder::bare_instantiate(Code::Upload(code.clone()))
				.evm_value(100.into())
				.salt(None)
				.build_and_unwrap_contract()
		});

		let call_trace = tracer.collect_trace().unwrap();
		assert_eq!(
			call_trace,
			CallTrace {
				from: ALICE_ADDR,
				to: addr,
				value: Some(100.into()),
				input: Bytes(code.clone()),
				call_type: CallType::Create,
				..Default::default()
			}
		);

		let mut tracer = CallTracer::new(Default::default(), |_| U256::zero());
		let data = b"garbage";
		let input = (code_hash, data).encode();
		trace(&mut tracer, || {
			assert_ok!(builder::call(addr).data(input.clone()).build());
		});

		let call_trace = tracer.collect_trace().unwrap();
		let child_addr = crate::address::create2(&addr, &code, data, &[1u8; 32]);

		assert_eq!(
			call_trace,
			CallTrace {
				from: ALICE_ADDR,
				to: addr,
				value: Some(0.into()),
				input: input.clone().into(),
				calls: vec![CallTrace {
					from: addr,
					input: input.clone().into(),
					to: child_addr,
					value: Some(0.into()),
					call_type: CallType::Create2,
					..Default::default()
				},],
				..Default::default()
			}
		);
	});
}

#[test]
fn prestate_tracing_works() {
	use crate::evm::*;
	use alloc::collections::BTreeMap;

	let (dummy_code, _) = compile_module("dummy").unwrap();
	let (code, _) = compile_module("tracing").unwrap();
	let (callee_code, _) = compile_module("tracing_callee").unwrap();
	ExtBuilder::default().existential_deposit(200).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000);

		let Contract { addr: addr_callee, .. } =
			builder::bare_instantiate(Code::Upload(callee_code.clone()))
				.build_and_unwrap_contract();

		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code.clone()))
			.native_value(10)
			.build_and_unwrap_contract();

		// redact balance so that tests are resilient to weight changes
		let alice_redacted_balance = Some(U256::from(1));

		let test_cases: Vec<(Box<dyn FnOnce()>, _, _)> = vec![
			(
				Box::new(|| {
					builder::bare_call(addr)
						.data((3u32, addr_callee).encode())
						.build_and_unwrap_result();
				}),
				PrestateTracerConfig {
					diff_mode: false,
					disable_storage: false,
					disable_code: false,
				},
				PrestateTrace::Prestate(BTreeMap::from([
					(
						ALICE_ADDR,
						PrestateTraceInfo {
							balance: alice_redacted_balance,
							nonce: Some(2),
							..Default::default()
						},
					),
					(
						BOB_ADDR,
						PrestateTraceInfo { balance: Some(U256::from(0u64)), ..Default::default() },
					),
					(
						addr_callee,
						PrestateTraceInfo {
							balance: Some(U256::from(0u64)),
							code: Some(Bytes(callee_code.clone())),
							nonce: Some(1),
							..Default::default()
						},
					),
					(
						addr,
						PrestateTraceInfo {
							balance: Some(U256::from(10_000_000u64)),
							code: Some(Bytes(code.clone())),
							nonce: Some(1),
							..Default::default()
						},
					),
				])),
			),
			(
				Box::new(|| {
					builder::bare_call(addr)
						.data((3u32, addr_callee).encode())
						.build_and_unwrap_result();
				}),
				PrestateTracerConfig {
					diff_mode: true,
					disable_storage: false,
					disable_code: false,
				},
				PrestateTrace::DiffMode {
					pre: BTreeMap::from([
						(
							BOB_ADDR,
							PrestateTraceInfo {
								balance: Some(U256::from(100u64)),
								..Default::default()
							},
						),
						(
							addr,
							PrestateTraceInfo {
								balance: Some(U256::from(9_999_900u64)),
								code: Some(Bytes(code.clone())),
								nonce: Some(1),
								..Default::default()
							},
						),
					]),
					post: BTreeMap::from([
						(
							BOB_ADDR,
							PrestateTraceInfo {
								balance: Some(U256::from(200u64)),
								..Default::default()
							},
						),
						(
							addr,
							PrestateTraceInfo {
								balance: Some(U256::from(9_999_800u64)),
								..Default::default()
							},
						),
					]),
				},
			),
			(
				Box::new(|| {
					builder::bare_instantiate(Code::Upload(dummy_code.clone()))
						.salt(None)
						.build_and_unwrap_result();
				}),
				PrestateTracerConfig {
					diff_mode: true,
					disable_storage: false,
					disable_code: false,
				},
				PrestateTrace::DiffMode {
					pre: BTreeMap::from([(
						ALICE_ADDR,
						PrestateTraceInfo {
							balance: alice_redacted_balance,
							nonce: Some(2),
							..Default::default()
						},
					)]),
					post: BTreeMap::from([
						(
							ALICE_ADDR,
							PrestateTraceInfo {
								balance: alice_redacted_balance,
								nonce: Some(3),
								..Default::default()
							},
						),
						(
							create1(&ALICE_ADDR, 1),
							PrestateTraceInfo {
								code: Some(dummy_code.clone().into()),
								balance: Some(U256::from(0)),
								nonce: Some(1),
								..Default::default()
							},
						),
					]),
				},
			),
		];

		for (exec_call, config, expected_trace) in test_cases.into_iter() {
			let mut tracer = PrestateTracer::<Test>::new(config);
			trace(&mut tracer, || {
				exec_call();
			});

			let mut trace = tracer.collect_trace();

			// redact alice balance
			match trace {
				PrestateTrace::DiffMode { ref mut pre, ref mut post } => {
					pre.get_mut(&ALICE_ADDR).map(|info| {
						info.balance = alice_redacted_balance;
					});
					post.get_mut(&ALICE_ADDR).map(|info| {
						info.balance = alice_redacted_balance;
					});
				},
				PrestateTrace::Prestate(ref mut pre) => {
					pre.get_mut(&ALICE_ADDR).map(|info| {
						info.balance = alice_redacted_balance;
					});
				},
			}

			assert_eq!(trace, expected_trace);
		}
	});
}

#[test]
fn unknown_precompiles_revert() {
	let (code, _code_hash) = compile_module("read_only_call").unwrap();

	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		let cases: Vec<(H160, Box<dyn FnOnce(_)>)> = vec![(
			H160::from_low_u64_be(0x0a),
			Box::new(|result| {
				assert_err!(result, <Error<Test>>::UnsupportedPrecompileAddress);
			}),
		)];

		for (callee_addr, assert_result) in cases {
			let result =
				builder::bare_call(addr).data((callee_addr, [0u8; 0]).encode()).build().result;
			assert_result(result);
		}
	});
}

#[test]
fn pure_precompile_works() {
	use hex_literal::hex;

	let cases = vec![
		(
			"ECRecover",
			H160::from_low_u64_be(1),
			hex!("18c547e4f7b0f325ad1e56f57e26c745b09a3e503d86e00e5255ff7f715d3d1c000000000000000000000000000000000000000000000000000000000000001c73b1693892219d736caba55bdb67216e485557ea6b6af75f37096c9aa6a5a75feeb940b1d03b21e36b0e47e79769f095fe2ab855bd91e3a38756b7d75a9c4549").to_vec(),
			hex!("000000000000000000000000a94f5374fce5edbc8e2a8697c15331677e6ebf0b").to_vec(),
		),
		(
			"Sha256",
			H160::from_low_u64_be(2),
			hex!("ec07171c4f0f0e2b").to_vec(),
			hex!("d0591ea667763c69a5f5a3bae657368ea63318b2c9c8349cccaf507e3cbd7c7a").to_vec(),
		),
		(
			"Ripemd160",
			H160::from_low_u64_be(3),
			hex!("ec07171c4f0f0e2b").to_vec(),
			hex!("000000000000000000000000a9c5ebaf7589fd8acfd542c3a008956de84fbeb7").to_vec(),
		),
		(
			"Identity",
			H160::from_low_u64_be(4),
			[42u8; 128].to_vec(),
			[42u8; 128].to_vec(),
		),
		(
			"Modexp",
			H160::from_low_u64_be(5),
			hex!("00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000002003fffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2efffffffffffffffffffffffffffffffffffffffffffffffffffffffefffffc2f").to_vec(),
			hex!("0000000000000000000000000000000000000000000000000000000000000001").to_vec(),
		),
		(
			"Bn128Add",
			H160::from_low_u64_be(6),
			hex!("18b18acfb4c2c30276db5411368e7185b311dd124691610c5d3b74034e093dc9063c909c4720840cb5134cb9f59fa749755796819658d32efc0d288198f3726607c2b7f58a84bd6145f00c9c2bc0bb1a187f20ff2c92963a88019e7c6a014eed06614e20c147e940f2d70da3f74c9a17df361706a4485c742bd6788478fa17d7").to_vec(),
			hex!("2243525c5efd4b9c3d3c45ac0ca3fe4dd85e830a4ce6b65fa1eeaee202839703301d1d33be6da8e509df21cc35964723180eed7532537db9ae5e7d48f195c915").to_vec(),
		),
		(
			"Bn128Mul",
			H160::from_low_u64_be(7),
			hex!("2bd3e6d0f3b142924f5ca7b49ce5b9d54c4703d7ae5648e61d02268b1a0a9fb721611ce0a6af85915e2f1d70300909ce2e49dfad4a4619c8390cae66cefdb20400000000000000000000000000000000000000000000000011138ce750fa15c2").to_vec(),
			hex!("070a8d6a982153cae4be29d434e8faef8a47b274a053f5a4ee2a6c9c13c31e5c031b8ce914eba3a9ffb989f9cdd5b0f01943074bf4f0f315690ec3cec6981afc").to_vec(),
		),
		(
			"Bn128Pairing",
			H160::from_low_u64_be(8),
			hex!("1c76476f4def4bb94541d57ebba1193381ffa7aa76ada664dd31c16024c43f593034dd2920f673e204fee2811c678745fc819b55d3e9d294e45c9b03a76aef41209dd15ebff5d46c4bd888e51a93cf99a7329636c63514396b4a452003a35bf704bf11ca01483bfa8b34b43561848d28905960114c8ac04049af4b6315a416782bb8324af6cfc93537a2ad1a445cfd0ca2a71acd7ac41fadbf933c2a51be344d120a2a4cf30c1bf9845f20c6fe39e07ea2cce61f0c9bb048165fe5e4de877550111e129f1cf1097710d41c4ac70fcdfa5ba2023c6ff1cbeac322de49d1b6df7c2032c61a830e3c17286de9462bf242fca2883585b93870a73853face6a6bf411198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c21800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa").to_vec(),
			hex!("0000000000000000000000000000000000000000000000000000000000000001").to_vec(),
		),
		(
			"Blake2F",
			H160::from_low_u64_be(9),
			hex!("0000000048c9bdf267e6096a3ba7ca8485ae67bb2bf894fe72f36e3cf1361d5f3af54fa5d182e6ad7f520e511f6c3e2b8c68059b6bbd41fbabd9831f79217e1319cde05b61626300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000300000000000000000000000000000001").to_vec(),
			hex!("08c9bcf367e6096a3ba7ca8485ae67bb2bf894fe72f36e3cf1361d5f3af54fa5d282e6ad7f520e511f6c3e2b8c68059b9442be0454267ce079217e1319cde05b").to_vec(),
		),
	];

	for (description, precompile_addr, input, output) in cases {
		let (code, _code_hash) = compile_module("call_and_return").unwrap();
		ExtBuilder::default().build().execute_with(|| {
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
			let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
				.native_value(1_000)
				.build_and_unwrap_contract();

			let result = builder::bare_call(addr)
				.data(
					(&precompile_addr, 100u64)
						.encode()
						.into_iter()
						.chain(input)
						.collect::<Vec<_>>(),
				)
				.build_and_unwrap_result();

			assert_eq!(
				Pallet::<Test>::evm_balance(&precompile_addr),
				U256::from(100),
				"{description}: unexpected balance"
			);
			assert_eq!(
				alloy_core::hex::encode(result.data),
				alloy_core::hex::encode(output),
				"{description} Unexpected output for precompile: {precompile_addr:?}",
			);
			assert_eq!(result.flags, ReturnFlags::empty());
		});
	}
}

#[test]
fn precompiles_work() {
	use crate::precompiles::Precompile;
	use alloy_core::sol_types::{Panic, PanicKind, Revert, SolError, SolInterface, SolValue};
	use precompiles::{INoInfo, NoInfo};

	let precompile_addr = H160(NoInfo::<Test>::MATCHER.base_address());

	let cases = vec![
		(
			INoInfo::INoInfoCalls::identity(INoInfo::identityCall { number: 42u64.into() })
				.abi_encode(),
			42u64.abi_encode(),
			RuntimeReturnCode::Success,
		),
		(
			INoInfo::INoInfoCalls::reverts(INoInfo::revertsCall { error: "panic".to_string() })
				.abi_encode(),
			Revert::from("panic").abi_encode(),
			RuntimeReturnCode::CalleeReverted,
		),
		(
			INoInfo::INoInfoCalls::panics(INoInfo::panicsCall {}).abi_encode(),
			Panic::from(PanicKind::Assert).abi_encode(),
			RuntimeReturnCode::CalleeReverted,
		),
		(
			INoInfo::INoInfoCalls::errors(INoInfo::errorsCall {}).abi_encode(),
			Vec::new(),
			RuntimeReturnCode::CalleeTrapped,
		),
		// passing non decodeable input reverts with solidity panic
		(
			b"invalid".to_vec(),
			Panic::from(PanicKind::ResourceError).abi_encode(),
			RuntimeReturnCode::CalleeReverted,
		),
	];

	for (input, output, error_code) in cases {
		let (code, _code_hash) = compile_module("call_and_returncode").unwrap();
		ExtBuilder::default().build().execute_with(|| {
			let id = <Test as Config>::AddressMapper::to_account_id(&precompile_addr);
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
			let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
				.native_value(1000)
				.build_and_unwrap_contract();

			let result = builder::bare_call(addr)
				.data(
					(&precompile_addr, 0u64).encode().into_iter().chain(input).collect::<Vec<_>>(),
				)
				.build_and_unwrap_result();

			// no account or contract info should be created for a NoInfo pre-compile
			assert!(test_utils::get_contract_checked(&precompile_addr).is_none());
			assert!(!System::account_exists(&id));
			assert_eq!(Pallet::<Test>::evm_balance(&precompile_addr), U256::zero());

			assert_eq!(result.flags, ReturnFlags::empty());
			assert_eq!(u32::from_le_bytes(result.data[..4].try_into().unwrap()), error_code as u32);
			assert_eq!(
				&result.data[4..],
				&output,
				"Unexpected output for precompile: {precompile_addr:?}",
			);
		});
	}
}

#[test]
fn precompiles_with_info_creates_contract() {
	use crate::precompiles::Precompile;
	use alloy_core::sol_types::SolInterface;
	use precompiles::{IWithInfo, WithInfo};

	let precompile_addr = H160(WithInfo::<Test>::MATCHER.base_address());

	let cases = vec![(
		IWithInfo::IWithInfoCalls::dummy(IWithInfo::dummyCall {}).abi_encode(),
		Vec::<u8>::new(),
		RuntimeReturnCode::Success,
	)];

	for (input, output, error_code) in cases {
		let (code, _code_hash) = compile_module("call_and_returncode").unwrap();
		ExtBuilder::default().build().execute_with(|| {
			let id = <Test as Config>::AddressMapper::to_account_id(&precompile_addr);
			let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
			let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
				.native_value(1000)
				.build_and_unwrap_contract();

			let result = builder::bare_call(addr)
				.data(
					(&precompile_addr, 0u64).encode().into_iter().chain(input).collect::<Vec<_>>(),
				)
				.build_and_unwrap_result();

			// a pre-compile with contract info should create an account on first call
			assert!(test_utils::get_contract_checked(&precompile_addr).is_some());
			assert!(System::account_exists(&id));
			assert_eq!(Pallet::<Test>::evm_balance(&precompile_addr), U256::from(0));

			assert_eq!(result.flags, ReturnFlags::empty());
			assert_eq!(u32::from_le_bytes(result.data[..4].try_into().unwrap()), error_code as u32);
			assert_eq!(
				&result.data[4..],
				&output,
				"Unexpected output for precompile: {precompile_addr:?}",
			);
		});
	}
}

#[test]
fn bump_nonce_once_works() {
	let (code, hash) = compile_module("dummy").unwrap();

	ExtBuilder::default().existential_deposit(100).build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 1_000_000);
		frame_system::Account::<Test>::mutate(&ALICE, |account| account.nonce = 1);

		let _ = <Test as Config>::Currency::set_balance(&BOB, 1_000_000);
		frame_system::Account::<Test>::mutate(&BOB, |account| account.nonce = 1);

		builder::bare_instantiate(Code::Upload(code.clone()))
			.origin(RuntimeOrigin::signed(ALICE))
			.bump_nonce(BumpNonce::Yes)
			.salt(None)
			.build_and_unwrap_result();
		assert_eq!(System::account_nonce(&ALICE), 2);

		// instantiate again is ok
		let result = builder::bare_instantiate(Code::Existing(hash))
			.origin(RuntimeOrigin::signed(ALICE))
			.bump_nonce(BumpNonce::Yes)
			.salt(None)
			.build()
			.result;
		assert!(result.is_ok());

		builder::bare_instantiate(Code::Upload(code.clone()))
			.origin(RuntimeOrigin::signed(BOB))
			.bump_nonce(BumpNonce::No)
			.salt(None)
			.build_and_unwrap_result();
		assert_eq!(System::account_nonce(&BOB), 1);

		// instantiate again should fail
		let err = builder::bare_instantiate(Code::Upload(code))
			.origin(RuntimeOrigin::signed(BOB))
			.bump_nonce(BumpNonce::No)
			.salt(None)
			.build()
			.result
			.unwrap_err();

		assert_eq!(err, <Error<Test>>::DuplicateContract.into());
	});
}

#[test]
fn code_size_for_precompiles_works() {
	use crate::precompiles::Precompile;
	use precompiles::NoInfo;

	let builtin_precompile = H160(NoInfo::<Test>::MATCHER.base_address());
	let primitive_precompile = H160::from_low_u64_be(1);

	let (code, _code_hash) = compile_module("extcodesize").unwrap();
	ExtBuilder::default().build().execute_with(|| {
		let _ = <Test as Config>::Currency::set_balance(&ALICE, 100_000_000_000);
		let Contract { addr, .. } = builder::bare_instantiate(Code::Upload(code))
			.native_value(1000)
			.build_and_unwrap_contract();

		// the primitive pre-compiles return 0 code size on eth
		builder::bare_call(addr)
			.data((&primitive_precompile, 0u64).encode())
			.build_and_unwrap_result();

		// other precompiles should return the minimal evm revert code
		builder::bare_call(addr)
			.data((&builtin_precompile, 5u64).encode())
			.build_and_unwrap_result();
	});
}
