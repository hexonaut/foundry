//! Hooks to EVM execution
use super::{
    backend::CheatcodeBackend, memory_stackstate_owned::MemoryStackStateOwned, ConsoleCalls,
    HEVMCalls, HevmConsoleEvents,
};
use crate::{
    sputnik::{Executor, SputnikExecutor},
    Evm,
};

use sputnik::{
    backend::Backend,
    executor::stack::{
        Log, PrecompileFailure, PrecompileOutput, PrecompileSet, StackExecutor, StackExitKind,
        StackState, StackSubstateMetadata,
    },
    gasometer, Capture, Config, Context, CreateScheme, ExitError, ExitReason, ExitRevert,
    ExitSucceed, Handler, Runtime, Transfer,
};
use std::{process::Command, rc::Rc};

use ethers::{
    abi::{RawLog, Token},
    contract::EthLogDecode,
    core::{abi::AbiDecode, k256::ecdsa::SigningKey, utils},
    signers::{LocalWallet, Signer},
    types::{Address, H160, H256, U256},
};
use std::convert::Infallible;

use crate::sputnik::cheatcodes::patch_hardhat_console_log_selector;
use once_cell::sync::Lazy;

// This is now getting us the right hash? Also tried [..20]
// Lazy::new(|| Address::from_slice(&keccak256("hevm cheat code")[12..]));
/// Address where the Vm cheatcodes contract lives
pub static CHEATCODE_ADDRESS: Lazy<Address> = Lazy::new(|| {
    Address::from_slice(&hex::decode("7109709ECfa91a80626fF3989D68f67F5b1DD12D").unwrap())
});

// This is the address used by console.sol, vendored by nomiclabs/hardhat:
// https://github.com/nomiclabs/hardhat/blob/master/packages/hardhat-core/console.sol
pub static CONSOLE_ADDRESS: Lazy<Address> = Lazy::new(|| {
    Address::from_slice(&hex::decode("000000000000000000636F6e736F6c652e6c6f67").unwrap())
});

/// For certain cheatcodes, we may internally change the status of the call, i.e. in
/// `expectRevert`. Solidity will see a successful call and attempt to abi.decode for the called
/// function. Therefore, we need to populate the return with dummy bytes such that the decode
/// doesn't fail
pub static DUMMY_OUTPUT: [u8; 320] = [0u8; 320];

/// Hooks on live EVM execution and forwards everything else to a Sputnik [`Handler`].
///
/// It allows:
/// 1. Logging of values for debugging
/// 2. Modifying chain state live with cheatcodes
///
/// The `call_inner` and `create_inner` functions are copy-pasted from upstream, so that
/// it can hook in the runtime. They may eventually be removed if Sputnik allows bringing in your
/// own runtime handler.
#[derive(Clone, Debug)]
// TODO: Should this be called `HookedHandler`? Maybe we could implement other hooks
// here, e.g. hardhat console.log-style, or dapptools logs, some ad-hoc method for tracing
// etc.
pub struct CheatcodeHandler<H> {
    handler: H,
    enable_ffi: bool,
    console_logs: Vec<String>,
}

// Forwards everything internally except for the transact_call which is overwritten.
// TODO: Maybe we can pull this functionality up to the `Evm` trait to avoid having so many traits?
impl<'a, 'b, B: Backend, P: PrecompileSet> SputnikExecutor<CheatcodeStackState<'a, B>>
    for CheatcodeStackExecutor<'a, 'b, B, P>
{
    fn config(&self) -> &Config {
        self.handler.config()
    }

    fn state(&self) -> &CheatcodeStackState<'a, B> {
        self.handler.state()
    }

    fn state_mut(&mut self) -> &mut CheatcodeStackState<'a, B> {
        self.handler.state_mut()
    }

    fn expected_revert(&self) -> Option<&[u8]> {
        self.handler.state().expected_revert.as_deref()
    }

    fn gas_left(&self) -> U256 {
        // NB: We do this to avoid `function cannot return without recursing`
        U256::from(self.state().metadata().gasometer().gas())
    }

    fn transact_call(
        &mut self,
        caller: H160,
        address: H160,
        value: U256,
        data: Vec<u8>,
        gas_limit: u64,
        access_list: Vec<(H160, Vec<H256>)>,
    ) -> (ExitReason, Vec<u8>) {
        let transaction_cost = gasometer::call_transaction_cost(&data, &access_list);
        match self.state_mut().metadata_mut().gasometer_mut().record_transaction(transaction_cost) {
            Ok(()) => (),
            Err(e) => return (e.into(), Vec::new()),
        }

        // Initialize initial addresses for EIP-2929
        // Initialize initial addresses for EIP-2929
        if self.config().increase_state_access_gas {
            let addresses = core::iter::once(caller).chain(core::iter::once(address));
            self.state_mut().metadata_mut().access_addresses(addresses);

            self.handler.initialize_with_access_list(access_list);
        }

        self.state_mut().inc_nonce(caller);

        let context = Context { caller, address, apparent_value: value };

        match self.call_inner(
            address,
            Some(Transfer { source: caller, target: address, value }),
            data,
            Some(gas_limit),
            false,
            false,
            false,
            context,
        ) {
            Capture::Exit((s, v)) => (s, v),
            Capture::Trap(_) => unreachable!(),
        }
    }

    fn transact_create(
        &mut self,
        caller: H160,
        value: U256,
        init_code: Vec<u8>,
        gas_limit: u64,
        access_list: Vec<(H160, Vec<H256>)>,
    ) -> ExitReason {
        let transaction_cost = gasometer::create_transaction_cost(&init_code, &access_list);
        match self.state_mut().metadata_mut().gasometer_mut().record_transaction(transaction_cost) {
            Ok(()) => (),
            Err(e) => return e.into(),
        };
        self.handler.initialize_with_access_list(access_list);

        match self.create_inner(
            caller,
            CreateScheme::Legacy { caller },
            value,
            init_code,
            Some(gas_limit),
            false,
        ) {
            Capture::Exit((s, _, _)) => s,
            Capture::Trap(_) => unreachable!(),
        }
    }

    fn create_address(&self, scheme: CreateScheme) -> Address {
        self.handler.create_address(scheme)
    }

    fn clear_logs(&mut self) {
        self.state_mut().substate.logs_mut().clear()
    }

    fn logs(&self) -> Vec<String> {
        let logs = self.state().substate.logs().to_vec();
        logs.into_iter()
            .filter_map(|log| {
                // convert to the ethers type
                let log = RawLog { topics: log.topics, data: log.data };
                HevmConsoleEvents::decode_log(&log).ok()
            })
            .map(|event| {
                use HevmConsoleEvents::*;
                match event {
                    LogsFilter(inner) => format!("{}", inner.0),
                    LogBytesFilter(inner) => format!("{}", inner.0),
                    LogNamedAddressFilter(inner) => format!("{}: {:?}", inner.key, inner.val),
                    LogNamedBytes32Filter(inner) => {
                        format!("{}: 0x{}", inner.key, hex::encode(inner.val))
                    }
                    LogNamedDecimalIntFilter(inner) => format!(
                        "{}: {:?}",
                        inner.key,
                        ethers::utils::parse_units(inner.val, inner.decimals.as_u32()).unwrap()
                    ),
                    LogNamedDecimalUintFilter(inner) => {
                        format!(
                            "{}: {:?}",
                            inner.key,
                            ethers::utils::parse_units(inner.val, inner.decimals.as_u32()).unwrap()
                        )
                    }
                    LogNamedIntFilter(inner) => format!("{}: {:?}", inner.key, inner.val),
                    LogNamedUintFilter(inner) => format!("{}: {:?}", inner.key, inner.val),
                    LogNamedBytesFilter(inner) => {
                        format!("{}: 0x{}", inner.key, hex::encode(inner.val))
                    }
                    LogNamedStringFilter(inner) => format!("{}: {}", inner.key, inner.val),

                    e => e.to_string(),
                }
            })
            .chain(self.console_logs.clone())
            .collect()
    }
}

/// A [`MemoryStackStateOwned`] state instantiated over a [`CheatcodeBackend`]
pub type CheatcodeStackState<'a, B> = MemoryStackStateOwned<'a, CheatcodeBackend<B>>;

/// A [`CheatcodeHandler`] which uses a [`CheatcodeStackState`] to store its state and a
/// [`StackExecutor`] for executing transactions.
pub type CheatcodeStackExecutor<'a, 'b, B, P> =
    CheatcodeHandler<StackExecutor<'a, 'b, CheatcodeStackState<'a, B>, P>>;

impl<'a, 'b, B: Backend, P: PrecompileSet>
    Executor<CheatcodeStackState<'a, B>, CheatcodeStackExecutor<'a, 'b, B, P>>
{
    /// Instantiates a cheatcode-enabled [`Executor`]
    pub fn new_with_cheatcodes(
        backend: B,
        gas_limit: u64,
        config: &'a Config,
        precompiles: &'b P,
        enable_ffi: bool,
    ) -> Self {
        // make this a cheatcode-enabled backend
        let backend = CheatcodeBackend { backend, cheats: Default::default() };

        // create the memory stack state (owned, so that we can modify the backend via
        // self.state_mut on the transact_call fn)
        let metadata = StackSubstateMetadata::new(gas_limit, config);
        let state = MemoryStackStateOwned::new(metadata, backend);

        // create the executor and wrap it with the cheatcode handler
        let executor = StackExecutor::new_with_precompiles(state, config, precompiles);
        let executor = CheatcodeHandler { handler: executor, enable_ffi, console_logs: Vec::new() };

        let mut evm = Executor::from_executor(executor, gas_limit);

        // Need to create a non-empty contract at the cheat code address so that the EVM backend
        // thinks that something exists there.
        evm.initialize_contracts([
            (*CHEATCODE_ADDRESS, vec![0u8; 1].into()),
            (*CONSOLE_ADDRESS, vec![0u8; 1].into()),
        ]);

        evm
    }
}

// helper for creating an exit type
fn evm_error(retdata: &str) -> Capture<(ExitReason, Vec<u8>), Infallible> {
    Capture::Exit((
        ExitReason::Revert(ExitRevert::Reverted),
        ethers::abi::encode(&[Token::String(retdata.to_owned())]),
    ))
}

impl<'a, 'b, B: Backend, P: PrecompileSet> CheatcodeStackExecutor<'a, 'b, B, P> {
    /// Given a transaction's calldata, it tries to parse it a console call and print the call
    fn console_log(&mut self, input: Vec<u8>) -> Capture<(ExitReason, Vec<u8>), Infallible> {
        // replacing hardhat style selectors (`uint`) with abigen style (`uint256`)
        let input = patch_hardhat_console_log_selector(input);
        let decoded = match ConsoleCalls::decode(&input) {
            Ok(inner) => inner,
            Err(err) => return evm_error(&err.to_string()),
        };
        self.console_logs.push(decoded.to_string());
        Capture::Exit((ExitReason::Succeed(ExitSucceed::Stopped), vec![]))
    }

    /// Given a transaction's calldata, it tries to parse it as an [`HEVM cheatcode`](super::HEVM)
    /// call and modify the state accordingly.
    fn apply_cheatcode(
        &mut self,
        input: Vec<u8>,
        msg_sender: H160,
    ) -> Capture<(ExitReason, Vec<u8>), Infallible> {
        let mut res = vec![];

        // Get a mutable ref to the state so we can apply the cheats
        let state = self.state_mut();
        let decoded = match HEVMCalls::decode(&input) {
            Ok(inner) => inner,
            Err(err) => return evm_error(&err.to_string()),
        };

        match decoded {
            HEVMCalls::Warp(inner) => {
                state.backend.cheats.block_timestamp = Some(inner.0);
            }
            HEVMCalls::Roll(inner) => {
                state.backend.cheats.block_number = Some(inner.0);
            }
            HEVMCalls::Fee(inner) => {
                state.backend.cheats.block_base_fee_per_gas = Some(inner.0);
            }
            HEVMCalls::Store(inner) => {
                state.set_storage(inner.0, inner.1.into(), inner.2.into());
            }
            HEVMCalls::Load(inner) => {
                res = state.storage(inner.0, inner.1.into()).0.to_vec();
            }
            HEVMCalls::Ffi(inner) => {
                let args = inner.0;
                // if FFI is not explicitly enabled at runtime, do not let this be called
                // (we could have an FFI cheatcode executor instead but feels like
                // over engineering)
                if !self.enable_ffi {
                    return evm_error(
                        "ffi disabled: run again with --ffi if you want to allow tests to call external scripts",
                    );
                }

                // execute the command & get the stdout
                let output = match Command::new(&args[0]).args(&args[1..]).output() {
                    Ok(res) => res.stdout,
                    Err(err) => return evm_error(&err.to_string()),
                };

                // get the hex string & decode it
                let output = unsafe { std::str::from_utf8_unchecked(&output) };
                let decoded = match hex::decode(&output[2..]) {
                    Ok(res) => res,
                    Err(err) => return evm_error(&err.to_string()),
                };

                // encode the data as Bytes
                res = ethers::abi::encode(&[Token::Bytes(decoded.to_vec())]);
            }
            HEVMCalls::Addr(inner) => {
                let sk = inner.0;
                if sk.is_zero() {
                    return evm_error("Bad Cheat Code. Private Key cannot be 0.")
                }
                // 256 bit priv key -> 32 byte slice
                let mut bs: [u8; 32] = [0; 32];
                sk.to_big_endian(&mut bs);
                let xsk = match SigningKey::from_bytes(&bs) {
                    Ok(xsk) => xsk,
                    Err(err) => return evm_error(&err.to_string()),
                };
                let addr = utils::secret_key_to_address(&xsk);
                res = ethers::abi::encode(&[Token::Address(addr)]);
            }
            HEVMCalls::Sign(inner) => {
                let sk = inner.0;
                let digest = inner.1;
                if sk.is_zero() {
                    return evm_error("Bad Cheat Code. Private Key cannot be 0.")
                }
                // 256 bit priv key -> 32 byte slice
                let mut bs: [u8; 32] = [0; 32];
                sk.to_big_endian(&mut bs);

                let xsk = match SigningKey::from_bytes(&bs) {
                    Ok(xsk) => xsk,
                    Err(err) => return evm_error(&err.to_string()),
                };
                let wallet = LocalWallet::from(xsk).with_chain_id(self.handler.chain_id().as_u64());

                // The EVM precompile does not use EIP-155
                let sig = wallet.sign_hash(digest.into(), false);

                let recovered = sig.recover(digest).unwrap();
                assert_eq!(recovered, wallet.address());

                let mut r_bytes = [0u8; 32];
                let mut s_bytes = [0u8; 32];
                sig.r.to_big_endian(&mut r_bytes);
                sig.s.to_big_endian(&mut s_bytes);
                res = ethers::abi::encode(&[Token::Tuple(vec![
                    Token::Uint(sig.v.into()),
                    Token::FixedBytes(r_bytes.to_vec()),
                    Token::FixedBytes(s_bytes.to_vec()),
                ])]);
            }
            HEVMCalls::Prank(inner) => {
                let caller = inner.0;
                if let Some((orginal_pranker, caller, depth)) = self.state().msg_sender {
                    let start_prank_depth = if let Some(depth) = self.state().metadata().depth() {
                        depth + 1
                    } else {
                        0
                    };
                    // we allow someone to do a 1 time prank even when startPrank is set if
                    // and only if we ensure that the startPrank *cannot* be applied to the
                    // following call
                    if start_prank_depth == depth && caller == orginal_pranker {
                        return evm_error("You have an active `startPrank` at this frame depth already. Use either `prank` or `startPrank`, not both");
                    }
                }
                self.state_mut().next_msg_sender = Some(caller);
            }
            HEVMCalls::StartPrank(inner) => {
                // startPrank works by using frame depth to determine whether to overwrite
                // msg.sender if we set a prank caller at a particular depth, it
                // will continue to use the prank caller for any subsequent calls
                // until stopPrank is called.
                //
                // We additionally have to store the original message sender of the cheatcode caller
                // so that we dont apply it to any other addresses when depth ==
                // prank_depth
                let caller = inner.0;
                if self.state().next_msg_sender.is_some() {
                    return evm_error("You have an active `prank` call already. Use either `prank` or `startPrank`, not both");
                } else {
                    self.state_mut().msg_sender = Some((
                        msg_sender,
                        caller,
                        if let Some(depth) = self.state().metadata().depth() {
                            depth + 1
                        } else {
                            0
                        },
                    ));
                }
            }
            HEVMCalls::StopPrank(_) => {
                self.state_mut().msg_sender = None;
            }
            HEVMCalls::ExpectRevert(inner) => {
                if self.state().expected_revert.is_some() {
                    return evm_error(
                        "You must call another function prior to expecting a second revert.",
                    )
                } else {
                    self.state_mut().expected_revert = Some(inner.0.to_vec());
                }
            }
            HEVMCalls::Deal(inner) => {
                let who = inner.0;
                let value = inner.1;
                state.reset_balance(who);
                state.deposit(who, value);
            }
            HEVMCalls::Etch(inner) => {
                let who = inner.0;
                let code = inner.1;
                state.set_code(who, code.to_vec());
            }
        };

        // TODO: Add more cheat codes.
        Capture::Exit((ExitReason::Succeed(ExitSucceed::Stopped), res))
    }

    // NB: This function is copy-pasted from uptream's `execute`, adjusted so that we call the
    // Runtime with our own handler
    pub fn execute(&mut self, runtime: &mut Runtime) -> ExitReason {
        match runtime.run(self) {
            Capture::Exit(s) => s,
            Capture::Trap(_) => unreachable!("Trap is Infallible"),
        }
    }

    // NB: This function is copy-pasted from uptream's call_inner
    #[allow(clippy::too_many_arguments)]
    fn call_inner(
        &mut self,
        code_address: H160,
        transfer: Option<Transfer>,
        input: Vec<u8>,
        target_gas: Option<u64>,
        is_static: bool,
        take_l64: bool,
        take_stipend: bool,
        context: Context,
    ) -> Capture<(ExitReason, Vec<u8>), Infallible> {
        macro_rules! try_or_fail {
            ( $e:expr ) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return Capture::Exit((e.into(), Vec::new())),
                }
            };
        }

        fn l64(gas: u64) -> u64 {
            gas - gas / 64
        }

        let after_gas = if take_l64 && self.config().call_l64_after_gas {
            if self.config().estimate {
                let initial_after_gas = self.state().metadata().gasometer().gas();
                let diff = initial_after_gas - l64(initial_after_gas);
                try_or_fail!(self.state_mut().metadata_mut().gasometer_mut().record_cost(diff));
                self.state().metadata().gasometer().gas()
            } else {
                l64(self.state().metadata().gasometer().gas())
            }
        } else {
            self.state().metadata().gasometer().gas()
        };

        let target_gas = target_gas.unwrap_or(after_gas);
        let mut gas_limit = std::cmp::min(target_gas, after_gas);

        try_or_fail!(self.state_mut().metadata_mut().gasometer_mut().record_cost(gas_limit));

        if let Some(transfer) = transfer.as_ref() {
            if take_stipend && transfer.value != U256::zero() {
                gas_limit = gas_limit.saturating_add(self.config().call_stipend);
            }
        }

        let code = self.code(code_address);

        self.handler.enter_substate(gas_limit, is_static);
        self.state_mut().touch(context.address);

        if let Some(depth) = self.state().metadata().depth() {
            if depth > self.config().call_stack_limit {
                let _ = self.handler.exit_substate(StackExitKind::Reverted);
                return Capture::Exit((ExitError::CallTooDeep.into(), Vec::new()))
            }
        }

        if let Some(transfer) = transfer {
            match self.state_mut().transfer(transfer) {
                Ok(()) => (),
                Err(e) => {
                    let _ = self.handler.exit_substate(StackExitKind::Reverted);
                    return Capture::Exit((ExitReason::Error(e), Vec::new()))
                }
            }
        }

        if let Some(result) = self.handler.precompiles().execute(
            code_address,
            &input,
            Some(gas_limit),
            &context,
            is_static,
        ) {
            return match result {
                Ok(PrecompileOutput { exit_status, output, cost, logs }) => {
                    for Log { address, topics, data } in logs {
                        match self.log(address, topics, data) {
                            Ok(_) => continue,
                            Err(error) => return Capture::Exit((ExitReason::Error(error), output)),
                        }
                    }

                    let _ = self.state_mut().metadata_mut().gasometer_mut().record_cost(cost);
                    let _ = self.handler.exit_substate(StackExitKind::Succeeded);
                    Capture::Exit((ExitReason::Succeed(exit_status), output))
                }
                Err(e) => {
                    let e = match e {
                        PrecompileFailure::Error { exit_status } => ExitReason::Error(exit_status),
                        PrecompileFailure::Revert { exit_status, .. } => {
                            ExitReason::Revert(exit_status)
                        }
                        PrecompileFailure::Fatal { exit_status } => ExitReason::Fatal(exit_status),
                    };
                    let _ = self.handler.exit_substate(StackExitKind::Failed);
                    Capture::Exit((e, Vec::new()))
                }
            }
        }

        // each cfg is about 200 bytes, is this a lot to clone? why does this error
        // not manifest upstream?
        let config = self.config().clone();
        let mut runtime = Runtime::new(Rc::new(code), Rc::new(input), context, &config);
        let reason = self.execute(&mut runtime);
        // // log::debug!(target: "evm", "Call execution using address {}: {:?}", code_address,
        // reason);
        match reason {
            ExitReason::Succeed(s) => {
                let _ = self.handler.exit_substate(StackExitKind::Succeeded);
                Capture::Exit((ExitReason::Succeed(s), runtime.machine().return_value()))
            }
            ExitReason::Error(e) => {
                let _ = self.handler.exit_substate(StackExitKind::Failed);
                Capture::Exit((ExitReason::Error(e), Vec::new()))
            }
            ExitReason::Revert(e) => {
                let _ = self.handler.exit_substate(StackExitKind::Reverted);
                Capture::Exit((ExitReason::Revert(e), runtime.machine().return_value()))
            }
            ExitReason::Fatal(e) => {
                self.state_mut().metadata_mut().gasometer_mut().fail();
                let _ = self.handler.exit_substate(StackExitKind::Failed);
                Capture::Exit((ExitReason::Fatal(e), Vec::new()))
            }
        }
    }

    // NB: This function is copy-pasted from uptream's create_inner
    fn create_inner(
        &mut self,
        caller: H160,
        scheme: CreateScheme,
        value: U256,
        init_code: Vec<u8>,
        target_gas: Option<u64>,
        take_l64: bool,
    ) -> Capture<(ExitReason, Option<H160>, Vec<u8>), Infallible> {
        macro_rules! try_or_fail {
            ( $e:expr ) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return Capture::Exit((e.into(), None, Vec::new())),
                }
            };
        }

        fn check_first_byte(config: &Config, code: &[u8]) -> Result<(), ExitError> {
            if config.disallow_executable_format {
                if let Some(0xef) = code.get(0) {
                    return Err(ExitError::InvalidCode)
                }
            }
            Ok(())
        }

        fn l64(gas: u64) -> u64 {
            gas - gas / 64
        }

        let address = self.create_address(scheme);

        self.state_mut().metadata_mut().access_address(caller);
        self.state_mut().metadata_mut().access_address(address);

        if let Some(depth) = self.state().metadata().depth() {
            if depth > self.config().call_stack_limit {
                return Capture::Exit((ExitError::CallTooDeep.into(), None, Vec::new()))
            }
        }

        if self.balance(caller) < value {
            return Capture::Exit((ExitError::OutOfFund.into(), None, Vec::new()))
        }

        let after_gas = if take_l64 && self.config().call_l64_after_gas {
            if self.config().estimate {
                let initial_after_gas = self.state().metadata().gasometer().gas();
                let diff = initial_after_gas - l64(initial_after_gas);
                try_or_fail!(self.state_mut().metadata_mut().gasometer_mut().record_cost(diff));
                self.state().metadata().gasometer().gas()
            } else {
                l64(self.state().metadata().gasometer().gas())
            }
        } else {
            self.state().metadata().gasometer().gas()
        };

        let target_gas = target_gas.unwrap_or(after_gas);

        let gas_limit = core::cmp::min(after_gas, target_gas);
        try_or_fail!(self.state_mut().metadata_mut().gasometer_mut().record_cost(gas_limit));

        self.state_mut().inc_nonce(caller);

        self.handler.enter_substate(gas_limit, false);

        {
            if self.code_size(address) != U256::zero() {
                let _ = self.handler.exit_substate(StackExitKind::Failed);
                return Capture::Exit((ExitError::CreateCollision.into(), None, Vec::new()))
            }

            if self.handler.nonce(address) > U256::zero() {
                let _ = self.handler.exit_substate(StackExitKind::Failed);
                return Capture::Exit((ExitError::CreateCollision.into(), None, Vec::new()))
            }

            self.state_mut().reset_storage(address);
        }

        let context = Context { address, caller, apparent_value: value };
        let transfer = Transfer { source: caller, target: address, value };
        match self.state_mut().transfer(transfer) {
            Ok(()) => (),
            Err(e) => {
                let _ = self.handler.exit_substate(StackExitKind::Reverted);
                return Capture::Exit((ExitReason::Error(e), None, Vec::new()))
            }
        }

        if self.config().create_increase_nonce {
            self.state_mut().inc_nonce(address);
        }

        let config = self.config().clone();
        let mut runtime = Runtime::new(Rc::new(init_code), Rc::new(Vec::new()), context, &config);

        let reason = self.execute(&mut runtime);
        // log::debug!(target: "evm", "Create execution using address {}: {:?}", address, reason);

        match reason {
            ExitReason::Succeed(s) => {
                let out = runtime.machine().return_value();

                // As of EIP-3541 code starting with 0xef cannot be deployed
                if let Err(e) = check_first_byte(self.config(), &out) {
                    self.state_mut().metadata_mut().gasometer_mut().fail();
                    let _ = self.handler.exit_substate(StackExitKind::Failed);
                    return Capture::Exit((e.into(), None, Vec::new()))
                }

                if let Some(limit) = self.config().create_contract_limit {
                    if out.len() > limit {
                        self.state_mut().metadata_mut().gasometer_mut().fail();
                        let _ = self.handler.exit_substate(StackExitKind::Failed);
                        return Capture::Exit((
                            ExitError::CreateContractLimit.into(),
                            None,
                            Vec::new(),
                        ))
                    }
                }

                match self.state_mut().metadata_mut().gasometer_mut().record_deposit(out.len()) {
                    Ok(()) => {
                        let e = self.handler.exit_substate(StackExitKind::Succeeded);
                        self.state_mut().set_code(address, out);
                        try_or_fail!(e);
                        Capture::Exit((ExitReason::Succeed(s), Some(address), Vec::new()))
                    }
                    Err(e) => {
                        let _ = self.handler.exit_substate(StackExitKind::Failed);
                        Capture::Exit((ExitReason::Error(e), None, Vec::new()))
                    }
                }
            }
            ExitReason::Error(e) => {
                self.state_mut().metadata_mut().gasometer_mut().fail();
                let _ = self.handler.exit_substate(StackExitKind::Failed);
                Capture::Exit((ExitReason::Error(e), None, Vec::new()))
            }
            ExitReason::Revert(e) => {
                let _ = self.handler.exit_substate(StackExitKind::Reverted);
                Capture::Exit((ExitReason::Revert(e), None, runtime.machine().return_value()))
            }
            ExitReason::Fatal(e) => {
                self.state_mut().metadata_mut().gasometer_mut().fail();
                let _ = self.handler.exit_substate(StackExitKind::Failed);
                Capture::Exit((ExitReason::Fatal(e), None, Vec::new()))
            }
        }
    }
}

// Delegates everything internally, except the `call_inner` call, which is hooked
// so that we can modify
impl<'a, 'b, B: Backend, P: PrecompileSet> Handler for CheatcodeStackExecutor<'a, 'b, B, P> {
    type CreateInterrupt = Infallible;
    type CreateFeedback = Infallible;
    type CallInterrupt = Infallible;
    type CallFeedback = Infallible;

    fn call(
        &mut self,
        code_address: H160,
        transfer: Option<Transfer>,
        input: Vec<u8>,
        target_gas: Option<u64>,
        is_static: bool,
        context: Context,
    ) -> Capture<(ExitReason, Vec<u8>), Self::CallInterrupt> {
        // We intercept calls to the `CHEATCODE_ADDRESS` to apply the cheatcode directly
        // to the state.
        // NB: This is very similar to how Optimism's custom intercept logic to "predeploys" work
        // (e.g. with the StateManager)

        if code_address == *CHEATCODE_ADDRESS {
            self.apply_cheatcode(input, context.caller)
        } else if code_address == *CONSOLE_ADDRESS {
            self.console_log(input)
        } else {
            // modify execution context depending on the cheatcode
            let expected_revert = self.state_mut().expected_revert.take();
            let mut new_context = context;
            let mut new_transfer = transfer;

            // handle `startPrank` - see apply_cheatcodes for more info
            if let Some((original_msg_sender, permanent_caller, depth)) = self.state().msg_sender {
                let curr_depth =
                    if let Some(depth) = self.state().metadata().depth() { depth + 1 } else { 0 };
                if curr_depth == depth && new_context.caller == original_msg_sender {
                    new_context.caller = permanent_caller;

                    if let Some(t) = &new_transfer {
                        new_transfer = Some(Transfer {
                            source: permanent_caller,
                            target: t.target,
                            value: t.value,
                        });
                    }
                }
            }

            // handle normal `prank`
            if let Some(caller) = self.state_mut().next_msg_sender.take() {
                new_context.caller = caller;

                if let Some(t) = &new_transfer {
                    new_transfer =
                        Some(Transfer { source: caller, target: t.target, value: t.value });
                }
            }

            // perform the call
            let res = self.call_inner(
                code_address,
                new_transfer,
                input,
                target_gas,
                is_static,
                true,
                true,
                new_context,
            );

            if let Some(expected_revert) = expected_revert {
                let final_res = match res {
                    Capture::Exit((ExitReason::Revert(_e), data)) => {
                        if data.len() >= 4 && data[0..4] == [8, 195, 121, 160] {
                            // its a revert string
                            let decoded_data =
                                ethers::abi::decode(&[ethers::abi::ParamType::Bytes], &data[4..])
                                    .expect("String error code, but not actual string");
                            let decoded_data = decoded_data[0]
                                .clone()
                                .into_bytes()
                                .expect("Can never fail because it is bytes");
                            if decoded_data == *expected_revert {
                                return Capture::Exit((
                                    ExitReason::Succeed(ExitSucceed::Returned),
                                    DUMMY_OUTPUT.to_vec(),
                                ))
                            } else {
                                return evm_error(&*format!(
                                    "Error != expected error: '{}' != '{}'",
                                    String::from_utf8_lossy(&decoded_data[..]),
                                    String::from_utf8_lossy(&expected_revert)
                                ))
                            }
                        }

                        if data == *expected_revert {
                            Capture::Exit((
                                ExitReason::Succeed(ExitSucceed::Returned),
                                DUMMY_OUTPUT.to_vec(),
                            ))
                        } else {
                            evm_error(&*format!(
                                "Error data != expected error data: 0x{} != 0x{}",
                                hex::encode(data),
                                hex::encode(expected_revert)
                            ))
                        }
                    }
                    _ => evm_error("Expected revert call did not revert"),
                };
                final_res
            } else {
                res
            }
        }
    }

    // Everything else is left the same
    fn balance(&self, address: H160) -> U256 {
        self.handler.balance(address)
    }

    fn code_size(&self, address: H160) -> U256 {
        self.handler.code_size(address)
    }

    fn code_hash(&self, address: H160) -> H256 {
        self.handler.code_hash(address)
    }

    fn code(&self, address: H160) -> Vec<u8> {
        self.handler.code(address)
    }

    fn storage(&self, address: H160, index: H256) -> H256 {
        self.handler.storage(address, index)
    }

    fn original_storage(&self, address: H160, index: H256) -> H256 {
        self.handler.original_storage(address, index)
    }

    fn gas_left(&self) -> U256 {
        // Need to disambiguate type, because the same method exists in the `SputnikExecutor`
        // trait and the `Handler` trait.
        Handler::gas_left(&self.handler)
    }

    fn gas_price(&self) -> U256 {
        self.handler.gas_price()
    }

    fn origin(&self) -> H160 {
        self.handler.origin()
    }

    fn block_hash(&self, number: U256) -> H256 {
        self.handler.block_hash(number)
    }

    fn block_number(&self) -> U256 {
        self.handler.block_number()
    }

    fn block_coinbase(&self) -> H160 {
        self.handler.block_coinbase()
    }

    fn block_timestamp(&self) -> U256 {
        self.handler.block_timestamp()
    }

    fn block_difficulty(&self) -> U256 {
        self.handler.block_difficulty()
    }

    fn block_gas_limit(&self) -> U256 {
        self.handler.block_gas_limit()
    }

    fn block_base_fee_per_gas(&self) -> U256 {
        self.handler.block_base_fee_per_gas()
    }

    fn chain_id(&self) -> U256 {
        self.handler.chain_id()
    }

    fn exists(&self, address: H160) -> bool {
        self.handler.exists(address)
    }

    fn deleted(&self, address: H160) -> bool {
        self.handler.deleted(address)
    }

    fn is_cold(&self, address: H160, index: Option<H256>) -> bool {
        self.handler.is_cold(address, index)
    }

    fn set_storage(&mut self, address: H160, index: H256, value: H256) -> Result<(), ExitError> {
        self.handler.set_storage(address, index, value)
    }

    fn log(&mut self, address: H160, topics: Vec<H256>, data: Vec<u8>) -> Result<(), ExitError> {
        self.handler.log(address, topics, data)
    }

    fn mark_delete(&mut self, address: H160, target: H160) -> Result<(), ExitError> {
        self.handler.mark_delete(address, target)
    }

    fn create(
        &mut self,
        caller: H160,
        scheme: CreateScheme,
        value: U256,
        init_code: Vec<u8>,
        target_gas: Option<u64>,
    ) -> Capture<(ExitReason, Option<H160>, Vec<u8>), Self::CreateInterrupt> {
        self.handler.create(caller, scheme, value, init_code, target_gas)
    }

    fn pre_validate(
        &mut self,
        context: &Context,
        opcode: sputnik::Opcode,
        stack: &sputnik::Stack,
    ) -> Result<(), ExitError> {
        self.handler.pre_validate(context, opcode, stack)
    }
}

#[cfg(test)]
mod tests {
    use crate::{fuzz::FuzzedExecutor, sputnik::helpers::vm, test_helpers::COMPILED, Evm};

    use super::*;

    #[test]
    fn ds_test_logs() {
        let mut evm = vm();
        let compiled = COMPILED.find("DebugLogs").expect("could not find contract");
        let (addr, _, _, _) =
            evm.deploy(Address::zero(), compiled.bytecode().unwrap().clone(), 0.into()).unwrap();

        // after the evm call is done, we call `logs` and print it all to the user
        let (_, _, _, logs) =
            evm.call::<(), _, _>(Address::zero(), addr, "test_log()", (), 0.into()).unwrap();
        let expected = [
            "Hi",
            "0x1234",
            "0x1111111111111111111111111111111111111111",
            "0x41b1a0649752af1b28b3dc29a1556eee781e4a4c3a1f7f53f90fa834de098c4d",
            "123",
            "1234",
            "0x4567",
            "lol",
            "addr: 0x2222222222222222222222222222222222222222",
            "key: 0x41b1a0649752af1b28b3dc29a1556eee781e4a4c3a1f7f53f90fa834de098c4d",
            "key: 123000000000000000000",
            "key: 1234000000000000000000",
            "key: 123",
            "key: 1234",
            "key: 0x4567",
            "key: lol",
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        assert_eq!(logs, expected);
    }

    #[test]
    fn console_logs() {
        let mut evm = vm();

        let compiled = COMPILED.find("ConsoleLogs").expect("could not find contract");
        let (addr, _, _, _) =
            evm.deploy(Address::zero(), compiled.bytecode().unwrap().clone(), 0.into()).unwrap();

        // after the evm call is done, we call `logs` and print it all to the user
        let (_, _, _, logs) =
            evm.call::<(), _, _>(Address::zero(), addr, "test_log()", (), 0.into()).unwrap();
        let expected = [
            "0x1111111111111111111111111111111111111111",
            "Hi",
            "Hi, Hi",
            "1337",
            "1337, 1245",
            "Hi, 1337",
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        assert_eq!(logs, expected);
    }

    #[test]
    fn logs_external_contract() {
        let mut evm = vm();

        let compiled = COMPILED.find("DebugLogs").expect("could not find contract");
        let (addr, _, _, _) =
            evm.deploy(Address::zero(), compiled.bytecode().unwrap().clone(), 0.into()).unwrap();

        // after the evm call is done, we call `logs` and print it all to the user
        let (_, _, _, logs) = evm
            .call::<(), _, _>(Address::zero(), addr, "test_log_elsewhere()", (), 0.into())
            .unwrap();
        let expected = ["0x1111111111111111111111111111111111111111", "Hi"]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(logs, expected);
    }

    #[test]
    fn cheatcodes() {
        let mut evm = vm();
        let compiled = COMPILED.find("CheatCodes").expect("could not find contract");
        let (addr, _, _, _) =
            evm.deploy(Address::zero(), compiled.bytecode().unwrap().clone(), 0.into()).unwrap();

        let state = evm.state().clone();
        let mut cfg = proptest::test_runner::Config::default();
        cfg.failure_persistence = None;
        let runner = proptest::test_runner::TestRunner::new(cfg);

        // ensure the storage slot is set at 10 anyway
        let (storage_contract, _, _, _) = evm
            .call::<Address, _, _>(Address::zero(), addr, "store()(address)", (), 0.into())
            .unwrap();
        let (slot, _, _, _) = evm
            .call::<U256, _, _>(Address::zero(), storage_contract, "slot0()(uint256)", (), 0.into())
            .unwrap();
        assert_eq!(slot, 10.into());

        let evm = FuzzedExecutor::new(&mut evm, runner, Address::zero());

        let abi = compiled.abi.as_ref().unwrap();
        for func in abi.functions().filter(|func| func.name.starts_with("test")) {
            // Skip the FFI unit test if not in a unix system
            if func.name == "testFFI" && !cfg!(unix) {
                continue
            }

            let should_fail = func.name.starts_with("testFail");
            if func.inputs.is_empty() {
                let (_, reason, _, _) =
                    evm.as_mut().call_unchecked(Address::zero(), addr, func, (), 0.into()).unwrap();
                assert!(evm.as_mut().check_success(addr, &reason, should_fail));
            } else {
                assert!(evm.fuzz(func, addr, should_fail).is_ok());
            }

            evm.as_mut().reset(state.clone());
        }
    }

    #[test]
    fn ffi_fails_if_disabled() {
        let mut evm = vm();
        evm.executor.enable_ffi = false;

        let compiled = COMPILED.find("CheatCodes").expect("could not find contract");
        let (addr, _, _, _) =
            evm.deploy(Address::zero(), compiled.bytecode().unwrap().clone(), 0.into()).unwrap();

        let err =
            evm.call::<(), _, _>(Address::zero(), addr, "testFFI()", (), 0.into()).unwrap_err();
        let reason = match err {
            crate::EvmError::Execution { reason, .. } => reason,
            _ => panic!("unexpected error"),
        };
        assert_eq!(reason, "ffi disabled: run again with --ffi if you want to allow tests to call external scripts");
    }
}
