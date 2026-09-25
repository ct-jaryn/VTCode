#![allow(
    missing_docs,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use vtcode_core::tools::rate_limiter::{RateLimiter, RateLimiterConfig};

pub fn rate_limiter_benchmark(c: &mut Criterion) {
    let config = RateLimiterConfig { per_sec: 100, burst: 200 };

    let _rate_limiter_benchmark = c.bench_function("rate_limiter_acquire", |b| {
        let mut limiter = RateLimiter::new_with_config(config);
        b.iter(|| {
            // Benchmark token acquisition
            drop(black_box(limiter.acquire("tool_name")));
        })
    });
}

fn simulated_tool_outcome_clone(c: &mut Criterion) {
    // Simulate the ToolPipelineOutcome structure
    #[expect(
        dead_code,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    struct ExecutionStatus {
        output: Option<String>,
        stdout: Option<String>,
        modified_files: Vec<String>,
    }

    #[expect(
        dead_code,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    struct PipelineOutcome {
        status: ExecutionStatus,
        stdout: Option<String>,
        modified_files: Vec<String>,
    }

    let big_string = "x".repeat(10000); // 10KB
    let modified = vec!["file1.rs".to_string(), "file2.rs".to_string(), "file3.rs".to_string()];

    let _double_clone_benchmark = c.bench_function("outcome_double_clone", |b| {
        b.iter(|| {
            // Old way: double clone
            let output = Some(big_string.clone());
            let stdout = Some(big_string.clone());
            let mod_files = modified.clone();

            let outcome = PipelineOutcome {
                status: ExecutionStatus {
                    output: output.clone(),
                    stdout: stdout.clone(),
                    modified_files: mod_files.clone(),
                },
                stdout,
                modified_files: mod_files,
            };
            drop(black_box(outcome));
        })
    });

    let _single_clone_benchmark = c.bench_function("outcome_single_clone", |b| {
        b.iter(|| {
            // New way: single clone
            let output = Some(big_string.clone());
            let stdout = Some(big_string.clone());
            let mod_files = modified.clone();

            let stdout_copy = stdout.clone();
            let mod_files_copy = mod_files.clone();

            let outcome = PipelineOutcome {
                status: ExecutionStatus { output, stdout, modified_files: mod_files },
                stdout: stdout_copy,
                modified_files: mod_files_copy,
            };
            drop(black_box(outcome));
        })
    });
}

/// Heterogeneous payload sizes for the same clone patterns above: 1KB covers
/// chat-size outputs, 100KB covers spool-size outputs. The 10KB baseline stays
/// in `simulated_tool_outcome_clone` under its original names so history is
/// comparable. Follows the `bench_with_input` precedent in
/// `vtcode-ui/benches/markdown_render.rs`.
fn simulated_tool_outcome_clone_sizes(c: &mut Criterion) {
    #[expect(
        dead_code,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    struct ExecutionStatus {
        output: Option<String>,
        stdout: Option<String>,
        modified_files: Vec<String>,
    }

    #[expect(
        dead_code,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    struct PipelineOutcome {
        status: ExecutionStatus,
        stdout: Option<String>,
        modified_files: Vec<String>,
    }

    let mut group = c.benchmark_group("outcome_clone_sizes");
    for (tag, len) in [("1KB", 1000usize), ("100KB", 100_000usize)] {
        let payload = "x".repeat(len);
        let modified = vec!["file1.rs".to_string(), "file2.rs".to_string(), "file3.rs".to_string()];
        let _throughput = group.throughput(Throughput::Bytes(payload.len() as u64));
        let _double_clone_sized =
            group.bench_with_input(BenchmarkId::new("double_clone", tag), &payload, |b, payload| {
                b.iter(|| {
                    let output = Some(payload.clone());
                    let stdout = Some(payload.clone());
                    let mod_files = modified.clone();

                    let outcome = PipelineOutcome {
                        status: ExecutionStatus {
                            output: output.clone(),
                            stdout: stdout.clone(),
                            modified_files: mod_files.clone(),
                        },
                        stdout,
                        modified_files: mod_files,
                    };
                    drop(black_box(outcome));
                });
            });
        let _single_clone_sized =
            group.bench_with_input(BenchmarkId::new("single_clone", tag), &payload, |b, payload| {
                b.iter(|| {
                    let output = Some(payload.clone());
                    let stdout = Some(payload.clone());
                    let mod_files = modified.clone();

                    let stdout_copy = stdout.clone();
                    let mod_files_copy = mod_files.clone();

                    let outcome = PipelineOutcome {
                        status: ExecutionStatus { output, stdout, modified_files: mod_files },
                        stdout: stdout_copy,
                        modified_files: mod_files_copy,
                    };
                    drop(black_box(outcome));
                });
            });
    }
    group.finish();
}

criterion_group!(benches, rate_limiter_benchmark, simulated_tool_outcome_clone, simulated_tool_outcome_clone_sizes);
criterion_main!(benches);
