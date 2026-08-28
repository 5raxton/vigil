use crate::config::{DependencyConfig, DependencyType};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct DepGraph {
    pub services: HashSet<String>,
    edges_after: HashMap<String, Vec<String>>,
    edges_before: HashMap<String, Vec<String>>,
    edges_wants: HashMap<String, Vec<String>>,
    required_deps: HashSet<(String, String)>,
}

impl DepGraph {
    pub fn new() -> Self {
        Self {
            services: HashSet::new(),
            edges_after: HashMap::new(),
            edges_before: HashMap::new(),
            edges_wants: HashMap::new(),
            required_deps: HashSet::new(),
        }
    }

    pub fn add_service(&mut self, name: String) {
        self.services.insert(name);
    }

    pub fn add_dependency(&mut self, from: &str, dep: &DependencyConfig) {
        match dep.kind {
            DependencyType::After => {
                self.edges_after
                    .entry(from.to_string())
                    .or_default()
                    .push(dep.service.clone());
            }
            DependencyType::Before => {
                self.edges_before
                    .entry(from.to_string())
                    .or_default()
                    .push(dep.service.clone());
            }
            DependencyType::Wants => {
                self.edges_wants
                    .entry(from.to_string())
                    .or_default()
                    .push(dep.service.clone());
            }
        }
        if dep.required {
            self.required_deps
                .insert((from.to_string(), dep.service.clone()));
        }
    }

    pub fn get_wanted_services(&self, service: &str) -> Vec<String> {
        self.edges_wants
            .get(service)
            .cloned()
            .unwrap_or_default()
    }

    pub fn get_missing_required(&self, available: &HashSet<String>) -> Vec<(String, String)> {
        let mut missing = Vec::new();
        for (service, dep) in &self.required_deps {
            if available.contains(service) && !available.contains(dep) {
                missing.push((service.clone(), dep.clone()));
            }
        }
        missing
    }

    pub fn resolve_order(&self, services: &[String]) -> Result<Vec<String>, String> {
        let service_set: HashSet<String> = services.iter().cloned().collect();
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();

        for svc in services {
            in_degree.entry(svc.clone()).or_insert(0);
            adj.entry(svc.clone()).or_default();
        }

        for (svc, deps) in &self.edges_after {
            if service_set.contains(svc) {
                for dep in deps {
                    if service_set.contains(dep) {
                        adj.entry(dep.clone()).or_default().push(svc.clone());
                        *in_degree.entry(svc.clone()).or_insert(0) += 1;
                    }
                }
            }
        }

        for (svc, deps) in &self.edges_before {
            if service_set.contains(svc) {
                for dep in deps {
                    if service_set.contains(dep) {
                        adj.entry(svc.clone()).or_default().push(dep.clone());
                        *in_degree.entry(dep.clone()).or_insert(0) += 1;
                    }
                }
            }
        }

        let mut queue: BinaryHeap<Reverse<String>> = BinaryHeap::new();
        for (svc, &degree) in &in_degree {
            if degree == 0 {
                queue.push(Reverse(svc.clone()));
            }
        }

        let mut sorted = Vec::new();
        while let Some(Reverse(svc)) = queue.pop() {
            sorted.push(svc.clone());
            if let Some(deps) = adj.get(&svc) {
                for dep in deps {
                    let degree = in_degree.get_mut(dep).unwrap();
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push(Reverse(dep.clone()));
                    }
                }
            }
        }

        if sorted.len() != services.len() {
            return Err("dependency cycle detected".to_string());
        }

        Ok(sorted)
    }
}

impl Default for DepGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DependencyConfig, DependencyType};

    fn dep_after(service: &str) -> DependencyConfig {
        DependencyConfig {
            service: service.to_string(),
            kind: DependencyType::After,
            required: true,
        }
    }

    fn dep_before(service: &str) -> DependencyConfig {
        DependencyConfig {
            service: service.to_string(),
            kind: DependencyType::Before,
            required: true,
        }
    }

    fn dep_wants(service: &str) -> DependencyConfig {
        DependencyConfig {
            service: service.to_string(),
            kind: DependencyType::Wants,
            required: false,
        }
    }

    #[test]
    fn test_simple_order() {
        let mut graph = DepGraph::new();
        graph.add_service("a".into());
        graph.add_service("b".into());
        graph.add_dependency("b", &dep_after("a"));

        let order = graph
            .resolve_order(&["a".into(), "b".into()])
            .unwrap();
        assert_eq!(order, vec!["a", "b"]);
    }

    #[test]
    fn test_cycle_detection() {
        let mut graph = DepGraph::new();
        graph.add_service("a".into());
        graph.add_service("b".into());
        graph.add_dependency("a", &dep_after("b"));
        graph.add_dependency("b", &dep_after("a"));

        assert!(graph.resolve_order(&["a".into(), "b".into()]).is_err());
    }

    #[test]
    fn test_before_dependency() {
        let mut graph = DepGraph::new();
        graph.add_service("a".into());
        graph.add_service("b".into());
        graph.add_dependency("a", &dep_before("b"));

        let order = graph
            .resolve_order(&["a".into(), "b".into()])
            .unwrap();
        assert_eq!(order, vec!["a", "b"]);
    }

    #[test]
    fn test_chain_propagates_to_leaf() {
        let mut graph = DepGraph::new();
        for s in ["a", "b", "c", "d"] {
            graph.add_service(s.into());
        }
        graph.add_dependency("b", &dep_after("a"));
        graph.add_dependency("c", &dep_after("b"));
        graph.add_dependency("d", &dep_after("c"));

        let order = graph
            .resolve_order(&["a".into(), "b".into(), "c".into(), "d".into()])
            .unwrap();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn test_order_is_deterministic() {
        let mut graph = DepGraph::new();
        for s in ["a", "b", "c", "d", "e", "f"] {
            graph.add_service(s.into());
        }
        graph.add_dependency("d", &dep_after("b"));
        graph.add_dependency("e", &dep_after("b"));
        graph.add_dependency("f", &dep_after("c"));

        let input = vec![
            "d".into(),
            "b".into(),
            "f".into(),
            "c".into(),
            "e".into(),
            "a".into(),
        ];
        let first = graph.resolve_order(&input).unwrap();
        let second = graph.resolve_order(&input).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn test_missing_required_only_for_known_services() {
        let mut graph = DepGraph::new();
        graph.add_service("a".into());
        graph.add_dependency("a", &dep_after("ghost"));

        let available: HashSet<String> = ["a".into()].into_iter().collect();
        let missing = graph.get_missing_required(&available);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], ("a".to_string(), "ghost".to_string()));
    }

    #[test]
    fn test_wants_are_recorded_but_ordering_neutral() {
        let mut graph = DepGraph::new();
        graph.add_service("a".into());
        graph.add_service("b".into());
        graph.add_service("c".into());
        graph.add_dependency("a", &dep_wants("b"));
        graph.add_dependency("b", &dep_after("c"));

        assert_eq!(graph.get_wanted_services("a"), vec!["b".to_string()]);

        let order = graph
            .resolve_order(&["a".into(), "b".into(), "c".into()])
            .unwrap();
        assert!(order.iter().position(|s| s == "c").unwrap() < order.iter().position(|s| s == "b").unwrap());
    }
}
