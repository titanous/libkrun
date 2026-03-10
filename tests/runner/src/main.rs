use anyhow::Context;
use clap::Parser;
use nix::sys::resource::{getrlimit, setrlimit, Resource};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempdir::TempDir;
use test_cases::{test_cases, Test, TestSetup};

#[derive(Debug)]
struct TestResult {
    name: String,
    passed: bool,
    duration: Duration,
    log_path: PathBuf,
}

fn get_test(name: &str) -> anyhow::Result<Box<dyn Test>> {
    let tests = test_cases();
    tests
        .into_iter()
        .find(|t| t.name() == name)
        .with_context(|| format!("No such test: {name}"))
        .map(|t| t.test)
}

fn start_vm(test_setup: TestSetup) -> anyhow::Result<()> {
    // Raise soft fd limit up to the hard limit
    let (_soft_limit, hard_limit) =
        getrlimit(Resource::RLIMIT_NOFILE).context("getrlimit RLIMIT_NOFILE")?;
    setrlimit(Resource::RLIMIT_NOFILE, hard_limit, hard_limit)
        .context("setrlimit RLIMIT_NOFILE")?;

    let test = get_test(&test_setup.test_case)?;
    test.start_vm(test_setup.clone())
        .with_context(|| format!("testcase: {test_setup:?}"))?;
    Ok(())
}

fn run_single_test(
    test_case: &str,
    base_dir: &Path,
    keep_all: bool,
    max_name_len: usize,
    print_lock: Option<&Mutex<()>>,
) -> anyhow::Result<TestResult> {
    let executable = env::current_exe().context("Failed to detect current executable")?;
    let test_dir = base_dir.join(test_case);
    fs::create_dir(&test_dir).context("Failed to create test directory")?;

    let log_path = test_dir.join("log.txt");
    let log_file = File::create(&log_path).context("Failed to create log file")?;

    let child = Command::new(&executable)
        .arg("start-vm")
        .arg("--test-case")
        .arg(test_case)
        .arg("--tmp-dir")
        .arg(&test_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log_file)
        .spawn()
        .context("Failed to start subprocess for test")?;

    let start = Instant::now();
    let _ = get_test(test_case)?;
    let result = catch_unwind(|| {
        let test = get_test(test_case).unwrap();
        test.check(child);
    });
    let duration = start.elapsed();

    let passed = result.is_ok();
    let status = if passed { "OK" } else { "FAIL" };
    {
        let _guard = print_lock.map(|m| m.lock().unwrap());
        eprintln!(
            "[{test_case}] {:.<width$} {status} ({:.1}s)",
            "",
            duration.as_secs_f64(),
            width = max_name_len - test_case.len() + 3
        );
    }
    if passed && !keep_all {
        let _ = fs::remove_dir_all(&test_dir);
    }

    Ok(TestResult {
        name: test_case.to_string(),
        passed,
        duration,
        log_path,
    })
}

fn write_github_summary(
    results: &[TestResult],
    num_ok: usize,
    num_tests: usize,
) -> anyhow::Result<()> {
    let summary_path = env::var("GITHUB_STEP_SUMMARY")
        .context("GITHUB_STEP_SUMMARY environment variable not set")?;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&summary_path)
        .context("Failed to open GITHUB_STEP_SUMMARY")?;

    let all_passed = num_ok == num_tests;
    let status = if all_passed { "✅" } else { "❌" };

    writeln!(
        file,
        "## {status} Integration Tests ({num_ok}/{num_tests} passed)\n"
    )?;

    for result in results {
        let icon = if result.passed { "✅" } else { "❌" };
        let log_content = fs::read_to_string(&result.log_path).unwrap_or_default();

        writeln!(file, "<details>")?;
        writeln!(file, "<summary>{icon} {}</summary>\n", result.name)?;
        writeln!(file, "```")?;
        // Limit log size to avoid huge summaries (2 MiB limit)
        const MAX_LOG_SIZE: usize = 2 * 1024 * 1024;
        let truncated = if log_content.len() > MAX_LOG_SIZE {
            format!(
                "... (truncated, showing last 1 MiB) ...\n{}",
                &log_content[log_content.len() - MAX_LOG_SIZE..]
            )
        } else {
            log_content
        };
        writeln!(file, "{truncated}")?;
        writeln!(file, "```")?;
        writeln!(file, "</details>\n")?;
    }

    Ok(())
}

fn run_tests(
    test_case: &str,
    base_dir: Option<PathBuf>,
    keep_all: bool,
    github_summary: bool,
    jobs: usize,
) -> anyhow::Result<()> {
    // Create the base directory - either use provided path or create a temp one
    let base_dir = match base_dir {
        Some(path) => {
            fs::create_dir_all(&path).context("Failed to create base directory")?;
            path
        }
        None => TempDir::new("libkrun-tests")
            .context("Failed to create temp base directory")?
            .into_path(),
    };

    let wall_start = Instant::now();
    let results: Vec<TestResult>;

    if test_case == "all" {
        let all_tests = test_cases();
        let max_name_len = all_tests.iter().map(|t| t.name.len()).max().unwrap_or(0);
        let test_names: Vec<&'static str> = all_tests.into_iter().map(|t| t.name).collect();

        if jobs <= 1 {
            // Sequential (original behavior)
            let mut seq_results = Vec::new();
            for name in test_names {
                seq_results.push(
                    run_single_test(name, &base_dir, keep_all, max_name_len, None)
                        .context(name)?,
                );
            }
            results = seq_results;
        } else {
            // Parallel execution with thread pool
            let base_dir = Arc::new(base_dir.clone());
            let print_lock = Arc::new(Mutex::new(()));
            let work: Arc<Mutex<std::vec::IntoIter<&'static str>>> =
                Arc::new(Mutex::new(test_names.into_iter()));
            let collected: Arc<Mutex<Vec<TestResult>>> = Arc::new(Mutex::new(Vec::new()));

            std::thread::scope(|s| {
                let mut handles = Vec::new();
                for _ in 0..jobs {
                    let work = Arc::clone(&work);
                    let collected = Arc::clone(&collected);
                    let base_dir = Arc::clone(&base_dir);
                    let print_lock = Arc::clone(&print_lock);
                    handles.push(s.spawn(move || {
                        loop {
                            let name = {
                                let mut iter = work.lock().unwrap();
                                iter.next()
                            };
                            let Some(name) = name else { break };
                            match run_single_test(
                                name,
                                &base_dir,
                                keep_all,
                                max_name_len,
                                Some(&print_lock),
                            ) {
                                Ok(r) => collected.lock().unwrap().push(r),
                                Err(e) => {
                                    let _guard = print_lock.lock().unwrap();
                                    eprintln!("[{name}] ERROR: {e:#}");
                                    collected.lock().unwrap().push(TestResult {
                                        name: name.to_string(),
                                        passed: false,
                                        duration: Duration::ZERO,
                                        log_path: base_dir.join(name).join("log.txt"),
                                    });
                                }
                            }
                        }
                    }));
                }
                for h in handles {
                    h.join().unwrap();
                }
            });

            results = Arc::try_unwrap(collected).unwrap().into_inner().unwrap();
        }
    } else {
        let max_name_len = test_case.len();
        results = vec![
            run_single_test(test_case, &base_dir, keep_all, max_name_len, None)
                .context(test_case.to_string())?,
        ];
    }

    let wall_time = wall_start.elapsed();
    let num_tests = results.len();
    let num_ok = results.iter().filter(|r| r.passed).count();

    // Write GitHub Actions summary if requested
    if github_summary {
        write_github_summary(&results, num_ok, num_tests)?;
    }

    // Print timing summary sorted slowest-first.
    let total: Duration = results.iter().map(|r| r.duration).sum();
    let mut by_time: Vec<_> = results.iter().collect();
    by_time.sort_by(|a, b| b.duration.cmp(&a.duration));
    eprintln!("\n--- timing (slowest first) ---");
    for r in &by_time {
        let tag = if r.passed { " " } else { "!" };
        eprintln!(
            "{tag} {:.1}s  {}",
            r.duration.as_secs_f64(),
            r.name
        );
    }
    eprintln!("  {:.1}s  sum of test times", total.as_secs_f64());
    if jobs > 1 {
        eprintln!(
            "  {:.1}s  wall time ({jobs} jobs, {:.1}x speedup)",
            wall_time.as_secs_f64(),
            total.as_secs_f64() / wall_time.as_secs_f64(),
        );
    }

    let num_failures = num_tests - num_ok;
    if num_failures > 0 {
        eprintln!("(See test artifacts at: {})", base_dir.display());
        println!("\nFAIL (PASSED {num_ok}/{num_tests})");
        anyhow::bail!("")
    } else {
        if keep_all {
            eprintln!("(See test artifacts at: {})", base_dir.display());
        }
        eprintln!("\nOK ({num_ok}/{num_tests} passed)");
    }

    Ok(())
}

#[derive(clap::Subcommand, Clone, Debug)]
enum CliCommand {
    Test {
        /// Specify which test to run or "all"
        #[arg(long, default_value = "all")]
        test_case: String,
        /// Base directory for test artifacts
        #[arg(long)]
        base_dir: Option<PathBuf>,
        /// Keep all test artifacts even on success
        #[arg(long)]
        keep_all: bool,
        /// Write test results to GitHub Actions job summary ($GITHUB_STEP_SUMMARY)
        #[arg(long)]
        github_summary: bool,
        /// Number of tests to run in parallel (default: 1 = sequential)
        #[arg(short = 'j', long = "jobs", default_value = "1")]
        jobs: usize,
    },
    StartVm {
        #[arg(long)]
        test_case: String,
        #[arg(long)]
        tmp_dir: PathBuf,
    },
}

impl Default for CliCommand {
    fn default() -> Self {
        Self::Test {
            test_case: "all".to_string(),
            base_dir: None,
            keep_all: false,
            github_summary: false,
            jobs: 1,
        }
    }
}

#[derive(clap::Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

fn main() -> anyhow::Result<()> {
    let _ = env_logger::try_init();
    let cli = Cli::parse();
    let command = cli.command.unwrap_or_default();

    match command {
        CliCommand::StartVm { test_case, tmp_dir } => start_vm(TestSetup { test_case, tmp_dir }),
        CliCommand::Test {
            test_case,
            base_dir,
            keep_all,
            github_summary,
            jobs,
        } => run_tests(&test_case, base_dir, keep_all, github_summary, jobs),
    }
}
