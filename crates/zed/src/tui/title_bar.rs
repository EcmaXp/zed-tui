use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use project::Project;
use theme::ActiveTheme as _;
use ui::{Color, Label, LabelCommon as _, h_flex};
use workspace::{Workspace, project_window_title};

pub struct TerminalTitleBar {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalTitleBar {
    pub fn new(
        workspace: &Workspace,
        workspace_handle: Entity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        Self {
            workspace: workspace_handle.downgrade(),
            _subscriptions: vec![
                cx.observe(&project, |_, _, cx| cx.notify()),
                cx.observe(&git_store, |_, _, cx| cx.notify()),
                cx.observe(&workspace_handle, |_, _, cx| cx.notify()),
            ],
            project,
        }
    }

    fn branch_label(&self, cx: &App) -> Option<String> {
        let repository = self.project.read(cx).active_repository(cx)?;
        let branch = repository.read(cx).branch.as_ref()?;
        Some(format!("⎇ {}", branch.name()))
    }

    fn active_item_title(&self, cx: &App) -> Option<SharedString> {
        let workspace = self.workspace.upgrade()?;
        let item = workspace.read(cx).active_item(cx)?;
        Some(item.tab_content_text(0, cx))
    }
}

impl Render for TerminalTitleBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self.branch_label(cx);
        let active_item = self.active_item_title(cx);
        h_flex()
            .w_full()
            .gap_2()
            .px_1()
            .bg(cx.theme().colors().title_bar_background)
            .child(
                Label::new(project_window_title(self.project.read(cx), cx)).color(Color::Default),
            )
            .when_some(branch, |this, branch| {
                this.child(Label::new(branch).color(Color::Muted))
            })
            .child(div().flex_1())
            .when_some(active_item, |this, title| {
                this.child(Label::new(title).color(Color::Muted))
            })
    }
}
