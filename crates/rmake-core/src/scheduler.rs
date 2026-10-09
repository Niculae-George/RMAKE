//! Dependency-aware, bounded-concurrency execution for a frozen build plan.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc,
    thread,
};

use rmake_model::{BuildAction, BuildPlan, Cancellation, Diagnostic, Result};

/// Run ready actions with at most `jobs` workers. The returned bool is the
/// action closure's reuse flag; results are ordered as in `plan.actions`.
pub fn run(
    plan: &BuildPlan,
    jobs: usize,
    cancellation: &Cancellation,
    action: &(dyn Fn(&BuildAction) -> Result<bool> + Sync),
) -> Result<Vec<(String, bool)>> {
    let graph = validate(plan)?;
    cancellation.check()?;

    let worker_limit = jobs.max(1);
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel::<(usize, Result<bool>)>();
        let mut handles = HashMap::new();
        let mut scheduled = vec![false; plan.actions.len()];
        let mut finished = vec![None; plan.actions.len()];
        let mut running = 0usize;
        let mut stopped = false;
        let mut first_error = None;

        loop {
            if cancellation.is_cancelled() && !stopped {
                stopped = true;
                first_error = Some(cancelled());
            }

            while !stopped && running < worker_limit {
                let ready = (0..plan.actions.len()).find(|&index| {
                    !scheduled[index]
                        && graph.prerequisites[index]
                            .iter()
                            .all(|&prerequisite| finished[prerequisite].is_some())
                });
                let Some(index) = ready else { break };

                let worker_sender = sender.clone();
                let worker_action = &plan.actions[index];
                let spawn = thread::Builder::new()
                    .name(format!("rmake-{}", worker_action.id))
                    .spawn_scoped(scope, move || {
                        let outcome = catch_unwind(AssertUnwindSafe(|| action(worker_action)))
                            .unwrap_or_else(|payload| Err(worker_panic(payload)));
                        let _ = worker_sender.send((index, outcome));
                    });
                match spawn {
                    Ok(handle) => {
                        handles.insert(index, handle);
                        scheduled[index] = true;
                        running += 1;
                    }
                    Err(error) => {
                        stopped = true;
                        first_error = Some(Diagnostic::new(
                            "RMAKE-S104",
                            format!("cannot start scheduler worker: {error}"),
                        ));
                    }
                }
            }

            if running == 0 {
                if stopped || finished.iter().all(Option::is_some) {
                    break;
                }
                // validate() proves this cannot occur unless scheduler state
                // becomes inconsistent.
                first_error = Some(Diagnostic::new(
                    "RMAKE-S103",
                    "no runnable action remains in an acyclic build plan",
                ));
                break;
            }

            match receiver.recv() {
                Ok((index, outcome)) => {
                    running -= 1;
                    let Some(handle) = handles.remove(&index) else {
                        if first_error.is_none() {
                            first_error = Some(Diagnostic::new(
                                "RMAKE-S104",
                                "scheduler received an unknown worker completion",
                            ));
                        }
                        stopped = true;
                        continue;
                    };
                    match handle.join() {
                        Ok(()) => match outcome {
                            Ok(reused) => finished[index] = Some(reused),
                            Err(error) => {
                                if first_error.is_none() {
                                    first_error = Some(error);
                                }
                                stopped = true;
                            }
                        },
                        Err(payload) => {
                            if first_error.is_none() {
                                first_error = Some(worker_panic(payload));
                            }
                            stopped = true;
                        }
                    }
                }
                Err(_) => {
                    // All workers have exited without completing the protocol.
                    // Reap every handle before returning the infrastructure error.
                    for (_, handle) in handles.drain() {
                        let _ = handle.join();
                    }
                    running = 0;
                    stopped = true;
                    if first_error.is_none() {
                        first_error = Some(Diagnostic::new(
                            "RMAKE-S104",
                            "scheduler workers disconnected before reporting completion",
                        ));
                    }
                }
            }
        }

        drop(sender);
        if let Some(error) = first_error {
            return Err(error);
        }
        let mut completed = Vec::with_capacity(plan.actions.len());
        for (index, action) in plan.actions.iter().enumerate() {
            let Some(reused) = finished[index] else {
                return Err(Diagnostic::new(
                    "RMAKE-S104",
                    format!("scheduler did not complete action {:?}", action.id),
                ));
            };
            completed.push((action.id.clone(), reused));
        }
        Ok(completed)
    })
}

struct Graph {
    prerequisites: Vec<Vec<usize>>,
}

fn validate(plan: &BuildPlan) -> Result<Graph> {
    let mut ids = BTreeMap::new();
    for (index, action) in plan.actions.iter().enumerate() {
        if ids.insert(action.id.as_str(), index).is_some() {
            return Err(Diagnostic::new(
                "RMAKE-S101",
                format!("build plan contains duplicate action id {:?}", action.id),
            ));
        }
    }

    let mut prerequisites = vec![Vec::new(); plan.actions.len()];
    let mut dependents = vec![Vec::new(); plan.actions.len()];
    let mut indegree = vec![0usize; plan.actions.len()];
    for (index, action) in plan.actions.iter().enumerate() {
        let mut unique = BTreeSet::new();
        for prerequisite in &action.prerequisites {
            let Some(&dependency) = ids.get(prerequisite.as_str()) else {
                return Err(Diagnostic::new(
                    "RMAKE-S102",
                    format!(
                        "action {:?} depends on unknown action {:?}",
                        action.id, prerequisite
                    ),
                ));
            };
            if unique.insert(dependency) {
                prerequisites[index].push(dependency);
                dependents[dependency].push(index);
                indegree[index] += 1;
            }
        }
    }

    let mut ready: VecDeque<_> = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect();
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for &dependent in &dependents[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.push_back(dependent);
            }
        }
    }
    if visited != plan.actions.len() {
        let cycle: Vec<_> = indegree
            .iter()
            .enumerate()
            .filter(|(_, degree)| **degree > 0)
            .map(|(index, _)| plan.actions[index].id.as_str())
            .collect();
        return Err(Diagnostic::new(
            "RMAKE-S103",
            format!(
                "build plan contains a dependency cycle involving {}",
                cycle.join(", ")
            ),
        ));
    }

    Ok(Graph { prerequisites })
}

fn worker_panic(payload: Box<dyn std::any::Any + Send>) -> Diagnostic {
    let detail = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .unwrap_or("worker panicked");
    Diagnostic::new("RMAKE-S104", format!("scheduler action failed: {detail}"))
}

fn cancelled() -> Diagnostic {
    Diagnostic::new("RMAKE-X130", "operation cancelled")
}
