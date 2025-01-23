//! Graph views of a [`WorkflowSpec`]: ordering, reachability, cycle
//! detection, and the validation built on them.
//!
//! Steps are identified by name; if names repeat (which validation rejects)
//! a dependency resolves to the first step with that name. Dependencies on
//! unknown steps are ignored by the traversals. Every traversal breaks ties
//! by declaration order, so results are deterministic.

use std::collections::{BTreeSet, HashMap, VecDeque};

use belay_json::{FromJsonError, Value};

use super::WorkflowSpec;
use crate::error::quote;
use crate::ids::{LABEL_RULE, STEP_RULE};
use crate::job::validate_meta;
use crate::limits::{MAX_STEP_NAME_LEN, MAX_WORKFLOW_STEPS};
use crate::validate::{Validate, Validator};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Unvisited,
    OnPath,
    Done,
}

impl WorkflowSpec {
    /// Step name to position; the first occurrence wins.
    fn positions(&self) -> HashMap<&str, usize> {
        let mut map = HashMap::with_capacity(self.steps.len());
        for (i, step) in self.steps.iter().enumerate() {
            map.entry(step.name.as_str()).or_insert(i);
        }
        map
    }

    /// For each step, the positions of the steps it depends on.
    fn parent_edges(&self) -> Vec<Vec<usize>> {
        let positions = self.positions();
        self.steps
            .iter()
            .map(|s| {
                s.depends_on
                    .iter()
                    .filter_map(|d| positions.get(d.as_str()).copied())
                    .collect()
            })
            .collect()
    }

    /// Steps without dependencies, in declaration order.
    pub fn roots(&self) -> impl Iterator<Item = &super::StepSpec> {
        self.steps.iter().filter(|s| s.depends_on.is_empty())
    }

    /// Steps that depend directly on `name`, in declaration order.
    #[must_use]
    pub fn dependents(&self, name: &str) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|s| s.depends_on.iter().any(|d| d == name))
            .map(|s| s.name.as_str())
            .collect()
    }

    /// Every step `name` depends on, directly or not.
    #[must_use]
    pub fn ancestors(&self, name: &str) -> BTreeSet<&str> {
        self.reach(name, |spec, step| {
            spec.get_step(step)
                .map(|s| s.depends_on.iter().map(String::as_str).collect())
                .unwrap_or_default()
        })
    }

    /// Every step that depends on `name`, directly or not: what
    /// `skip_dependents` skips.
    #[must_use]
    pub fn descendants(&self, name: &str) -> BTreeSet<&str> {
        self.reach(name, |spec, step| spec.dependents(step))
    }

    /// Breadth-first closure of `next` from `start`, excluding `start`
    /// unless it lies on a cycle.
    fn reach<'a>(
        &'a self,
        start: &str,
        next: impl Fn(&'a WorkflowSpec, &str) -> Vec<&'a str>,
    ) -> BTreeSet<&'a str> {
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<&'a str> = next(self, start).into();
        while let Some(step) = queue.pop_front() {
            if seen.insert(step) {
                queue.extend(next(self, step));
            }
        }
        seen
    }

    /// Step names in an order that runs every step after its dependencies,
    /// preferring declaration order. On a cycle, returns the steps that
    /// could not be placed.
    pub fn topological_order(&self) -> Result<Vec<&str>, Vec<&str>> {
        let parents = self.parent_edges();
        let mut placed = vec![false; self.steps.len()];
        let mut order = Vec::with_capacity(self.steps.len());
        // O(n^2) over at most MAX_WORKFLOW_STEPS steps; simple and keeps the
        // declaration-order tie break obvious.
        while order.len() < self.steps.len() {
            let next = (0..self.steps.len())
                .find(|&i| !placed[i] && parents[i].iter().all(|&p| placed[p]));
            match next {
                Some(i) => {
                    placed[i] = true;
                    order.push(self.steps[i].name.as_str());
                }
                None => {
                    let stuck = (0..self.steps.len()).filter(|&i| !placed[i]);
                    return Err(stuck.map(|i| self.steps[i].name.as_str()).collect());
                }
            }
        }
        Ok(order)
    }

    /// A dependency cycle, if there is one, as the path `[a, b, c, a]`
    /// where each step depends on the next. The path starts at its
    /// earliest-declared step.
    #[must_use]
    pub fn find_cycle(&self) -> Option<Vec<&str>> {
        let parents = self.parent_edges();
        let mut color = vec![Color::Unvisited; self.steps.len()];
        let mut path = Vec::new();
        for start in 0..self.steps.len() {
            if color[start] == Color::Unvisited {
                if let Some(mut cycle) = visit(start, &parents, &mut color, &mut path) {
                    let first = cycle
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, &node)| node)
                        .map_or(0, |(pos, _)| pos);
                    cycle.rotate_left(first);
                    cycle.push(cycle[0]);
                    return Some(cycle.iter().map(|&i| self.steps[i].name.as_str()).collect());
                }
            }
        }
        None
    }
}

/// Depth-first search along dependency edges; returns the nodes of the
/// first cycle found, in path order.
fn visit(
    node: usize,
    parents: &[Vec<usize>],
    color: &mut [Color],
    path: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    color[node] = Color::OnPath;
    path.push(node);
    for &next in &parents[node] {
        match color[next] {
            Color::OnPath => {
                let pos = path
                    .iter()
                    .position(|&p| p == next)
                    .expect("nodes marked OnPath are on the current path");
                return Some(path[pos..].to_vec());
            }
            Color::Unvisited => {
                if let Some(cycle) = visit(next, parents, color, path) {
                    return Some(cycle);
                }
            }
            Color::Done => {}
        }
    }
    path.pop();
    color[node] = Color::Done;
    None
}

/// Step names referenced by `{{steps.<name>...}}` placeholders anywhere in
/// the string values of `payload` (object keys are not scanned).
///
/// ```
/// use belay_json::json;
/// use belay_proto::workflow::referenced_steps;
///
/// let payload = json!({"url": "{{steps.fetch.result.url}}", "n": ["{{ steps.count }}", "{{other}}"]});
/// assert_eq!(referenced_steps(&payload).into_iter().collect::<Vec<_>>(), ["count", "fetch"]);
/// ```
#[must_use]
pub fn referenced_steps(payload: &Value) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    collect_references(payload, &mut found);
    found
}

fn collect_references<'a>(value: &'a Value, found: &mut BTreeSet<&'a str>) {
    match value {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(open) = rest.find("{{") {
                let after = &rest[open + 2..];
                let Some(close) = after.find("}}") else { break };
                let inner = after[..close].trim();
                if let Some(path) = inner.strip_prefix("steps.") {
                    let name = path.split('.').next().unwrap_or(path);
                    if !name.is_empty() {
                        found.insert(name);
                    }
                }
                rest = &after[close + 2..];
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_references(v, found)),
        Value::Object(map) => map.values().for_each(|v| collect_references(v, found)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

impl Validate for WorkflowSpec {
    fn validate_into(&self, v: &mut Validator) {
        v.check_name("name", &self.name, LABEL_RULE.max(MAX_STEP_NAME_LEN));
        validate_meta(v, &self.meta);
        if self.steps.is_empty() {
            v.report("steps", "must contain at least one step");
            return;
        }
        if self.steps.len() > MAX_WORKFLOW_STEPS {
            v.report(
                "steps",
                format!(
                    "at most {MAX_WORKFLOW_STEPS} steps are allowed, got {}",
                    self.steps.len()
                ),
            );
            return;
        }

        let positions = self.positions();
        let known: Vec<&str> = self.steps.iter().map(|s| s.name.as_str()).collect();
        // Graph checks only make sense once every edge points somewhere.
        let mut graph_ok = true;
        for (i, step) in self.steps.iter().enumerate() {
            v.at_index("steps", i, |v| {
                v.check_name("name", &step.name, STEP_RULE);
                let first = positions[step.name.as_str()];
                if first != i {
                    graph_ok = false;
                    v.report(
                        "name",
                        format!(
                            "duplicate step name {} (also steps[{first}])",
                            quote(&step.name)
                        ),
                    );
                }
                for (k, dep) in step.depends_on.iter().enumerate() {
                    let problem = if *dep == step.name {
                        Some(format!("step {} cannot depend on itself", quote(dep)))
                    } else if !positions.contains_key(dep.as_str()) {
                        let e = FromJsonError::unknown_variant(dep, "step", &known);
                        Some(e.message().to_owned())
                    } else if step.depends_on[..k].contains(dep) {
                        Some(format!("duplicate dependency {}", quote(dep)))
                    } else {
                        None
                    };
                    if let Some(problem) = problem {
                        graph_ok = false;
                        v.at_index("depends_on", k, |v| v.report_here(problem));
                    }
                }
                v.nested("job", &step.job);
                v.at_key("job", |v| {
                    v.check(step.job.workflow.is_none(), "workflow", || {
                        "must not be set; the workflow engine links each step to its run".to_owned()
                    });
                });
            });
        }
        if !graph_ok {
            return;
        }
        if let Some(cycle) = self.find_cycle() {
            v.report(
                "steps",
                format!(
                    "dependency cycle: {} (each step depends on the next)",
                    cycle.join(" -> ")
                ),
            );
            return;
        }
        for (i, step) in self.steps.iter().enumerate() {
            let refs = referenced_steps(&step.job.payload);
            if refs.is_empty() {
                continue;
            }
            let ancestors = self.ancestors(&step.name);
            for name in refs.into_iter().filter(|r| !ancestors.contains(r)) {
                let message = if positions.contains_key(name) {
                    format!(
                        "placeholder refers to step {}, which {} does not depend on",
                        quote(name),
                        quote(&step.name)
                    )
                } else {
                    format!("placeholder refers to unknown step {}", quote(name))
                };
                v.at_index("steps", i, |v| {
                    v.at_key("job", |v| v.report("payload", message))
                });
            }
        }
    }
}
