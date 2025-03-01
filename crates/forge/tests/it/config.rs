//! Test config.

use crate::test_helpers::{COMPILED, COMPILED_ZK, EVM_OPTS, PROJECT};
use alloy_primitives::keccak256;
use forge::{
    result::{SuiteResult, TestStatus},
    MultiContractRunner, MultiContractRunnerBuilder, TestOptions, TestOptionsBuilder,
};
use foundry_compilers::Artifact;
use foundry_config::{
    fs_permissions::PathPermission, Config, FsPermissions, FuzzConfig, FuzzDictionaryConfig,
    InvariantConfig, RpcEndpoint, RpcEndpoints,
};
use foundry_evm::{
    decode::decode_console_logs,
    inspectors::CheatsConfig,
    revm::primitives::SpecId,
    traces::{render_trace_arena, CallTraceDecoderBuilder},
};
use foundry_test_utils::{init_tracing, Filter};
use foundry_zksync_compiler::{DualCompiledContract, PackedEraBytecode};
use futures::future::join_all;
use itertools::Itertools;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

/// How to execute a test run.
pub struct TestConfig {
    pub runner: MultiContractRunner,
    pub should_fail: bool,
    pub filter: Filter,
    pub opts: TestOptions,
}

impl TestConfig {
    pub fn new(runner: MultiContractRunner) -> Self {
        Self::with_filter(runner, Filter::matches_all())
    }

    pub async fn filter(filter: Filter) -> Self {
        Self::with_filter(runner().await, filter)
    }

    pub fn with_filter(runner: MultiContractRunner, filter: Filter) -> Self {
        init_tracing();
        Self { runner, should_fail: false, filter, opts: test_opts() }
    }

    pub fn evm_spec(mut self, spec: SpecId) -> Self {
        self.runner.evm_spec = spec;
        self
    }

    pub fn should_fail(self) -> Self {
        self.set_should_fail(true)
    }

    pub fn set_should_fail(mut self, should_fail: bool) -> Self {
        self.should_fail = should_fail;
        self
    }

    /// Executes the test runner
    pub async fn test(&mut self) -> BTreeMap<String, SuiteResult> {
        self.runner.test_collect(&self.filter, self.opts.clone()).await
    }

    pub async fn run(&mut self) {
        self.try_run().await.unwrap()
    }

    /// Executes the test case
    ///
    /// Returns an error if
    ///    * filter matched 0 test cases
    ///    * a test results deviates from the configured `should_fail` setting
    pub async fn try_run(&mut self) -> eyre::Result<()> {
        let suite_result = self.test().await;
        if suite_result.is_empty() {
            eyre::bail!("empty test result");
        }
        for (_, SuiteResult { test_results, .. }) in suite_result {
            for (test_name, result) in test_results {
                if self.should_fail && (result.status == TestStatus::Success) ||
                    !self.should_fail && (result.status == TestStatus::Failure)
                {
                    let logs = decode_console_logs(&result.logs);
                    let outcome = if self.should_fail { "fail" } else { "pass" };
                    let call_trace_decoder = CallTraceDecoderBuilder::default().build();
                    let decoded_traces = join_all(
                        result
                            .traces
                            .iter()
                            .map(|(_, a)| render_trace_arena(a, &call_trace_decoder))
                            .collect::<Vec<_>>(),
                    )
                    .await
                    .into_iter()
                    .map(|x| x.unwrap())
                    .collect::<Vec<_>>();
                    eyre::bail!(
                        "Test {} did not {} as expected.\nReason: {:?}\nLogs:\n{}\n\nTraces:\n{}",
                        test_name,
                        outcome,
                        result.reason,
                        logs.join("\n"),
                        decoded_traces.into_iter().format("\n"),
                    )
                }
            }
        }

        Ok(())
    }
}

/// Returns the [`TestOptions`] used by the tests.
pub fn test_opts() -> TestOptions {
    TestOptionsBuilder::default()
        .fuzz(FuzzConfig {
            runs: 256,
            max_test_rejects: 65536,
            seed: None,
            dictionary: FuzzDictionaryConfig {
                include_storage: true,
                include_push_bytes: true,
                dictionary_weight: 40,
                max_fuzz_dictionary_addresses: 10_000,
                max_fuzz_dictionary_values: 10_000,
            },
        })
        .invariant(InvariantConfig {
            runs: 256,
            depth: 15,
            fail_on_revert: false,
            call_override: false,
            dictionary: FuzzDictionaryConfig {
                dictionary_weight: 80,
                include_storage: true,
                include_push_bytes: true,
                max_fuzz_dictionary_addresses: 10_000,
                max_fuzz_dictionary_values: 10_000,
            },
            shrink_sequence: true,
            shrink_run_limit: 2usize.pow(18u32),
        })
        .build(&COMPILED, &PROJECT.paths.root)
        .expect("Config loaded")
}

pub fn manifest_root() -> &'static Path {
    let mut root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // need to check here where we're executing the test from, if in `forge` we need to also allow
    // `testdata`
    if root.ends_with("forge") {
        root = root.parent().unwrap();
    }
    root
}

/// Builds a base runner
pub fn base_runner() -> MultiContractRunnerBuilder {
    init_tracing();
    MultiContractRunnerBuilder::default().sender(EVM_OPTS.sender)
}

/// Builds a non-tracing runner
pub async fn runner() -> MultiContractRunner {
    let mut config = Config::with_root(PROJECT.root());
    config.fs_permissions = FsPermissions::new(vec![PathPermission::read_write(manifest_root())]);
    runner_with_config(config).await
}

/// Builds a non-tracing runner
pub async fn runner_with_config(mut config: Config) -> MultiContractRunner {
    config.rpc_endpoints = rpc_endpoints();
    config.allow_paths.push(manifest_root().to_path_buf());

    let root = &PROJECT.paths.root;
    let opts = &*EVM_OPTS;
    let env = opts.evm_env().await.expect("could not instantiate fork environment");
    let output = COMPILED.clone();
    base_runner()
        .with_test_options(test_opts())
        .with_cheats_config(CheatsConfig::new(
            &config,
            opts.clone(),
            None,
            Default::default(),
            false,
        ))
        .sender(config.sender)
        .build(root, output, env, opts.clone())
        .unwrap()
}

/// Builds a non-tracing zk runner
pub async fn runner_with_config_and_zk(mut config: Config) -> MultiContractRunner {
    config.rpc_endpoints = rpc_endpoints();
    config.allow_paths.push(manifest_root().to_path_buf());

    let root = &PROJECT.paths.root;
    let opts = &*EVM_OPTS;
    let env = opts.evm_env().await.expect("could not instantiate fork environment");
    let output = COMPILED.clone();
    let zk_output = COMPILED_ZK.clone();

    // Dual compiled contracts
    let mut dual_compiled_contracts = vec![];
    let mut solc_bytecodes = HashMap::new();
    for (contract_name, artifact) in output.artifacts() {
        let contract_name =
            contract_name.split('.').next().expect("name cannot be empty").to_string();
        let deployed_bytecode = artifact.get_deployed_bytecode();
        let deployed_bytecode = deployed_bytecode
            .as_ref()
            .and_then(|d| d.bytecode.as_ref().and_then(|b| b.object.as_bytes()));
        let bytecode = artifact.get_bytecode().and_then(|b| b.object.as_bytes().cloned());
        if let Some(bytecode) = bytecode {
            if let Some(deployed_bytecode) = deployed_bytecode {
                solc_bytecodes.insert(contract_name.clone(), (bytecode, deployed_bytecode.clone()));
            }
        }
    }

    // TODO make zk optional and solc default
    for (contract_name, artifact) in zk_output.artifacts() {
        let deployed_bytecode = artifact.get_deployed_bytecode();
        let deployed_bytecode = deployed_bytecode
            .as_ref()
            .and_then(|d| d.bytecode.as_ref().and_then(|b| b.object.as_bytes()));
        if let Some(deployed_bytecode) = deployed_bytecode {
            let packed_bytecode = PackedEraBytecode::from_vec(deployed_bytecode);
            if let Some((solc_bytecode, solc_deployed_bytecode)) =
                solc_bytecodes.get(&contract_name)
            {
                dual_compiled_contracts.push(DualCompiledContract {
                    name: contract_name,
                    zk_bytecode_hash: packed_bytecode.bytecode_hash(),
                    zk_deployed_bytecode: packed_bytecode.bytecode(),
                    evm_bytecode_hash: keccak256(solc_deployed_bytecode),
                    evm_bytecode: solc_bytecode.to_vec(),
                    evm_deployed_bytecode: solc_deployed_bytecode.to_vec(),
                });
            }
        }
    }

    base_runner()
        .with_test_options(test_opts())
        .with_cheats_config(CheatsConfig::new(
            &config,
            opts.clone(),
            None,
            dual_compiled_contracts,
            false,
        ))
        .sender(config.sender)
        .build(root, output, env, opts.clone())
        .unwrap()
}

/// Builds a tracing runner
pub async fn tracing_runner() -> MultiContractRunner {
    let mut opts = EVM_OPTS.clone();
    opts.verbosity = 5;
    base_runner()
        .build(
            &PROJECT.paths.root,
            (*COMPILED).clone(),
            EVM_OPTS.evm_env().await.expect("Could not instantiate fork environment"),
            opts,
        )
        .unwrap()
}

// Builds a runner that runs against forked state
pub async fn forked_runner(rpc: &str) -> MultiContractRunner {
    let mut opts = EVM_OPTS.clone();

    opts.env.chain_id = None; // clear chain id so the correct one gets fetched from the RPC
    opts.fork_url = Some(rpc.to_string());

    let env = opts.evm_env().await.expect("Could not instantiate fork environment");
    let fork = opts.get_fork(&Default::default(), env.clone());

    base_runner()
        .with_fork(fork)
        .build(&PROJECT.paths.root, (*COMPILED).clone(), env, opts)
        .unwrap()
}

/// the RPC endpoints used during tests
pub fn rpc_endpoints() -> RpcEndpoints {
    RpcEndpoints::new([
        (
            "rpcAlias",
            RpcEndpoint::Url(
                "https://eth-mainnet.alchemyapi.io/v2/Lc7oIGYeL_QvInzI0Wiu_pOZZDEKBrdf".to_string(),
            ),
        ),
        ("rpcEnvAlias", RpcEndpoint::Env("${RPC_ENV_ALIAS}".to_string())),
    ])
}

/// A helper to assert the outcome of multiple tests with helpful assert messages
#[track_caller]
#[allow(clippy::type_complexity)]
pub fn assert_multiple(
    actuals: &BTreeMap<String, SuiteResult>,
    expecteds: BTreeMap<
        &str,
        Vec<(&str, bool, Option<String>, Option<Vec<String>>, Option<usize>)>,
    >,
) {
    assert_eq!(actuals.len(), expecteds.len(), "We did not run as many contracts as we expected");
    for (contract_name, tests) in &expecteds {
        assert!(
            actuals.contains_key(*contract_name),
            "We did not run the contract {contract_name}"
        );

        assert_eq!(
            actuals[*contract_name].len(),
            expecteds[contract_name].len(),
            "We did not run as many test functions as we expected for {contract_name}"
        );
        for (test_name, should_pass, reason, expected_logs, expected_warning_count) in tests {
            let logs = &actuals[*contract_name].test_results[*test_name].decoded_logs;

            let warnings_count = &actuals[*contract_name].warnings.len();

            if *should_pass {
                assert!(
                    actuals[*contract_name].test_results[*test_name].status == TestStatus::Success,
                    "Test {} did not pass as expected.\nReason: {:?}\nLogs:\n{}",
                    test_name,
                    actuals[*contract_name].test_results[*test_name].reason,
                    logs.join("\n")
                );
            } else {
                assert!(
                    actuals[*contract_name].test_results[*test_name].status == TestStatus::Failure,
                    "Test {} did not fail as expected.\nLogs:\n{}",
                    test_name,
                    logs.join("\n")
                );
                assert_eq!(
                    actuals[*contract_name].test_results[*test_name].reason, *reason,
                    "Failure reason for test {test_name} did not match what we expected."
                );
            }

            if let Some(expected_logs) = expected_logs {
                assert_eq!(
                    logs,
                    expected_logs,
                    "Logs did not match for test {}.\nExpected:\n{}\n\nGot:\n{}",
                    test_name,
                    expected_logs.join("\n"),
                    logs.join("\n")
                );
            }

            if let Some(expected_warning_count) = expected_warning_count {
                assert_eq!(
                    warnings_count, expected_warning_count,
                    "Test {test_name} did not pass as expected. Expected:\n{expected_warning_count}Got:\n{warnings_count}"
                );
            }
        }
    }
}
