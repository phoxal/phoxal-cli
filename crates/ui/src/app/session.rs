//! One tui-realm terminal loop for an attached Phoxal resident.

use std::cell::RefCell;
use std::io::{self, Stderr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tuirealm::application::{Application, PollStrategy};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::listener::EventListenerCfg;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::Terminal;
use tuirealm::ratatui::backend::CrosstermBackend;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout};
use tuirealm::state::State;

use crate::Theme;
use crate::components::shared::{
    BusRegion, FooterRegion, HeaderRegion, InputRegion, LogsRegion, ModalRegion, OverviewRegion,
    RenderComponent, RenderRegion, RuntimesRegion, TabsRegion,
};
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

struct InputComponent;

impl Component for InputComponent {
    fn view(&mut self, _frame: &mut Frame, _area: Rect) {}

    fn query(&self, _attr: Attribute) -> Option<QueryResult<'_>> {
        None
    }

    fn attr(&mut self, _attr: Attribute, _value: AttrValue) {}

    fn state(&self) -> State {
        State::None
    }

    fn perform(&mut self, cmd: Cmd) -> CmdResult {
        CmdResult::Invalid(cmd)
    }
}

impl AppComponent<Msg, UserEvent> for InputComponent {
    fn on(&mut self, event: &Event<UserEvent>) -> Option<Msg> {
        match event {
            // tui-realm's crossterm adapter normalizes terminal input into this
            // key vocabulary; no release/repeat variants reach the application.
            Event::Keyboard(key) => Some(Msg::Navigate(NavigationMsg::Key(*key))),
            Event::WindowResize(_, _) | Event::FocusGained => {
                Some(Msg::Navigate(NavigationMsg::Refresh { clear: true }))
            }
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

    let model = Rc::new(RefCell::new(AppModel::default()));
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
        if let Some(exit) = render_requested(&mut terminal, &mut application, &model)? {
            return Ok(exit);
        }
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
    }
}

fn render_requested(
    terminal: &mut Terminal<CrosstermBackend<Stderr>>,
    application: &mut Application<ComponentId, Msg, UserEvent>,
    model: &Rc<RefCell<AppModel>>,
) -> Result<Option<AttachmentOutcome>> {
    let (redraw, clear, exit, page) = {
        let state = model.borrow();
        (
            state.redraw_requested,
            state.clear_requested,
            state.exit,
            state.route.page(),
        )
    };
    if clear {
        terminal.clear()?;
    }
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
        let mut state = model.borrow_mut();
        state.redraw_requested = false;
        state.clear_requested = false;
    }
    Ok(exit)
}

fn dispatch(
    model: &Rc<RefCell<AppModel>>,
    effects: &mpsc::Sender<Effect>,
    message: Msg,
) -> Result<()> {
    let emitted = {
        let mut model = model.borrow_mut();
        update(&mut model, message)
    };
    for effect in emitted {
        match effects.try_send(effect) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                anyhow::bail!("attachment effect router stopped");
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                model
                    .borrow_mut()
                    .overview
                    .push_diagnostic("attachment command queue is full; retry".to_string());
            }
        }
    }
    Ok(())
}

fn mount_components(
    application: &mut Application<ComponentId, Msg, UserEvent>,
    model: &Rc<RefCell<AppModel>>,
    theme: Theme,
) -> Result<()> {
    application.mount(ComponentId::Input, Box::new(InputComponent), Vec::new())?;
    application.active(&ComponentId::Input)?;
    mount_region::<HeaderRegion>(application, ComponentId::Header, model, theme)?;
    mount_region::<TabsRegion>(application, ComponentId::Tabs, model, theme)?;
    mount_region::<OverviewRegion>(application, ComponentId::Overview, model, theme)?;
    mount_region::<RuntimesRegion>(application, ComponentId::Runtimes, model, theme)?;
    mount_region::<LogsRegion>(application, ComponentId::Logs, model, theme)?;
    mount_region::<BusRegion>(application, ComponentId::Bus, model, theme)?;
    mount_region::<InputRegion>(application, ComponentId::InputPage, model, theme)?;
    mount_region::<ModalRegion>(application, ComponentId::Modal, model, theme)?;
    mount_region::<FooterRegion>(application, ComponentId::Footer, model, theme)?;
    Ok(())
}

fn mount_region<R: RenderRegion + 'static>(
    application: &mut Application<ComponentId, Msg, UserEvent>,
    id: ComponentId,
    model: &Rc<RefCell<AppModel>>,
    theme: Theme,
) -> Result<()> {
    application.mount(
        id,
        Box::new(RenderComponent::<R>::new(Rc::clone(model), theme)),
        Vec::new(),
    )?;
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
