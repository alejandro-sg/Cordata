use std::collections::{HashMap, VecDeque};

use crate::discovery::RawComponent;
use crate::model::{Component, ComponentRole, ComponentStatus, Connection};

pub struct GraphResult {
    pub components: Vec<Component>,
    pub connections: Vec<Connection>,
}

pub fn build_graph(
    raw_components: Vec<RawComponent>,
    files_map: HashMap<String, (Vec<crate::model::FileInfo>, usize)>,
    status_map: HashMap<String, ComponentStatus>,
) -> GraphResult {
    let mut dependents_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut dependencies_map: HashMap<String, Vec<String>> = HashMap::new();

    for comp in &raw_components {
        dependencies_map.insert(comp.id.clone(), comp.internal_deps.clone());
        for dep in &comp.internal_deps {
            dependents_map.entry(dep.clone()).or_default().push(comp.id.clone());
        }
    }

    // Assign topological layers (Layer 0: Core/Base, Layer 1: Engine/Services, Layer 2: UI/App/Tools)
    let layers = compute_layers(&raw_components, &dependencies_map);

    let mut connections = Vec::new();
    for comp in &raw_components {
        let comp_active = status_map.get(&comp.id).map(|s| s.is_active() || s.is_modified()).unwrap_or(false);
        for dep in &comp.internal_deps {
            let dep_active = status_map.get(dep).map(|s| s.is_active() || s.is_modified()).unwrap_or(false);
            connections.push(Connection {
                from: comp.id.clone(),
                to: dep.clone(),
                kind: "dependency".into(),
                is_active: comp_active || dep_active,
            });
        }
    }

    let mut components = Vec::new();
    for comp in raw_components {
        let (files, total_lines) = files_map.get(&comp.id).cloned().unwrap_or_default();
        let status = status_map.get(&comp.id).cloned().unwrap_or(ComponentStatus::Clean);
        let deps = dependencies_map.get(&comp.id).cloned().unwrap_or_default();
        let mut dependents = dependents_map.get(&comp.id).cloned().unwrap_or_default();
        dependents.sort();

        let layer = *layers.get(&comp.id).unwrap_or(&0);

        components.push(Component {
            id: comp.id,
            name: comp.name,
            path: comp.rel_path,
            role: comp.role,
            summary: comp.summary,
            files,
            total_lines,
            dependencies: deps,
            dependents,
            layer,
            status,
        });
    }

    // Sort components: first by layer, then by role, then by name
    components.sort_by(|a, b| {
        a.layer
            .cmp(&b.layer)
            .then_with(|| role_rank(a.role).cmp(&role_rank(b.role)))
            .then_with(|| a.name.cmp(&b.name))
    });

    GraphResult {
        components,
        connections,
    }
}

fn role_rank(role: ComponentRole) -> usize {
    match role {
        ComponentRole::Core => 0,
        ComponentRole::Engine => 1,
        ComponentRole::Interface => 2,
        ComponentRole::Tool => 3,
        ComponentRole::Subsystem => 4,
    }
}

fn compute_layers(
    components: &[RawComponent],
    _deps_map: &HashMap<String, Vec<String>>,
) -> HashMap<String, usize> {
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut reverse_adj: HashMap<String, Vec<String>> = HashMap::new();

    for comp in components {
        in_degree.entry(comp.id.clone()).or_insert(0);
        for dep in &comp.internal_deps {
            *in_degree.entry(comp.id.clone()).or_insert(0) += 1;
            reverse_adj.entry(dep.clone()).or_default().push(comp.id.clone());
        }
    }

    let mut queue = VecDeque::new();
    let mut layers: HashMap<String, usize> = HashMap::new();

    // Components that depend on nothing else are base layer 0
    for comp in components {
        if comp.internal_deps.is_empty() {
            queue.push_back((comp.id.clone(), 0));
            layers.insert(comp.id.clone(), 0);
        }
    }

    // If all have dependencies (cycle or unusual), seed with Core components
    if queue.is_empty() {
        for comp in components {
            if comp.role == ComponentRole::Core {
                queue.push_back((comp.id.clone(), 0));
                layers.insert(comp.id.clone(), 0);
            }
        }
    }

    // BFS forward
    while let Some((curr, layer)) = queue.pop_front() {
        if let Some(dependents) = reverse_adj.get(&curr) {
            for dep in dependents {
                let current_dep_layer = layers.get(dep).copied().unwrap_or(0);
                let next_layer = layer + 1;
                if next_layer > current_dep_layer && next_layer < 10 {
                    layers.insert(dep.clone(), next_layer);
                    queue.push_back((dep.clone(), next_layer));
                }
            }
        }
    }

    // Fallback layer assignment based on role for any unassigned
    for comp in components {
        layers.entry(comp.id.clone()).or_insert_with(|| match comp.role {
            ComponentRole::Core => 0,
            ComponentRole::Engine => 1,
            ComponentRole::Subsystem => 1,
            ComponentRole::Interface => 2,
            ComponentRole::Tool => 2,
        });
    }

    layers
}
