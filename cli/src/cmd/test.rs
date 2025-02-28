//! Test command

use crate::cmd::{build::BuildArgs, Cmd};
use ansi_term::Colour;
use clap::{AppSettings, Parser};
use ethers::solc::{ArtifactOutput, Project};
use evm_adapters::{evm_opts::EvmOpts, sputnik::helpers::vm};
use forge::{MultiContractRunnerBuilder, TestFilter};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Parser)]
pub struct Filter {
    #[clap(
        long = "match",
        short = 'm',
        help = "only run test methods matching regex (deprecated, see --match-test, --match-contract)"
    )]
    pattern: Option<regex::Regex>,

    #[clap(
        long = "match-test",
        help = "only run test methods matching regex",
        conflicts_with = "pattern"
    )]
    test_pattern: Option<regex::Regex>,

    #[clap(
        long = "no-match-test",
        help = "only run test methods not matching regex",
        conflicts_with = "pattern"
    )]
    test_pattern_inverse: Option<regex::Regex>,

    #[clap(
        long = "match-contract",
        help = "only run test methods in contracts matching regex",
        conflicts_with = "pattern"
    )]
    contract_pattern: Option<regex::Regex>,

    #[clap(
        long = "no-match-contract",
        help = "only run test methods in contracts not matching regex",
        conflicts_with = "pattern"
    )]
    contract_pattern_inverse: Option<regex::Regex>,
}

impl TestFilter for Filter {
    fn matches_test(&self, test_name: &str) -> bool {
        let mut ok = true;
        // Handle the deprecated option match
        if let Some(re) = &self.pattern {
            ok &= re.is_match(test_name);
        }
        if let Some(re) = &self.test_pattern {
            ok &= re.is_match(test_name);
        }
        if let Some(re) = &self.test_pattern_inverse {
            ok &= !re.is_match(test_name);
        }
        ok
    }

    fn matches_contract(&self, contract_name: &str) -> bool {
        let mut ok = true;
        if let Some(re) = &self.contract_pattern {
            ok &= re.is_match(contract_name);
        }
        if let Some(re) = &self.contract_pattern_inverse {
            ok &= !re.is_match(contract_name);
        }
        ok
    }
}

#[derive(Debug, Clone, Parser)]
// This is required to group Filter options in help output
#[clap(global_setting = AppSettings::DeriveDisplayOrder)]
pub struct TestArgs {
    #[clap(help = "print the test results in json format", long, short)]
    json: bool,

    #[clap(flatten)]
    evm_opts: EvmOpts,

    #[clap(flatten)]
    filter: Filter,

    #[clap(flatten)]
    opts: BuildArgs,

    #[clap(
        help = "if set to true, the process will exit with an exit code = 0, even if the tests fail",
        long,
        env = "FORGE_ALLOW_FAILURE"
    )]
    allow_failure: bool,
}

impl Cmd for TestArgs {
    type Output = TestOutcome;

    fn run(self) -> eyre::Result<Self::Output> {
        let TestArgs { opts, evm_opts, json, filter, allow_failure } = self;
        // Setup the fuzzer
        // TODO: Add CLI Options to modify the persistence
        let cfg = proptest::test_runner::Config { failure_persistence: None, ..Default::default() };
        let fuzzer = proptest::test_runner::TestRunner::new(cfg);

        // Set up the project
        let project = opts.project()?;

        // prepare the test builder
        let mut evm_cfg = crate::utils::sputnik_cfg(&opts.compiler.evm_version);
        evm_cfg.create_contract_limit = None;

        let builder = MultiContractRunnerBuilder::default()
            .fuzzer(fuzzer)
            .initial_balance(evm_opts.initial_balance)
            .evm_cfg(evm_cfg)
            .sender(evm_opts.sender);

        test(builder, project, evm_opts, filter, json, allow_failure)
    }
}

/// The result of a single test
#[derive(Debug, Clone)]
pub struct Test {
    /// The signature of the test
    pub signature: String,
    /// Result of the executed solidity test
    pub result: forge::TestResult,
}

impl Test {
    pub fn gas_used(&self) -> u64 {
        self.result.gas_used
    }
}

/// Represents the bundled results of all tests
pub struct TestOutcome {
    /// Whether failures are allowed
    allow_failure: bool,
    /// All test results `contract -> (test name -> TestResult)`
    pub results: BTreeMap<String, BTreeMap<String, forge::TestResult>>,
}

impl TestOutcome {
    fn new(
        results: BTreeMap<String, BTreeMap<String, forge::TestResult>>,
        allow_failure: bool,
    ) -> Self {
        Self { results, allow_failure }
    }

    /// Iterator over all succeeding tests and their names
    pub fn successes(&self) -> impl Iterator<Item = (&String, &forge::TestResult)> {
        self.tests().filter(|(_, t)| t.success)
    }

    /// Iterator over all failing tests and their names
    pub fn failures(&self) -> impl Iterator<Item = (&String, &forge::TestResult)> {
        self.tests().filter(|(_, t)| !t.success)
    }

    /// Iterator over all tests and their names
    pub fn tests(&self) -> impl Iterator<Item = (&String, &forge::TestResult)> {
        self.results.values().flat_map(|tests| tests.iter())
    }

    pub fn into_tests(self) -> impl Iterator<Item = Test> {
        self.results
            .into_values()
            .flat_map(|tests| tests.into_iter())
            .map(|(name, result)| Test { signature: name, result })
    }

    /// Checks if there are any failures and failures are disallowed
    pub fn ensure_ok(&self) -> eyre::Result<()> {
        if !self.allow_failure {
            let failures = self.failures().count();
            if failures > 0 {
                let successes = self.successes().count();
                eyre::bail!(
                    "Encountered a total of {} failing tests, {} tests succeeded",
                    failures,
                    successes
                );
            }
        }
        Ok(())
    }
}

/// Runs all the tests
fn test<A: ArtifactOutput + 'static>(
    builder: MultiContractRunnerBuilder,
    project: Project<A>,
    evm_opts: EvmOpts,
    filter: Filter,
    json: bool,
    allow_failure: bool,
) -> eyre::Result<TestOutcome> {
    let verbosity = evm_opts.verbosity;
    let mut runner = builder.build(project, evm_opts)?;

    let results = runner.test(&filter)?;

    if json {
        let res = serde_json::to_string(&results)?;
        println!("{}", res);
    } else {
        // Dapptools-style printing of test results
        for (i, (contract_name, tests)) in results.iter().enumerate() {
            if i > 0 {
                println!()
            }
            if !tests.is_empty() {
                let term = if tests.len() > 1 { "tests" } else { "test" };
                println!("Running {} {} for {}", tests.len(), term, contract_name);
            }

            for (name, result) in tests {
                let status = if result.success {
                    Colour::Green.paint("[PASS]")
                } else {
                    let txt = match (&result.reason, &result.counterexample) {
                        (Some(ref reason), Some(ref counterexample)) => {
                            format!(
                                "[FAIL. Reason: {}. Counterexample: {}]",
                                reason, counterexample
                            )
                        }
                        (None, Some(ref counterexample)) => {
                            format!("[FAIL. Counterexample: {}]", counterexample)
                        }
                        (Some(ref reason), None) => {
                            format!("[FAIL. Reason: {}]", reason)
                        }
                        (None, None) => "[FAIL]".to_string(),
                    };

                    Colour::Red.paint(txt)
                };

                // adds a linebreak only if there were any traces or logs, so that the
                // output does not look like 1 big block.
                let mut add_newline = false;
                println!("{} {} {}", status, name, result.kind.gas_used());
                if verbosity > 1 && !result.logs.is_empty() {
                    add_newline = true;
                    println!("Logs:");
                    for log in &result.logs {
                        println!("  {}", log);
                    }
                }

                if verbosity > 2 {
                    if let (Some(traces), Some(identified_contracts)) =
                        (&result.traces, &result.identified_contracts)
                    {
                        if !result.success && verbosity == 3 || verbosity > 3 {
                            // add a new line if any logs were printed & to separate them from
                            // the traces to be printed
                            if !result.logs.is_empty() {
                                println!();
                            }

                            let mut ident = identified_contracts.clone();
                            if verbosity > 4 || !result.success {
                                add_newline = true;
                                println!("Traces:");

                                // print setup calls as well
                                traces.iter().for_each(|trace| {
                                    trace.pretty_print(
                                        0,
                                        &runner.known_contracts,
                                        &mut ident,
                                        &vm(),
                                        "  ",
                                    );
                                });
                            } else if !traces.is_empty() {
                                add_newline = true;
                                println!("Traces:");
                                traces.last().expect("no last but not empty").pretty_print(
                                    0,
                                    &runner.known_contracts,
                                    &mut ident,
                                    &vm(),
                                    "  ",
                                );
                            }
                        }
                    }
                }

                if add_newline {
                    println!();
                }
            }
        }
    }

    Ok(TestOutcome::new(results, allow_failure))
}
