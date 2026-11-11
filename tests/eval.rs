//! Memory quality evaluation (#35): scripted multi-session scenarios, scored.
//!
//! One-shot tests pin that each mechanism works. They do not say whether
//! memory, used over many sessions by more than one writer, gets an agent the
//! right thing: whether a superseded fact loses to its replacement, whether an
//! inference is outranked by what the user actually said, whether an unrelated
//! question gets nothing rather than the least-irrelevant memory, how many
//! operations it takes to repair a wrong memory, and whether a poisoned record
//! ever reaches the context. That is what this measures.
//!
//! Each scenario under `tests/eval/scenarios/` is a JSON script of steps —
//! saves by named writers, status changes, tampering, probes — run against the
//! real [`Store`] (signing, verification, ranking, lifecycle) over the shared
//! in-process fake VTA. Probes are scored for precision, recall and reciprocal
//! rank against hand-labelled relevant keys, and carry hard checks (`top`,
//! `absent`, `expectEmpty`, `refused`, …) that fail the run. A probe marked
//! `soft` is scored and reported but does not fail it: that is how a known gap
//! is kept visible without blocking every change.
//!
//! The shape borrows from LongMemEval where it applies — knowledge updates
//! (staleness), abstention (minimisation), temporal validity (expiry) — and
//! adds what a signed, multi-writer store needs: contradiction between writers
//! and bases, repairability, and poisoning.
//!
//! ```text
//! cargo test --test eval                 # every scenario; markdown report
//! cargo test --test eval -- staleness    # scenarios whose name or dimension matches
//! EVAL_REPORT_JSON=out.json cargo test --test eval
//! ```
//!
//! With `EVAL_SUMMARY=1` on GitHub Actions the report is also appended to the
//! job summary. See
//! `tests/eval/README.md` for the scenario format.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use vta_agent_memory::fence::{Fence, Provenance};
use vta_agent_memory::lifecycle::Status;
use vta_agent_memory::provenance::{Basis, Evidence};
use vta_agent_memory::record::{MemoryKey, MemoryRecord, MemoryType};
use vta_agent_memory::store::{SaveOutcome, Store};

const CONTEXT: &str = "eval";

// ---- scenario format --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Dimension {
    Precision,
    Staleness,
    Contradiction,
    Minimisation,
    Repairability,
    Poisoning,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scenario {
    name: String,
    dimension: Dimension,
    description: String,
    /// Writers on the context; the first is the one that recalls. Default one,
    /// `agent`.
    #[serde(default)]
    writers: Vec<String>,
    /// For repairability: the most `repair`-flagged operations the fix may
    /// take.
    #[serde(default)]
    max_repair_ops: Option<usize>,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum Step {
    Save(SaveStep),
    Status(StatusStep),
    Forget(ForgetStep),
    /// Overwrite fields of a stored record's JSON, as a writer with raw access
    /// could — after signing.
    Tamper(TamperStep),
    /// Store a raw value directly.
    Plant(PlantStep),
    /// Copy one stored value, signature and all, to another key.
    Copy(CopyStep),
    Probe(Probe),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveStep {
    #[serde(default, rename = "as")]
    writer: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: String,
    #[serde(default)]
    body: String,
    #[serde(default = "declared")]
    basis: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    evidence: Vec<Evidence>,
    #[serde(default)]
    valid_until: Option<String>,
    #[serde(default)]
    review_after: Option<String>,
    #[serde(default)]
    repair: bool,
}

fn declared() -> String {
    "declared".into()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StatusStep {
    #[serde(default, rename = "as")]
    writer: Option<String>,
    key: String,
    status: Status,
    reason: String,
    #[serde(default)]
    repair: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ForgetStep {
    key: String,
    #[serde(default)]
    repair: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TamperStep {
    key: String,
    #[serde(default)]
    set: Map<String, Value>,
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlantStep {
    key: String,
    /// A string is stored as is; anything else as its JSON.
    value: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyStep {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Probe {
    label: String,
    #[serde(default)]
    query: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Hand-labelled: the keys that answer this query.
    #[serde(default)]
    relevant: Vec<String>,
    /// Must be ranked first.
    #[serde(default)]
    top: Option<String>,
    /// Must not be returned.
    #[serde(default)]
    absent: Vec<String>,
    /// Must return nothing (abstention).
    #[serde(default)]
    expect_empty: bool,
    /// Must be reported as refused.
    #[serde(default)]
    refused: Vec<String>,
    /// Must be returned flagged `stale`.
    #[serde(default)]
    stale: Vec<String>,
    /// Key → substring its returned description must contain.
    #[serde(default)]
    describe: BTreeMap<String, String>,
    /// Render the results inside a fence and require exactly one pair of
    /// delimiters — stored text must not be able to forge one.
    #[serde(default)]
    fence: bool,
    /// Scored and reported, but a miss does not fail the run: a known gap.
    #[serde(default)]
    soft: bool,
}

fn default_limit() -> usize {
    8
}

// ---- running ----------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeResult {
    label: String,
    soft: bool,
    returned: Vec<String>,
    /// `None` where the metric does not apply (no labelled relevant keys).
    precision: Option<f64>,
    recall: Option<f64>,
    reciprocal_rank: Option<f64>,
    failures: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioResult {
    name: String,
    dimension: Dimension,
    description: String,
    repair_ops: usize,
    probes: Vec<ProbeResult>,
    /// Failures outside any probe: a step that did not apply, a repair budget
    /// exceeded.
    errors: Vec<String>,
}

impl ScenarioResult {
    fn hard_failures(&self) -> usize {
        self.errors.len()
            + self
                .probes
                .iter()
                .filter(|p| !p.soft)
                .map(|p| p.failures.len())
                .sum::<usize>()
    }
}

struct Run {
    vta: Arc<FakeMemoryKeyspace>,
    stores: BTreeMap<String, Store>,
    reader: String,
}

impl Run {
    fn new(writers: &[String]) -> Self {
        let vta = Arc::new(FakeMemoryKeyspace::default());
        let writers = if writers.is_empty() {
            vec!["agent".to_string()]
        } else {
            writers.to_vec()
        };
        let stores = writers
            .iter()
            .map(|w| (w.clone(), store_on(&vta, CONTEXT)))
            .collect();
        Self {
            vta,
            stores,
            reader: writers[0].clone(),
        }
    }

    fn store(&self, writer: &Option<String>) -> Result<&Store, String> {
        let w = writer.as_deref().unwrap_or(&self.reader);
        self.stores
            .get(w)
            .ok_or_else(|| format!("unknown writer `{w}`"))
    }

    fn raw(&self, key: &str) -> Result<String, String> {
        self.vta
            .entries
            .lock()
            .unwrap()
            .get(&(CONTEXT.to_string(), key.to_string()))
            .map(|(v, _)| v.clone())
            .ok_or_else(|| format!("no stored value at `{key}`"))
    }

    async fn step(&self, step: &Step) -> Result<(), String> {
        match step {
            Step::Save(s) => {
                let kind = MemoryType::parse(&s.kind).ok_or("unknown type")?;
                let basis = Basis::parse(&s.basis).ok_or("unknown basis")?;
                let mut r = MemoryRecord::new(kind, &s.name, &s.description, &s.body, vec![])
                    .with_provenance(basis, s.confidence, s.evidence.clone());
                r.valid_until = s.valid_until.clone();
                r.review_after = s.review_after.clone();
                r.check_limits()?;
                match self
                    .store(&s.writer)?
                    .save_versioned(&r, None)
                    .await
                    .map_err(|e| format!("{e:#}"))?
                {
                    SaveOutcome::Saved { .. } => Ok(()),
                    SaveOutcome::Conflict { conflict, .. } => Err(format!(
                        "save of `{}` conflicted: {}",
                        s.name, conflict.reason
                    )),
                }
            }
            Step::Status(s) => {
                let key = MemoryKey::parse(&s.key).map_err(|e| e.to_string())?;
                self.store(&s.writer)?
                    .set_status(&key, s.status, &s.reason, None)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            }
            Step::Forget(s) => {
                let key = MemoryKey::parse(&s.key).map_err(|e| e.to_string())?;
                self.store(&None)?
                    .forget(&key)
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
            Step::Tamper(t) => {
                let mut v: Value =
                    serde_json::from_str(&self.raw(&t.key)?).map_err(|e| e.to_string())?;
                let obj = v.as_object_mut().ok_or("not a JSON record")?;
                for (k, val) in &t.set {
                    obj.insert(k.clone(), val.clone());
                }
                for k in &t.remove {
                    obj.remove(k);
                }
                self.vta.plant(CONTEXT, &t.key, v.to_string());
                Ok(())
            }
            Step::Plant(p) => {
                let value = match &p.value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                self.vta.plant(CONTEXT, &p.key, value);
                Ok(())
            }
            Step::Copy(c) => {
                let value = self.raw(&c.from)?;
                self.vta.plant(CONTEXT, &c.to, value);
                Ok(())
            }
            Step::Probe(_) => unreachable!("probes are run by `probe`"),
        }
    }

    async fn probe(&self, p: &Probe) -> ProbeResult {
        let mut failures = Vec::new();
        let store = &self.stores[&self.reader];
        let kind = p.kind.as_deref().and_then(MemoryType::parse);
        let recall = match store.recall(&p.query, kind, p.limit).await {
            Ok(r) => r,
            Err(e) => {
                return ProbeResult {
                    label: p.label.clone(),
                    soft: p.soft,
                    returned: vec![],
                    precision: None,
                    recall: None,
                    reciprocal_rank: None,
                    failures: vec![format!("recall failed: {e:#}")],
                };
            }
        };
        let returned: Vec<String> = recall
            .hits
            .iter()
            .map(|h| h.entry.key.to_string())
            .collect();
        let summaries: Vec<Value> = recall.hits.iter().map(|h| h.entry.summary()).collect();
        let refused: BTreeSet<&str> = recall.refused.iter().map(|r| r.key.as_str()).collect();
        let relevant: BTreeSet<&str> = p.relevant.iter().map(String::as_str).collect();

        // Metrics, where they apply.
        let found = returned
            .iter()
            .filter(|k| relevant.contains(k.as_str()))
            .count();
        let (precision, recall_score, rr) = if relevant.is_empty() {
            (None, None, None)
        } else {
            let precision = if returned.is_empty() {
                1.0
            } else {
                found as f64 / returned.len() as f64
            };
            let rr = returned
                .iter()
                .position(|k| relevant.contains(k.as_str()))
                .map(|i| 1.0 / (i + 1) as f64)
                .unwrap_or(0.0);
            (
                Some(precision),
                Some(found as f64 / relevant.len() as f64),
                Some(rr),
            )
        };

        // Hard checks.
        if let Some(top) = &p.top
            && returned.first() != Some(top)
        {
            failures.push(format!(
                "`{top}` should rank first; got {}",
                fmt_keys(&returned)
            ));
        }
        for k in &p.absent {
            if returned.contains(k) {
                failures.push(format!("`{k}` must not be returned"));
            }
        }
        if p.expect_empty && !returned.is_empty() {
            failures.push(format!(
                "should return nothing; got {}",
                fmt_keys(&returned)
            ));
        }
        for k in &p.refused {
            if !refused.contains(k.as_str()) {
                failures.push(format!("`{k}` should be reported refused"));
            }
        }
        for k in &p.stale {
            let flagged = summaries
                .iter()
                .any(|s| s["key"] == k.as_str() && s["stale"] == true);
            if !flagged {
                failures.push(format!("`{k}` should be returned flagged stale"));
            }
        }
        for (k, want) in &p.describe {
            match summaries.iter().find(|s| s["key"] == k.as_str()) {
                Some(s) if s["description"].as_str().unwrap_or("").contains(want) => {}
                Some(s) => failures.push(format!(
                    "`{k}` should read \"…{want}…\"; it reads \"{}\"",
                    s["description"].as_str().unwrap_or("")
                )),
                None => failures.push(format!("`{k}` should be returned")),
            }
        }
        if p.fence {
            let fence = Fence::new(Provenance::Context);
            let rendered = fence.wrap(&serde_json::to_string(&summaries).unwrap());
            // Exactly this fence's two delimiters, and no other delimiter
            // shape — a forged one carries a different nonce, so counting only
            // ours would never see it.
            let (opens, closes) = (
                rendered.matches(&fence.open()).count(),
                rendered.matches(&fence.close()).count(),
            );
            let shapes = rendered.matches("UNTRUSTED-MEMORY:").count();
            if (opens, closes, shapes) != (1, 1, 2) {
                failures.push(format!(
                    "rendered output has {opens} opening and {closes} closing delimiters and \
                     {shapes} delimiter shapes in all; stored text forged a fence"
                ));
            }
        }

        ProbeResult {
            label: p.label.clone(),
            soft: p.soft,
            returned,
            precision,
            recall: recall_score,
            reciprocal_rank: rr,
            failures,
        }
    }
}

fn fmt_keys(keys: &[String]) -> String {
    if keys.is_empty() {
        "nothing".into()
    } else {
        keys.iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

async fn run(scenario: &Scenario) -> ScenarioResult {
    let run = Run::new(&scenario.writers);
    let mut result = ScenarioResult {
        name: scenario.name.clone(),
        dimension: scenario.dimension,
        description: scenario.description.clone(),
        repair_ops: 0,
        probes: vec![],
        errors: vec![],
    };
    for (i, step) in scenario.steps.iter().enumerate() {
        if let Step::Probe(p) = step {
            result.probes.push(run.probe(p).await);
            continue;
        }
        let repair = match step {
            Step::Save(s) => s.repair,
            Step::Status(s) => s.repair,
            Step::Forget(s) => s.repair,
            _ => false,
        };
        if repair {
            result.repair_ops += 1;
        }
        if let Err(e) = run.step(step).await {
            result.errors.push(format!("step {}: {e}", i + 1));
        }
    }
    if let Some(max) = scenario.max_repair_ops
        && result.repair_ops > max
    {
        result.errors.push(format!(
            "repair took {} operations; the budget is {max}",
            result.repair_ops
        ));
    }
    for (_, store) in run.stores {
        store.shutdown().await;
    }
    result
}

// ---- reporting --------------------------------------------------------------

fn mean(xs: impl Iterator<Item = f64>) -> Option<f64> {
    let v: Vec<f64> = xs.collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn pct(x: Option<f64>) -> String {
    x.map(|x| format!("{:.0}%", x * 100.0))
        .unwrap_or_else(|| "–".into())
}

fn report(results: &[ScenarioResult]) -> String {
    let mut out = String::from("## Memory quality evaluation\n\n");
    out.push_str(
        "| dimension | scenarios | checks passed | precision | recall | MRR | repair ops |\n\
         |---|---:|---:|---:|---:|---:|---:|\n",
    );
    let mut by_dim: BTreeMap<Dimension, Vec<&ScenarioResult>> = BTreeMap::new();
    for r in results {
        by_dim.entry(r.dimension).or_default().push(r);
    }
    let row = |label: String, rs: &[&ScenarioResult]| {
        let probes: Vec<&ProbeResult> = rs.iter().flat_map(|r| &r.probes).collect();
        let checks = probes.len() + rs.iter().map(|r| r.errors.len()).sum::<usize>();
        let failed = probes.iter().filter(|p| !p.failures.is_empty()).count()
            + rs.iter().map(|r| r.errors.len()).sum::<usize>();
        let repairs: usize = rs.iter().map(|r| r.repair_ops).sum();
        format!(
            "| {label} | {} | {}/{} | {} | {} | {} | {} |\n",
            rs.len(),
            checks - failed,
            checks,
            pct(mean(probes.iter().filter_map(|p| p.precision))),
            pct(mean(probes.iter().filter_map(|p| p.recall))),
            mean(probes.iter().filter_map(|p| p.reciprocal_rank))
                .map(|x| format!("{x:.2}"))
                .unwrap_or_else(|| "–".into()),
            if repairs > 0 {
                repairs.to_string()
            } else {
                "–".into()
            }
        )
    };
    for (dim, rs) in &by_dim {
        out.push_str(&row(format!("{dim:?}").to_lowercase(), rs));
    }
    let all: Vec<&ScenarioResult> = results.iter().collect();
    out.push_str(&row("**all**".into(), &all));

    let hard: Vec<String> = results
        .iter()
        .flat_map(|r| {
            r.errors
                .iter()
                .map(move |e| format!("- **{}**: {e}", r.name))
                .chain(r.probes.iter().filter(|p| !p.soft).flat_map(move |p| {
                    p.failures
                        .iter()
                        .map(move |f| format!("- **{}** / {}: {f}", r.name, p.label))
                }))
        })
        .collect();
    let soft: Vec<String> = results
        .iter()
        .flat_map(|r| {
            r.probes.iter().filter(|p| p.soft).flat_map(move |p| {
                p.failures
                    .iter()
                    .map(move |f| format!("- **{}** / {}: {f}", r.name, p.label))
            })
        })
        .collect();

    if hard.is_empty() {
        out.push_str("\nAll hard checks pass.\n");
    } else {
        out.push_str(&format!(
            "\n### Failures ({})\n\n{}\n",
            hard.len(),
            hard.join("\n")
        ));
    }
    if !soft.is_empty() {
        out.push_str(&format!(
            "\n### Known gaps — soft probes missed ({})\n\n{}\n",
            soft.len(),
            soft.join("\n")
        ));
    }
    out
}

fn scenario_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
}

fn main() {
    // Arguments that are not flags filter scenarios by name or dimension.
    // Flags (`--quiet`, `--nocapture`, …) are what cargo passes to every test
    // binary; there is nothing here for them to change.
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eval/scenarios");
    let mut scenarios = Vec::new();
    for path in scenario_files(&dir) {
        let raw = std::fs::read_to_string(&path).unwrap();
        let s: Scenario =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let dim = format!("{:?}", s.dimension).to_lowercase();
        if filters.is_empty()
            || filters
                .iter()
                .any(|f| s.name.contains(f.as_str()) || dim == *f)
        {
            scenarios.push(s);
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let results: Vec<ScenarioResult> = rt.block_on(async {
        let mut out = Vec::new();
        for s in &scenarios {
            out.push(run(s).await);
        }
        out
    });

    let text = report(&results);
    println!("{text}");
    // Only when asked, so the report is not appended twice in a CI job that
    // also runs this as part of a plain `cargo test`.
    if std::env::var_os("EVAL_SUMMARY").is_some()
        && let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
    {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(f, "{text}");
        }
    }
    if let Ok(path) = std::env::var("EVAL_REPORT_JSON") {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({ "scenarios": results })).unwrap(),
        )
        .unwrap_or_else(|e| panic!("writing {path}: {e}"));
    }

    let failed: usize = results.iter().map(ScenarioResult::hard_failures).sum();
    if failed > 0 {
        eprintln!("eval: {failed} hard check(s) failed");
        std::process::exit(1);
    }
}
