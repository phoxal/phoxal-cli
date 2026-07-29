//! One tui-realm terminal loop for an attached Phoxal resident.

use std::io::{self, Stderr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tui_realm_stdlib::components::Phantom;
use tuirealm::application::{Application, PollStrategy};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::listener::EventListenerCfg;
use tuirealm::ratatui::Terminal;
use tuirealm::ratatui::backend::CrosstermBackend;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout};

use crate::Theme;
use crate::components::shared::{Region, RenderComponent};
use crate::terminal::{TerminalGuard, install_panic_hook};

use super::effect::{AttachmentOutcome, Effect};
use super::id::{ComponentId, PageId};
use super::message::{Msg, NavigationMsg, SessionInput};
use super::model::AppModel;
use super::subscriptions::{InputPort, PendingInputs, UserEvent};
use super::update::update;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiOptions {
    pub title: String,
    pub theme: Theme,
}

#[derive(Component)]
struct InputComponent {
    component: Phantom,
}

impl Default for InputComponent {
    fn default() -> Self {
        Self {
            component: Phantom::default(),
        }
    }
}

impl AppComponent<Msg, UserEvent> for InputComponent {
    fn on(&mut self, event: &Event<UserEvent>) -> Option<Msg> {
        match event {
            Event::Keyboard(key) => Some(Msg::Navigate(NavigationMsg::Key(*key))),
            Event::WindowResize(width, height) => Some(Msg::Navigate(NavigationMsg::Resize {
                width: *width,
                height: *height,
            })),
            Event::FocusGained => Some(Msg::Navigate(NavigationMsg::Resize {
                width: 0,
                height: 0,
            })),
            Event::User(UserEvent::Wake) => Some(Msg::Wake),
            _ => None,
        }
    }
}

pub async fn run(
    ingress: mpsc::Receiver<SessionInput>,
    effects: mpsc::Sender<Effect>,
    options: UiOptions,
) -> Result<AttachmentOutcome> {
    install_panic_hook();
    if !TerminalGuard::should_use_terminal(&io::stderr()) {
        anyhow::bail!(
            "interactive resident sessions require a terminal; run this command in a TTY"
        );
    }
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || run_blocking(handle, ingress, effects, options))
        .await
        .context("attachment UI worker panicked")?
}

fn run_blocking(
    handle: Handle,
    ingress: mpsc::Receiver<SessionInput>,
    effects: mpsc::Sender<Effect>,
    options: UiOptions,
) -> Result<AttachmentOutcome> {
    let title = phoxal_cli_core::session::sanitize_terminal_text(&options.title);
    let _guard = TerminalGuard::enter(&title)?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal: Terminal<CrosstermBackend<Stderr>> = Terminal::new(backend)?;
    terminal.clear()?;
    TerminalGuard::set_title(&title)?;

    let model = Arc::new(Mutex::new(AppModel::default()));
    let pending = Arc::new(PendingInputs::default());
    let listener = EventListenerCfg::default()
        .with_handle(handle)
        .async_crossterm_input_listener(Duration::ZERO, 32)
        .add_async_port(
            Box::new(InputPort::new(ingress, Arc::clone(&pending))),
            Duration::ZERO,
            1,
        );
    let mut application: Application<ComponentId, Msg, UserEvent> = Application::init(listener);
    mount_components(&mut application, &model, options.theme)?;

    loop {
        let messages = application
            .tick(PollStrategy::BlockCollectUpTo(64))
            .context("tui-realm event listener failed")?;
        for message in messages {
            if message == Msg::Wake {
                for input in pending.drain() {
                    dispatch(&model, &effects, input.into())?;
                }
            } else {
                dispatch(&model, &effects, message)?;
            }
        }

        let (redraw, exit, page) = {
            let state = model.lock().unwrap_or_else(PoisonError::into_inner);
            (state.redraw_requested, state.exit, state.route.page())
        };
        if redraw {
            terminal.draw(|frame| {
                let [header, tabs, body, footer] = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(2),
                        Constraint::Length(2),
                        Constraint::Min(5),
                        Constraint::Length(2),
                    ])
                    .areas(frame.area());
                application.view(&ComponentId::Header, frame, header);
                application.view(&ComponentId::Tabs, frame, tabs);
                application.view(&component_for_page(page), frame, body);
                application.view(&ComponentId::Modal, frame, frame.area());
                application.view(&ComponentId::Footer, frame, footer);
            })?;
            let mut state = model.lock().unwrap_or_else(PoisonError::into_inner);
            state.redraw_requested = false;
            state.redraws = state.redraws.saturating_add(1);
        }
        if let Some(exit) = exit {
            return Ok(exit);
        }
    }
}

fn dispatch(
    model: &Arc<Mutex<AppModel>>,
    effects: &mpsc::Sender<Effect>,
    message: Msg,
) -> Result<()> {
    let emitted = {
        let mut model = model.lock().unwrap_or_else(PoisonError::into_inner);
        update(&mut model, message)
    };
    for effect in emitted {
        effects
            .blocking_send(effect)
            .context("attachment effect router stopped")?;
    }
    Ok(())
}

fn mount_components(
    application: &mut Application<ComponentId, Msg, UserEvent>,
    model: &Arc<Mutex<AppModel>>,
    theme: Theme,
) -> Result<()> {
    application.mount(
        ComponentId::Input,
        Box::new(InputComponent::default()),
        Vec::new(),
    )?;
    application.active(&ComponentId::Input)?;
    for (id, region) in [
        (ComponentId::Header, Region::Header),
        (ComponentId::Tabs, Region::Tabs),
        (ComponentId::Overview, Region::Page(PageId::Overview)),
        (ComponentId::Runtimes, Region::Page(PageId::Runtimes)),
        (ComponentId::Logs, Region::Page(PageId::Logs)),
        (ComponentId::Bus, Region::Page(PageId::Bus)),
        (ComponentId::InputPage, Region::Page(PageId::Input)),
        (ComponentId::Modal, Region::Modal),
        (ComponentId::Footer, Region::Footer),
    ] {
        application.mount(
            id,
            Box::new(RenderComponent::new(Arc::clone(model), theme, region)),
            Vec::new(),
        )?;
    }
    Ok(())
}

const fn component_for_page(page: PageId) -> ComponentId {
    match page {
        PageId::Overview => ComponentId::Overview,
        PageId::Runtimes => ComponentId::Runtimes,
        PageId::Logs => ComponentId::Logs,
        PageId::Bus => ComponentId::Bus,
        PageId::Input => ComponentId::InputPage,
    }
}
