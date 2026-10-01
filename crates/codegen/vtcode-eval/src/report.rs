use crate::trace_analyzer::HarnessTraceSummary;
use crate::{EvalMetric, task::EvalCategory};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct TaskReport {
    task_id: String,
    pub(crate) category: String,
    pub(crate) metric: EvalMetric,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuiteReport {
    pub(crate) suite_id: String,
    pub(crate) suite_name: String,
    pub(crate) task_reports: Vec<TaskReport>,
    pub(crate) aggregate: EvalMetric,
    pub(crate) capability_metrics: EvalMetric,
    pub(crate) regression_metrics: EvalMetric,
    /// Sum of known per-attempt costs. Unknown-pricing attempts are counted
    /// separately instead of being treated as zero-cost successes.
    pub(crate) cost_usd: Option<f64>,
    pub(crate) unpriced_runs: u32,
    pub(crate) duration_secs: f64,
    /// Aggregate privacy-preserving trace facts joined by task and attempt.
    pub(crate) trace_summary: Option<HarnessTraceSummary>,
    /// Mean cost over priced attempts only. `None` when every attempt is unpriced.
    pub(crate) mean_cost_per_attempt: Option<f64>,
    /// Total known cost divided by successful attempts. `None` when any
    /// attempt is unpriced (unknown cost is not free) or nothing passed.
    pub(crate) cost_per_solve: Option<f64>,
    /// Mean gross tokens (input + output) over attempts that carry a trace.
    pub(crate) mean_tokens_per_attempt: Option<f64>,
    /// Mean model turns over attempts that carry a trace.
    pub(crate) mean_turns_per_attempt: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub(crate) generated_at: String,
    pub(crate) suites: Vec<SuiteReport>,
}

impl EvalReport {
    /// Create a report from generated-at metadata and suite reports.
    pub fn new(generated_at: impl Into<String>, suites: Vec<SuiteReport>) -> Self {
        Self { generated_at: generated_at.into(), suites }
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Eval Report\n\n");
        for s in &self.suites {
            out.push_str(&format!("## {}\n\n", s.suite_name));
            out.push_str(&format!(
                "- Aggregate: pass@k={:.1}%; pass^k={:.1}%\n",
                s.aggregate.pass_at_k * 100.0,
                s.aggregate.pass_power_k * 100.0
            ));
            if let Some(cost) = s.cost_usd {
                out.push_str(&format!("- Cost (known): ${cost:.6}; unpriced attempts: {}\n", s.unpriced_runs));
            } else if s.unpriced_runs > 0 {
                out.push_str(&format!("- Cost (known): unavailable; unpriced attempts: {}\n", s.unpriced_runs));
            }
            if s.mean_cost_per_attempt.is_some()
                || s.cost_per_solve.is_some()
                || s.mean_tokens_per_attempt.is_some()
                || s.mean_turns_per_attempt.is_some()
            {
                let mut parts = Vec::new();
                if let Some(cost_per_solve) = s.cost_per_solve {
                    parts.push(format!("cost/solve ${cost_per_solve:.4}"));
                }
                if let Some(mean_cost) = s.mean_cost_per_attempt {
                    parts.push(format!("${mean_cost:.4} per priced attempt"));
                }
                if let Some(mean_tokens) = s.mean_tokens_per_attempt {
                    parts.push(format!("{mean_tokens:.0} tokens"));
                }
                if let Some(mean_turns) = s.mean_turns_per_attempt {
                    parts.push(format!("{mean_turns:.1} turns"));
                }
                out.push_str(&format!("- Efficiency: {}\n", parts.join(" · ")));
            }
            if let Some(trace) = &s.trace_summary {
                if let Some(mean_ms) = trace.latency.mean_ms {
                    out.push_str(&format!(
                        "- Trace: {} turns, {} tool calls, mean latency {:.1} ms\n",
                        trace.turns, trace.tool_calls, mean_ms
                    ));
                } else {
                    out.push_str(&format!("- Trace: {} turns, {} tool calls\n", trace.turns, trace.tool_calls));
                }
            }
            out.push_str("| Task | Category | pass@k | pass^k | passed/total |\n");
            out.push_str("|------|----------|--------|--------|-------------|\n");
            for t in &s.task_reports {
                out.push_str(&format!(
                    "| {} | {} | {:.1}% | {:.1}% | {}/{} |\n",
                    t.task_id,
                    t.category.as_str(),
                    t.metric.pass_at_k * 100.0,
                    t.metric.pass_power_k * 100.0,
                    t.metric.passed_runs,
                    t.metric.total_runs
                ));
            }
            out.push('\n');
        }
        out
    }
}

pub fn build_task_report(task_id: &str, _name: &str, category: EvalCategory, metric: EvalMetric) -> TaskReport {
    TaskReport {
        task_id: task_id.into(),
        category: category.label().into(),
        metric,
    }
}

/// Cost-efficiency aggregates for a suite (HarnessTax-style frontier metrics).
///
/// Unknown pricing is never treated as zero: any unpriced attempt makes
/// `cost_per_solve` `None` so the report cannot claim a complete cost basis.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostEfficiency {
    pub mean_cost_per_attempt: Option<f64>,
    pub cost_per_solve: Option<f64>,
    pub mean_tokens_per_attempt: Option<f64>,
    pub mean_turns_per_attempt: Option<f64>,
}

/// Finite, non-negative attempt cost. Unpriced is not free.
pub fn priced_cost(cost_usd: Option<f64>) -> Option<f64> {
    cost_usd.filter(|value| value.is_finite() && *value >= 0.0)
}

impl CostEfficiency {
    /// Compute efficiency metrics from attempt results.
    ///
    /// `cost_usd` / `trace_summary` come from each `EvalRunResult`. Token and
    /// turn means use only attempts that carry a trace summary.
    pub fn from_runs<'a>(results: impl IntoIterator<Item = &'a crate::task::EvalRunResult>) -> Self {
        let mut total_cost = 0.0_f64;
        let mut known_cost_runs = 0_u32;
        let mut unpriced_runs = 0_u32;
        let mut passed_runs = 0_u32;
        let mut token_sum = 0.0_f64;
        let mut token_count = 0_u32;
        let mut turn_sum = 0.0_f64;
        let mut turn_count = 0_u32;

        for result in results {
            if result.outcome == crate::task::RunOutcome::Pass {
                passed_runs = passed_runs.saturating_add(1);
            }
            match priced_cost(result.cost_usd) {
                Some(cost) => {
                    total_cost += cost;
                    known_cost_runs = known_cost_runs.saturating_add(1);
                }
                None => {
                    unpriced_runs = unpriced_runs.saturating_add(1);
                }
            }
            if let Some(summary) = &result.trace_summary {
                let gross = (summary
                    .token_usage
                    .input_tokens
                    .saturating_add(summary.token_usage.output_tokens)) as f64;
                token_sum += gross;
                token_count = token_count.saturating_add(1);
                turn_sum += summary.turns as f64;
                turn_count = turn_count.saturating_add(1);
            }
        }

        let mean_cost_per_attempt = (known_cost_runs > 0).then(|| total_cost / f64::from(known_cost_runs));
        let cost_per_solve =
            (unpriced_runs == 0 && passed_runs > 0 && known_cost_runs > 0).then(|| total_cost / f64::from(passed_runs));
        let mean_tokens_per_attempt = (token_count > 0).then(|| token_sum / f64::from(token_count));
        let mean_turns_per_attempt = (turn_count > 0).then(|| turn_sum / f64::from(turn_count));

        Self {
            mean_cost_per_attempt,
            cost_per_solve,
            mean_tokens_per_attempt,
            mean_turns_per_attempt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric::EvalMetric;
    use crate::task::EvalCategory;

    #[test]
    fn to_markdown_renders_tasks_and_aggregate() {
        let report = EvalReport {
            generated_at: "2026-01-01".into(),
            suites: vec![SuiteReport {
                suite_id: "s1".into(),
                suite_name: "demo".into(),
                task_reports: vec![TaskReport {
                    task_id: "t1".into(),
                    category: "Capability".into(),
                    metric: EvalMetric {
                        pass_at_k: 0.5,
                        pass_power_k: 0.25,
                        pass_all_k: 0.0,
                        k: 1,
                        total_runs: 2,
                        passed_runs: 1,
                        task_id: "t1".into(),
                    },
                }],
                aggregate: EvalMetric {
                    pass_at_k: 0.5,
                    pass_power_k: 0.25,
                    pass_all_k: 0.0,
                    k: 1,
                    total_runs: 2,
                    passed_runs: 1,
                    task_id: "aggregate".into(),
                },
                capability_metrics: EvalMetric {
                    pass_at_k: 0.5,
                    pass_power_k: 0.25,
                    pass_all_k: 0.0,
                    k: 1,
                    total_runs: 2,
                    passed_runs: 1,
                    task_id: "cap".into(),
                },
                regression_metrics: EvalMetric {
                    pass_at_k: 0.0,
                    pass_power_k: 0.0,
                    pass_all_k: 0.0,
                    k: 0,
                    total_runs: 0,
                    passed_runs: 0,
                    task_id: "reg".into(),
                },
                cost_usd: Some(0.0),
                unpriced_runs: 0,
                duration_secs: 0.0,
                trace_summary: None,
                mean_cost_per_attempt: None,
                cost_per_solve: None,
                mean_tokens_per_attempt: None,
                mean_turns_per_attempt: None,
            }],
        };
        let md = report.to_markdown();
        assert!(md.contains("# Eval Report"));
        assert!(md.contains("demo"));
        assert!(md.contains("t1"));
        assert!(md.contains("Capability"));
        assert!(md.contains("pass^k"));
    }

    #[test]
    fn build_task_report_maps_category() {
        let tr = build_task_report(
            "t1",
            "name",
            EvalCategory::Regression,
            EvalMetric {
                pass_at_k: 1.0,
                pass_power_k: 1.0,
                pass_all_k: 1.0,
                k: 1,
                total_runs: 1,
                passed_runs: 1,
                task_id: "t1".into(),
            },
        );
        assert_eq!(tr.task_id, "t1");
        assert_eq!(tr.category, "Regression");
    }

    fn run(outcome: crate::task::RunOutcome, cost: Option<f64>, turns: u64, tokens: u64) -> crate::task::EvalRunResult {
        use crate::trace_analyzer::{HarnessTraceSummary, TokenUsage};
        crate::task::EvalRunResult {
            task_id: "t1".into(),
            outcome,
            error_message: None,
            duration_secs: 1.0,
            attempt: 1,
            cost_usd: cost,
            transcript_path: None,
            trace_summary: Some(HarnessTraceSummary {
                turns,
                token_usage: TokenUsage {
                    input_tokens: tokens / 2,
                    output_tokens: tokens - tokens / 2,
                    ..TokenUsage::default()
                },
                ..HarnessTraceSummary::default()
            }),
        }
    }

    #[test]
    fn cost_efficiency_uses_total_cost_over_passed_attempts() {
        let results = vec![
            run(crate::task::RunOutcome::Pass, Some(0.02), 10, 1_000),
            run(crate::task::RunOutcome::Fail, Some(0.04), 20, 3_000),
        ];
        let eff = CostEfficiency::from_runs(&results);
        assert_eq!(eff.cost_per_solve, Some(0.06));
        assert_eq!(eff.mean_cost_per_attempt, Some(0.03));
        assert_eq!(eff.mean_tokens_per_attempt, Some(2_000.0));
        assert_eq!(eff.mean_turns_per_attempt, Some(15.0));
    }

    #[test]
    fn cost_per_solve_is_none_when_any_attempt_is_unpriced() {
        let results = vec![
            run(crate::task::RunOutcome::Pass, Some(0.02), 5, 100),
            run(crate::task::RunOutcome::Pass, None, 5, 100),
        ];
        let eff = CostEfficiency::from_runs(&results);
        assert_eq!(eff.cost_per_solve, None);
        // Mean cost still reflects only the priced attempts.
        assert_eq!(eff.mean_cost_per_attempt, Some(0.02));
    }

    #[test]
    fn cost_per_solve_is_none_when_nothing_passed() {
        let results = vec![run(crate::task::RunOutcome::Fail, Some(0.10), 3, 50)];
        let eff = CostEfficiency::from_runs(&results);
        assert_eq!(eff.cost_per_solve, None);
        assert_eq!(eff.mean_cost_per_attempt, Some(0.10));
    }

    #[test]
    fn token_and_turn_means_are_none_without_traces() {
        let mut result = run(crate::task::RunOutcome::Pass, Some(0.01), 1, 1);
        result.trace_summary = None;
        let eff = CostEfficiency::from_runs(std::slice::from_ref(&result));
        assert_eq!(eff.mean_tokens_per_attempt, None);
        assert_eq!(eff.mean_turns_per_attempt, None);
    }

    #[test]
    fn markdown_renders_efficiency_line() {
        let report = EvalReport {
            generated_at: "2026-01-01".into(),
            suites: vec![SuiteReport {
                suite_id: "s1".into(),
                suite_name: "demo".into(),
                task_reports: Vec::new(),
                aggregate: EvalMetric {
                    pass_at_k: 1.0,
                    pass_power_k: 1.0,
                    pass_all_k: 1.0,
                    k: 1,
                    total_runs: 1,
                    passed_runs: 1,
                    task_id: "aggregate".into(),
                },
                capability_metrics: EvalMetric {
                    pass_at_k: 1.0,
                    pass_power_k: 1.0,
                    pass_all_k: 1.0,
                    k: 1,
                    total_runs: 1,
                    passed_runs: 1,
                    task_id: "cap".into(),
                },
                regression_metrics: EvalMetric {
                    pass_at_k: 0.0,
                    pass_power_k: 0.0,
                    pass_all_k: 0.0,
                    k: 0,
                    total_runs: 0,
                    passed_runs: 0,
                    task_id: "reg".into(),
                },
                cost_usd: Some(0.05),
                unpriced_runs: 0,
                duration_secs: 1.0,
                trace_summary: None,
                mean_cost_per_attempt: Some(0.05),
                cost_per_solve: Some(0.05),
                mean_tokens_per_attempt: Some(12_000.0),
                mean_turns_per_attempt: Some(8.5),
            }],
        };
        let md = report.to_markdown();
        assert!(
            md.contains("- Efficiency: cost/solve $0.0500 · $0.0500 per priced attempt · 12000 tokens · 8.5 turns"),
            "missing efficiency line: {md}"
        );
    }
}
