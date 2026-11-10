//! The field's model: every open thread as a node standing in its checkout, laid out where it will stay.
//!
//! The deck's third view (after lanes and ground). It answers one question at a glance: who is doing what, and where.
//! - A **territory** is one checkout (a folder with files). Threads in the same folder share it.
//! - A **node** is one open thread. Its place on the field comes from its slot and its folder, never from what it is
//!   doing, so a node you looked at a minute ago is where you left it.
//! - A **chip** is a component the thread stands on (see [`crate::flight::territory`]).
//! - A **link** joins two threads that both write one component: a collision, read from the files they touched.
//!
//! Pure geometry in world units; the window multiplies by the camera. Nothing here is guessed from a title.

use std::path::PathBuf;

use crate::desk::Touch;
use crate::flight::Zone;
use crate::runs::Run;
use crate::work::TriageIn;

pub const NODE_W: f32 = 264.;
pub const NODE_H: f32 = 84.;
/// A node and the room under it for its chips and gate.
pub const ROW_H: f32 = 236.;
/// Territory padding, and the strip on its left where collision links run.
pub const PAD: f32 = 24.;
pub const GUTTER: f32 = 20.;
/// Room for the territory's name above its first node.
pub const HEAD: f32 = 40.;
pub const GAP: f32 = 56.;
pub const MAX_CHIPS: usize = 3;
pub const STEP_H: f32 = 72.;
/// One handoff line under a step, and the room above the next step.
pub const HANDOFF_H: f32 = 26.;
const STEP_GAP: f32 = 28.;
/// Room under a run's name for its state line and buttons.
pub const RUN_HEAD: f32 = 96.;

pub const ZOOM_MIN: f32 = 0.35;
pub const ZOOM_MAX: f32 = 2.2;
/// Below this the field shows marks and names only.
pub const FLEET_BELOW: f32 = 0.6;
/// From this on a node also shows its last actions.
pub const AGENT_FROM: f32 = 1.4;

/// How much a node says, chosen by zoom alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Altitude {
    /// Mark and name: who is working, who is stuck.
    Fleet,
    /// Plus what it is doing now and the components it stands on.
    Place,
    /// Plus its last actions.
    Agent,
}

pub fn altitude(zoom: f32) -> Altitude {
    if zoom < FLEET_BELOW {
        Altitude::Fleet
    } else if zoom < AGENT_FROM {
        Altitude::Place
    } else {
        Altitude::Agent
    }
}

/// Where the window looks: `screen = world * zoom + (x, y)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub zoom: f32,
    pub x: f32,
    pub y: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self { zoom: 1.0, x: PAD, y: PAD }
    }
}

impl Camera {
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.x += dx;
        self.y += dy;
    }

    /// Scale by `factor`, keeping the world point under the screen point `(ax, ay)` where it is.
    pub fn zoom_at(&mut self, factor: f32, ax: f32, ay: f32) {
        let next = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        let k = next / self.zoom;
        self.x = ax - (ax - self.x) * k;
        self.y = ay - (ay - self.y) * k;
        self.zoom = next;
    }

    pub fn to_screen(&self, wx: f32, wy: f32) -> (f32, f32) {
        (wx * self.zoom + self.x, wy * self.zoom + self.y)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub name: String,
    pub touch: Touch,
    /// Another thread writes this component too.
    pub contested: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// 1-based, as shown everywhere else on the deck.
    pub slot: usize,
    /// Index into the inputs this field was laid out from.
    pub index: usize,
    pub x: f32,
    pub y: f32,
    pub chips: Vec<Chip>,
    /// Components past [`MAX_CHIPS`].
    pub more: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Territory {
    /// The checkout's folder name.
    pub place: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Two threads writing one component. Always inside one territory, so it runs down that territory's gutter.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub a: usize,
    pub b: usize,
    pub zone: String,
    pub x: f32,
    pub y0: f32,
    pub y1: f32,
}

/// One orchestrated run, standing to the right of the threads: its steps in plan order.
#[derive(Debug, Clone, PartialEq)]
pub struct RunTerritory {
    /// Index into the runs this field was laid out from.
    pub run: usize,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepNode {
    pub run: usize,
    pub step: usize,
    pub x: f32,
    pub y: f32,
    /// Indices into the run's handoffs for this step.
    pub handoffs: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Field {
    pub territories: Vec<Territory>,
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
    pub runs: Vec<RunTerritory>,
    pub steps: Vec<StepNode>,
    pub w: f32,
    pub h: f32,
}

impl Field {
    /// Stand the runs to the right of the threads. Steps keep plan order, so a step does not move when its worker does.
    pub fn with_runs(mut self, runs: &[Run]) -> Field {
        let mut x = if self.w > 0.0 { self.w + GAP } else { 0.0 };
        for (ri, run) in runs.iter().enumerate() {
            let w = GUTTER + NODE_W + PAD * 2.0;
            let mut y = HEAD + RUN_HEAD;
            for (si, step) in run.steps.iter().enumerate() {
                let handoffs: Vec<usize> = run.handoffs.iter().enumerate().filter(|(_, h)| h.step_id == step.id).map(|(i, _)| i).collect();
                let row = STEP_H + handoffs.len() as f32 * HANDOFF_H + STEP_GAP;
                self.steps.push(StepNode { run: ri, step: si, x: x + GUTTER + PAD, y, handoffs });
                y += row;
            }
            let h = y.max(HEAD + RUN_HEAD + STEP_H);
            self.runs.push(RunTerritory { run: ri, x, y: 0.0, w, h });
            self.h = self.h.max(h);
            x += w + GAP;
        }
        if !runs.is_empty() {
            self.w = x - GAP;
        }
        self
    }
}

fn writes(touch: Touch) -> bool {
    matches!(touch, Touch::Edited | Touch::Working | Touch::Waiting)
}

/// Territories left to right in the order their first thread was opened; nodes top to bottom by slot.
pub fn layout(threads: &[TriageIn], zones: &[Zone]) -> Field {
    let mut folders: Vec<&PathBuf> = Vec::new();
    for t in threads {
        if !folders.contains(&&t.folder) {
            folders.push(&t.folder);
        }
    }
    let mut field = Field::default();
    let mut x = 0.0;
    for folder in folders {
        let members: Vec<&TriageIn> = threads.iter().filter(|t| &t.folder == folder).collect();
        let (w, h) = (GUTTER + NODE_W + PAD * 2.0, HEAD + members.len() as f32 * ROW_H);
        field.territories.push(Territory { place: folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), x, y: 0.0, w, h });
        for (row, t) in members.iter().enumerate() {
            let on: Vec<Chip> = zones
                .iter()
                .filter_map(|z| z.occupants.iter().find(|o| o.slot == t.slot).map(|o| Chip { name: z.name.clone(), touch: o.touch, contested: z.contested && writes(o.touch) }))
                .collect();
            let more = on.len().saturating_sub(MAX_CHIPS);
            field.nodes.push(Node { slot: t.slot, index: t.index, x: x + GUTTER + PAD, y: HEAD + row as f32 * ROW_H, chips: on.into_iter().take(MAX_CHIPS).collect(), more });
        }
        field.h = field.h.max(h);
        x += w + GAP;
    }
    field.w = (x - GAP).max(0.0);
    for z in zones.iter().filter(|z| z.contested) {
        let mut writers: Vec<usize> = z.occupants.iter().filter(|o| writes(o.touch)).map(|o| o.slot).collect();
        writers.sort_unstable();
        for pair in writers.windows(2) {
            let (Some(a), Some(b)) = (field.nodes.iter().find(|n| n.slot == pair[0]), field.nodes.iter().find(|n| n.slot == pair[1])) else { continue };
            field.links.push(Link { a: a.slot, b: b.slot, zone: z.name.clone(), x: a.x - PAD - GUTTER / 2.0, y0: a.y + NODE_H / 2.0, y1: b.y + NODE_H / 2.0 });
        }
    }
    field
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TabState;
    use crate::flight::Occupant;

    fn thread(slot: usize, folder: &str) -> TriageIn {
        TriageIn {
            index: slot - 1,
            slot,
            title: format!("t{slot}"),
            folder: PathBuf::from(folder),
            state: TabState::Idle,
            active: false,
            files: 0,
            added: 0,
            removed: 0,
            check: None,
            falsification_warning: false,
            gate_title: None,
            gate_signature: None,
            gate_high_risk: false,
            working_secs: None,
            overlaps: Vec::new(),
            now: String::new(),
            beats: Vec::new(),
            tips: Vec::new(),
        }
    }

    fn zone(name: &str, place: &str, on: &[(usize, Touch)]) -> Zone {
        let occupants: Vec<Occupant> = on.iter().map(|(slot, touch)| Occupant { slot: *slot, touch: *touch }).collect();
        let contested = occupants.iter().filter(|o| writes(o.touch)).count() >= 2;
        Zone { name: name.into(), place: place.into(), occupants, contested }
    }

    #[test]
    fn threads_in_one_folder_share_a_territory_and_stack_by_slot() {
        let f = layout(&[thread(1, "/p/app"), thread(2, "/p/wt"), thread(3, "/p/app")], &[]);
        assert_eq!(f.territories.iter().map(|t| t.place.as_str()).collect::<Vec<_>>(), ["app", "wt"]);
        let at = |slot: usize| f.nodes.iter().find(|n| n.slot == slot).map(|n| (n.x, n.y)).unwrap();
        assert_eq!(at(1).0, at(3).0, "same folder, same column");
        assert_eq!(at(3).1 - at(1).1, ROW_H);
        assert!(at(2).0 > at(1).0 + NODE_W, "the second folder sits clear to the right");
        assert_eq!(f.territories[0].h, HEAD + 2.0 * ROW_H);
    }

    #[test]
    fn a_node_does_not_move_when_its_state_or_another_thread_changes() {
        let before = layout(&[thread(1, "/p/app"), thread(2, "/p/app")], &[]);
        let mut busy = thread(1, "/p/app");
        busy.state = TabState::Working;
        let after = layout(&[busy, thread(2, "/p/app"), thread(3, "/p/wt")], &[zone("core", "app", &[(1, Touch::Working)])]);
        for slot in [1, 2] {
            let pos = |f: &Field| f.nodes.iter().find(|n| n.slot == slot).map(|n| (n.x, n.y)).unwrap();
            assert_eq!(pos(&before), pos(&after));
        }
    }

    #[test]
    fn chips_are_the_components_a_thread_stands_on_and_the_rest_are_counted() {
        let zones: Vec<Zone> = ["a", "b", "c", "d"].iter().map(|n| zone(n, "app", &[(1, Touch::Read)])).chain([zone("e", "app", &[(2, Touch::Edited)])]).collect();
        let f = layout(&[thread(1, "/p/app"), thread(2, "/p/app")], &zones);
        assert_eq!(f.nodes[0].chips.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(f.nodes[0].more, 1);
        assert_eq!(f.nodes[1].chips, vec![Chip { name: "e".into(), touch: Touch::Edited, contested: false }]);
    }

    #[test]
    fn two_writers_on_one_component_are_linked_and_a_reader_is_not() {
        let zones = [zone("core", "app", &[(1, Touch::Edited), (2, Touch::Read), (3, Touch::Working)])];
        let f = layout(&[thread(1, "/p/app"), thread(2, "/p/app"), thread(3, "/p/app")], &zones);
        assert_eq!(f.links.len(), 1);
        let l = &f.links[0];
        assert_eq!((l.a, l.b, l.zone.as_str()), (1, 3, "core"));
        assert!(l.y1 > l.y0 && l.x < f.nodes[0].x);
        let chip = |slot: usize| f.nodes.iter().find(|n| n.slot == slot).unwrap().chips[0].contested;
        assert!(chip(1) && chip(3) && !chip(2), "the reader's chip is not marked contested");
    }

    #[test]
    fn zooming_keeps_the_point_under_the_cursor_and_stays_in_bounds() {
        let mut cam = Camera { zoom: 1.0, x: 10.0, y: 20.0 };
        let world = ((300.0 - cam.x) / cam.zoom, (200.0 - cam.y) / cam.zoom);
        cam.zoom_at(1.5, 300.0, 200.0);
        let (sx, sy) = cam.to_screen(world.0, world.1);
        assert!((sx - 300.0).abs() < 0.01 && (sy - 200.0).abs() < 0.01);
        for _ in 0..40 {
            cam.zoom_at(1.5, 0.0, 0.0);
        }
        assert_eq!(cam.zoom, ZOOM_MAX);
        for _ in 0..40 {
            cam.zoom_at(0.5, 0.0, 0.0);
        }
        assert_eq!(cam.zoom, ZOOM_MIN);
    }

    #[test]
    fn altitude_follows_zoom_alone() {
        assert_eq!(altitude(0.4), Altitude::Fleet);
        assert_eq!(altitude(FLEET_BELOW), Altitude::Place);
        assert_eq!(altitude(1.0), Altitude::Place);
        assert_eq!(altitude(AGENT_FROM), Altitude::Agent);
    }

    fn run(steps: &[(&str, &str)], handoffs: &[(&str, &str)]) -> Run {
        use crate::runs::{Handoff, Step};
        Run {
            run_id: "run-1".into(),
            goal: "g".into(),
            state: "execute".into(),
            reason: String::new(),
            steps: steps.iter().map(|(id, status)| Step { id: (*id).into(), name: (*id).into(), status: (*status).into(), worker: None, attempts: 0, scope: Vec::new() }).collect(),
            handoffs: handoffs.iter().map(|(id, step)| Handoff { id: (*id).into(), step_id: (*step).into(), reason: "worker_failed".into(), from: "a".into(), to: None, step_status: "failed".into() }).collect(),
            history: Vec::new(),
        }
    }

    #[test]
    fn a_run_stands_right_of_the_threads_and_its_steps_keep_plan_order() {
        let base = layout(&[thread(1, "/p/app")], &[]);
        let right_edge = base.w;
        let f = base.with_runs(&[run(&[("s1", "completed"), ("s2", "running"), ("s3", "pending")], &[("h1", "s2"), ("h2", "s2")])]);
        assert_eq!(f.runs.len(), 1);
        assert!(f.runs[0].x >= right_edge + GAP, "clear of the thread territory");
        assert_eq!(f.steps.iter().map(|s| s.step).collect::<Vec<_>>(), [0, 1, 2]);
        let y = |i: usize| f.steps[i].y;
        assert_eq!(y(1) - y(0), STEP_H + STEP_GAP);
        assert_eq!(y(2) - y(1), STEP_H + 2.0 * HANDOFF_H + STEP_GAP, "room for the two handoffs under s2");
        assert_eq!(f.steps[1].handoffs, vec![0, 1]);
        assert!(f.steps[0].handoffs.is_empty());
        assert!(f.runs[0].h >= y(2) + STEP_H && f.w >= f.runs[0].x + f.runs[0].w);
    }

    #[test]
    fn runs_alone_start_at_the_left_and_no_runs_change_nothing() {
        let f = layout(&[], &[]).with_runs(&[run(&[("s1", "pending")], &[])]);
        assert_eq!(f.runs[0].x, 0.0);
        let before = layout(&[thread(1, "/p/app")], &[]);
        assert_eq!(before.clone().with_runs(&[]), before);
    }

    #[test]
    fn an_empty_window_is_an_empty_field() {
        assert_eq!(layout(&[], &[]), Field::default());
    }
}
