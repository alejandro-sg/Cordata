use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentRole {
    Core,
    Engine,
    Interface,
    Tool,
    Subsystem,
}

impl ComponentRole {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Core => "CORE",
            Self::Engine => "ENGINE",
            Self::Interface => "UI/CLI",
            Self::Tool => "TOOL",
            Self::Subsystem => "MOD",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComponentStatus {
    Clean,
    Modified { changed_files: Vec<String> },
    Active { active_files: Vec<String>, note: Option<String> },
}

impl ComponentStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    pub fn is_modified(&self) -> bool {
        matches!(self, Self::Modified { .. })
    }

    pub fn changed_count(&self) -> usize {
        match self {
            Self::Clean => 0,
            Self::Modified { changed_files } => changed_files.len(),
            Self::Active { active_files, .. } => active_files.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub relative_path: String,
    pub lines: usize,
    pub is_modified: bool,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub id: String,
    pub name: String,
    pub path: String,
    pub role: ComponentRole,
    pub summary: String,
    pub files: Vec<FileInfo>,
    pub total_lines: usize,
    pub dependencies: Vec<String>,
    pub dependents: Vec<String>,
    pub layer: usize,
    pub status: ComponentStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectMetrics {
    pub total_components: usize,
    pub clean_components: usize,
    pub modified_components: usize,
    pub active_components: usize,
    pub total_files: usize,
    pub total_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Floorplan {
    pub project_name: String,
    pub root: String,
    pub components: Vec<Component>,
    pub connections: Vec<Connection>,
    pub active_scope: Vec<String>,
    pub metrics: ProjectMetrics,
}
