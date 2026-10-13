//! The Browser Agent view in the dock (docs/app.md §2, §4, phase P4):
//! Trajectory recording, screenshots, and live web inspection for chrome-devtools-mcp.

use std::path::PathBuf;

use cordata_theme::Role;
use cordata_view::shown;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};

use crate::hud;
use crate::theme::{color, tint};
use super::Workspace;

#[derive(Clone, Debug)]
pub struct BrowserStep {
    pub step_number: usize,
    pub action: String,
    pub target: String,
    pub details: String,
    pub screenshot_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct BrowserNetworkReq {
    pub method: String,
    pub url: String,
    pub status: u16,
}

pub struct BrowserView {
    pub url_input: Entity<InputState>,
    pub current_url: String,
    pub steps: Vec<BrowserStep>,
    pub selected_step: usize,
    pub console_logs: Vec<String>,
    pub network_requests: Vec<BrowserNetworkReq>,
    pub sub_tab: BrowserSubTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrowserSubTab {
    #[default]
    Trajectory,
    Console,
    Network,
}

impl BrowserView {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let url_input = cx.new(|cx| InputState::new(window, cx).placeholder("Enter URL to preview or inspect (e.g. http://localhost:3000)…"));
        Self {
            url_input,
            current_url: "about:blank".into(),
            steps: vec![
                BrowserStep {
                    step_number: 1,
                    action: "session_start".into(),
                    target: "browser_context".into(),
                    details: "Chrome DevTools MCP session initialized".into(),
                    screenshot_path: None,
                },
            ],
            selected_step: 0,
            console_logs: vec!["[browser] agent runtime connected".into()],
            network_requests: Vec::new(),
            sub_tab: BrowserSubTab::Trajectory,
        }
    }

    #[allow(dead_code)]
    pub fn add_step(&mut self, action: String, target: String, details: String, screenshot: Option<PathBuf>) {
        let num = self.steps.len() + 1;
        self.steps.push(BrowserStep {
            step_number: num,
            action,
            target,
            details,
            screenshot_path: screenshot,
        });
        self.selected_step = self.steps.len().saturating_sub(1);
    }
}

impl Workspace {
    pub(super) fn browser_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let browser = &self.browser_view;

        // URL bar & Controls
        let nav_bar = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .p(px(4.))
            .bg(color(cx, Role::Surface))
            .border_b_1()
            .border_color(color(cx, Role::Hairline))
            .child(
                Button::new("browser-prev-btn")
                    .label("◀")
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.browser_view.selected_step > 0 {
                            this.browser_view.selected_step -= 1;
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("browser-next-btn")
                    .label("▶")
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.browser_view.selected_step + 1 < this.browser_view.steps.len() {
                            this.browser_view.selected_step += 1;
                            cx.notify();
                        }
                    })),
            )
            .child(div().flex_1().child(Input::new(&browser.url_input)))
            .child(
                Button::new("browser-open-external-btn")
                    .label("Open ↗")
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let url = this.browser_view.url_input.read(cx).value().to_string();
                        let target = if url.is_empty() { &this.browser_view.current_url } else { &url };
                        if target.starts_with("http://") || target.starts_with("https://") {
                            let _ = std::process::Command::new("open").arg(target).spawn();
                        }
                    })),
            );

        // Subtabs: Trajectory, Console, Network
        const SUB_TABS: [BrowserSubTab; 3] = [BrowserSubTab::Trajectory, BrowserSubTab::Console, BrowserSubTab::Network];
        let sub_nav = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(color(cx, Role::Hairline))
            .pb(px(4.))
            .children(SUB_TABS.iter().map(|&tab| {
                let selected = browser.sub_tab == tab;
                let label = match tab {
                    BrowserSubTab::Trajectory => format!("Trajectory ({})", browser.steps.len()),
                    BrowserSubTab::Console => format!("Console ({})", browser.console_logs.len()),
                    BrowserSubTab::Network => format!("Network ({})", browser.network_requests.len()),
                };
                div()
                    .id(("browser-subtab", tab as usize))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .text_xs()
                    .font_family(cordata_theme::fonts::MONO)
                    .text_color(color(cx, if selected { Role::Accent } else { Role::TextSecondary }))
                    .when(selected, |el| el.bg(tint(cx, Role::Accent, 0.1)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browser_view.sub_tab = tab;
                        cx.notify();
                    }))
                    .child(SharedString::from(label))
            }));

        let content = match browser.sub_tab {
            BrowserSubTab::Trajectory => self.browser_trajectory_content(cx),
            BrowserSubTab::Console => self.browser_console_content(cx),
            BrowserSubTab::Network => self.browser_network_content(cx),
        };

        div()
            .id("browser-dock-pane")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(nav_bar)
            .child(sub_nav)
            .child(content)
            .into_any_element()
    }

    fn browser_trajectory_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let browser = &self.browser_view;
        let selected_step = browser.steps.get(browser.selected_step).cloned();

        let steps_list = {
            let items = browser.steps.iter().enumerate().map(|(ix, step)| {
                let is_selected = browser.selected_step == ix;
                div()
                    .id(("browser-step", ix))
                    .p(px(6.))
                    .rounded(px(2.))
                    .cursor_pointer()
                    .bg(color(cx, if is_selected { Role::Surface } else { Role::Ground }))
                    .border_1()
                    .border_color(color(cx, if is_selected { Role::Accent } else { Role::Hairline }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browser_view.selected_step = ix;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(color(cx, if is_selected { Role::Accent } else { Role::Text }))
                                    .child(SharedString::from(format!("Step {}: {}", step.step_number, step.action))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(cordata_theme::fonts::MONO)
                                    .text_color(color(cx, Role::TextDim))
                                    .child(SharedString::from(shown(&step.target))),
                            ),
                    )
            });

            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(items)
        };

        let detail_panel = if let Some(step) = selected_step {
            div()
                .p(px(8.))
                .bg(color(cx, Role::Surface))
                .rounded(px(2.))
                .border_1()
                .border_color(color(cx, Role::Hairline))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(color(cx, Role::Accent))
                        .child(SharedString::from(format!("Action: {}", step.action))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color(cx, Role::TextSecondary))
                        .child(SharedString::from(step.details.clone())),
                )
                .when_some(step.screenshot_path, |el, path| {
                    el.child(
                        div()
                            .pt(px(4.))
                            .child(hud::label("screenshot preview", cx))
                            .child(
                                div()
                                    .p(px(4.))
                                    .bg(color(cx, Role::Ground))
                                    .rounded(px(2.))
                                    .text_xs()
                                    .font_family(cordata_theme::fonts::MONO)
                                    .text_color(color(cx, Role::Agent))
                                    .child(SharedString::from(format!("frame: {}", path.display()))),
                            ),
                    )
                })
        } else {
            div()
                .p(px(8.))
                .text_xs()
                .text_color(color(cx, Role::TextDim))
                .child("No step selected.")
        };

        div()
            .id("browser-trajectory-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(hud::frame("recorded actions", steps_list, cx))
            .child(hud::frame("step inspector", detail_panel, cx))
            .into_any_element()
    }

    fn browser_console_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let logs = self.browser_view.console_logs.iter().enumerate().map(|(ix, log)| {
            div()
                .id(("console-log", ix))
                .text_xs()
                .font_family(cordata_theme::fonts::MONO)
                .text_color(color(cx, Role::TextSecondary))
                .child(SharedString::from(log.clone()))
        });

        div()
            .id("browser-console-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(6.))
            .bg(color(cx, Role::Ground))
            .rounded(px(2.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(logs)
            .into_any_element()
    }

    fn browser_network_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let items = self.browser_view.network_requests.iter().enumerate().map(|(ix, req)| {
            let role = if req.status < 400 { Role::Ok } else { Role::Danger };
            div()
                .id(("net-req", ix))
                .flex()
                .items_center()
                .justify_between()
                .p(px(4.))
                .text_xs()
                .font_family(cordata_theme::fonts::MONO)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().text_color(color(cx, Role::Accent)).child(SharedString::from(req.method.clone())))
                        .child(div().text_color(color(cx, Role::Text)).child(SharedString::from(req.url.clone()))),
                )
                .child(div().text_color(color(cx, role)).child(SharedString::from(req.status.to_string())))
        });

        div()
            .id("browser-network-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(6.))
            .bg(color(cx, Role::Ground))
            .rounded(px(2.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(items)
            .into_any_element()
    }
}
