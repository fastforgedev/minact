//! Job dependency scheduler — builds and executes the job DAG.
//!
//! Uses topological sort to determine job execution order based on `needs:` declarations.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::types::WorkflowError;
use crate::workflow::*;

/// Represents a node in the job dependency graph.
#[derive(Debug, Clone)]
pub struct JobNode {
    /// The job's unique identifier.
    pub id: String,
    /// The job definition.
    pub job: Job,
    /// Job IDs that this job depends on.
    pub dependencies: Vec<String>,
}

/// The job scheduler determines execution order from the dependency graph.
pub struct JobScheduler {
    /// All job nodes indexed by ID.
    nodes: HashMap<String, JobNode>,
}

impl JobScheduler {
    /// Build a scheduler from a workflow definition.
    pub fn new(workflow: &Workflow) -> Self {
        let mut nodes = HashMap::new();

        for (job_id, job) in &workflow.jobs {
            let dependencies = job.needs.clone().unwrap_or_default();
            nodes.insert(
                job_id.clone(),
                JobNode {
                    id: job_id.clone(),
                    job: job.clone(),
                    dependencies,
                },
            );
        }

        Self { nodes }
    }

    /// Topologically sort the jobs for execution.
    /// Returns a list of job IDs in execution order.
    /// Returns an error if a circular dependency is detected.
    pub fn resolve_execution_order(&self) -> Result<Vec<String>, WorkflowError> {
        // A `needs:` pointing at a job that does not exist would otherwise
        // stall the topological sort and be misreported as a cycle.
        for (id, node) in &self.nodes {
            for dep in &node.dependencies {
                if !self.nodes.contains_key(dep) {
                    return Err(WorkflowError::Other(format!(
                        "Job '{}' needs unknown job '{}'",
                        id, dep
                    )));
                }
            }
        }

        // Build in-degree counts
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();

        for (id, node) in &self.nodes {
            in_degree.entry(id.clone()).or_insert(0);
            for dep in &node.dependencies {
                adjacency.entry(dep.clone()).or_default().push(id.clone());
                *in_degree.entry(id.clone()).or_insert(0) += 1;
            }
        }

        // Kahn's algorithm
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut order = Vec::new();

        for (id, degree) in &in_degree {
            if *degree == 0 {
                queue.push_back(id.clone());
            }
        }

        let mut visited_count = 0;
        while let Some(job_id) = queue.pop_front() {
            order.push(job_id.clone());
            visited_count += 1;

            if let Some(neighbors) = adjacency.get(&job_id) {
                for neighbor in neighbors {
                    if let Some(degree) = in_degree.get_mut(neighbor) {
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push_back(neighbor.clone());
                        }
                    }
                }
            }
        }

        if visited_count != self.nodes.len() {
            return Err(WorkflowError::CircularDependency);
        }

        Ok(order)
    }

    /// Get all jobs that can be run in parallel at a given execution layer.
    /// Groups jobs by dependency depth (0 = no dependencies, 1 = depends on layer 0, etc.)
    pub fn resolve_parallel_layers(&self) -> Result<Vec<Vec<String>>, WorkflowError> {
        let order = self.resolve_execution_order()?;
        let mut layers: Vec<Vec<String>> = Vec::new();
        let mut depths: HashMap<String, usize> = HashMap::new();

        for job_id in &order {
            let node = &self.nodes[job_id];
            let depth = if node.dependencies.is_empty() {
                0
            } else {
                node.dependencies
                    .iter()
                    .filter_map(|d| depths.get(d))
                    .max()
                    .unwrap_or(&0)
                    + 1
            };

            depths.insert(job_id.clone(), depth);

            while layers.len() <= depth {
                layers.push(Vec::new());
            }
            layers[depth].push(job_id.clone());
        }

        // Jobs reach here in `HashMap` order, so without this the same
        // workflow lays out differently from one run to the next — and a job
        // that runs first one time runs second the next. Independent jobs have
        // no required order, so pick a stable one.
        for layer in &mut layers {
            layer.sort();
        }

        Ok(layers)
    }

    /// Get a specific job node by ID.
    pub fn get_job(&self, id: &str) -> Option<&JobNode> {
        self.nodes.get(id)
    }

    /// Get all job nodes.
    pub fn all_jobs(&self) -> &HashMap<String, JobNode> {
        &self.nodes
    }

    /// Check if a job has unmet dependencies remaining.
    pub fn has_unmet_dependencies(&self, job_id: &str, completed: &HashSet<String>) -> bool {
        if let Some(node) = self.nodes.get(job_id) {
            node.dependencies.iter().any(|d| !completed.contains(d))
        } else {
            false
        }
    }

    /// Get the immediate dependents of a job.
    pub fn get_dependents(&self, job_id: &str) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.dependencies.contains(&job_id.to_string()))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_workflow(jobs: HashMap<String, Job>) -> Workflow {
        Workflow {
            name: "test".to_string(),
            on: OnConfig::default(),
            env: HashMap::new(),
            defaults: None,
            jobs,
            file_path: None,
        }
    }

    fn make_job(name: &str, needs: Option<Vec<String>>) -> Job {
        Job {
            name: name.to_string(),
            needs,
            if_condition: None,
            env: HashMap::new(),
            defaults: None,
            steps: vec![Step {
                id: None,
                name: "step".to_string(),
                uses: None,
                run: Some("echo hello".to_string()),
                shell: None,
                working_directory: None,
                with: HashMap::new(),
                env: HashMap::new(),
                if_condition: None,
                continue_on_error: None,
                timeout_minutes: None,
            }],
            outputs: None,
            runs_on: None,
            timeout_minutes: None,
            continue_on_error: None,
            strategy: None,
            container: None,
            services: HashMap::new(),
        }
    }

    #[test]
    fn test_linear_dependencies() {
        let mut jobs = HashMap::new();
        jobs.insert("build".to_string(), make_job("Build", None));
        jobs.insert(
            "test".to_string(),
            make_job("Test", Some(vec!["build".to_string()])),
        );
        jobs.insert(
            "deploy".to_string(),
            make_job("Deploy", Some(vec!["test".to_string()])),
        );

        let workflow = make_workflow(jobs);
        let scheduler = JobScheduler::new(&workflow);
        let order = scheduler.resolve_execution_order().unwrap();

        assert_eq!(order, vec!["build", "test", "deploy"]);
    }

    #[test]
    fn test_parallel_jobs() {
        let mut jobs = HashMap::new();
        jobs.insert("build-a".to_string(), make_job("Build A", None));
        jobs.insert("build-b".to_string(), make_job("Build B", None));
        jobs.insert("build-c".to_string(), make_job("Build C", None));

        let workflow = make_workflow(jobs);
        let scheduler = JobScheduler::new(&workflow);
        let layers = scheduler.resolve_parallel_layers().unwrap();

        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 3);
    }

    #[test]
    fn test_diamond_dependency() {
        let mut jobs = HashMap::new();
        jobs.insert("setup".to_string(), make_job("Setup", None));
        jobs.insert(
            "build-a".to_string(),
            make_job("Build A", Some(vec!["setup".to_string()])),
        );
        jobs.insert(
            "build-b".to_string(),
            make_job("Build B", Some(vec!["setup".to_string()])),
        );
        jobs.insert(
            "merge".to_string(),
            make_job(
                "Merge",
                Some(vec!["build-a".to_string(), "build-b".to_string()]),
            ),
        );

        let workflow = make_workflow(jobs);
        let scheduler = JobScheduler::new(&workflow);
        let layers = scheduler.resolve_parallel_layers().unwrap();

        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec!["setup"]);
        assert_eq!(layers[1].len(), 2); // build-a and build-b in parallel
        assert_eq!(layers[2], vec!["merge"]);
    }

    #[test]
    fn test_circular_dependency() {
        let mut jobs = HashMap::new();
        jobs.insert("a".to_string(), make_job("A", Some(vec!["b".to_string()])));
        jobs.insert("b".to_string(), make_job("B", Some(vec!["a".to_string()])));

        let workflow = make_workflow(jobs);
        let scheduler = JobScheduler::new(&workflow);
        let result = scheduler.resolve_execution_order();

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WorkflowError::CircularDependency
        ));
    }

    #[test]
    fn test_unknown_dependency_is_reported_clearly() {
        let mut jobs = HashMap::new();
        jobs.insert(
            "test".to_string(),
            make_job("Test", Some(vec!["buidl".to_string()])),
        );

        let workflow = make_workflow(jobs);
        let scheduler = JobScheduler::new(&workflow);
        let err = scheduler.resolve_execution_order().unwrap_err().to_string();

        // A typo in `needs:` must not masquerade as a dependency cycle.
        assert!(err.contains("needs unknown job 'buidl'"), "{}", err);
    }

    #[test]
    fn parallel_layers_are_ordered_the_same_every_time() {
        let mut jobs = HashMap::new();
        jobs.insert("setup".to_string(), make_job("Setup", None));
        for id in ["zebra", "alpha", "middle"] {
            jobs.insert(
                id.to_string(),
                make_job(id, Some(vec!["setup".to_string()])),
            );
        }
        let workflow = make_workflow(jobs);

        // Independent jobs have no required order, but an arbitrary one makes
        // runs of the same workflow disagree with each other and with the
        // graph a UI draws.
        for _ in 0..8 {
            let layers = JobScheduler::new(&workflow)
                .resolve_parallel_layers()
                .expect("layers");
            assert_eq!(layers[0], vec!["setup".to_string()]);
            assert_eq!(
                layers[1],
                vec![
                    "alpha".to_string(),
                    "middle".to_string(),
                    "zebra".to_string()
                ],
            );
        }
    }
}
