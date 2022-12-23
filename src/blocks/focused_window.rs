//! Currently focused window
//!
//! This block displays the title and/or the active marks (when used with `sway`/`i3`) of the currently
//! focused window. Supported WMs are: `sway`, `i3` and most wlroots-based compositors. See `driver`
//! option for more info.
//!
//! # Configuration
//!
//! Key | Values | Default
//! ----|--------|--------
//! `format` | A string to customise the output of this block. See below for available placeholders. | <code>" $title.str(0,21) &vert;"</code>
//! `driver` | Which driver to use. Available values: `sway_ipc` - for `i3` and `sway`, `wlr_toplevel_management` - for Wayland compositors that implement [wlr-foreign-toplevel-management-unstable-v1](https://gitlab.freedesktop.org/wlroots/wlr-protocols/-/blob/master/unstable/wlr-foreign-toplevel-management-unstable-v1.xml), `auto` - try to automatically guess which driver to use. | `"auto"`
//!
//! Placeholder     | Value                                                                 | Type | Unit
//! ----------------|-----------------------------------------------------------------------|------|-----
//! `title`         | Window's title (may be absent)                                        | Text | -
//! `marks`         | Window's marks (present only with sway/i3)                            | Text | -
//! `visible_marks` | Window's marks that do not start with `_` (present only with sway/i3) | Text | -
//!
//! # Example
//!
//! ```toml
//! [[block]]
//! block = "focused_window"
//! [block.format]
//! full = " $title.str(0,15) |"
//! short = " $title.str(0,10) |"
//! ```
//!
//! This example instead of hiding block when the window's title is empty displays "Missing"
//!
//! ```toml
//! [[block]]
//! block = "focused_window"
//! format = " $title.str(0,21) | Missing "

use super::prelude::*;
use swayipc_async::{Connection, Event, EventStream, EventType, WindowChange, WorkspaceChange};

mod wl_protocol {
    use wayrs_client;
    use wayrs_client::protocol::*;
    wayrs_scanner::generate!("wl-protocol/wlr-foreign-toplevel-management-unstable-v1.xml");
}
use wayrs_client::event_queue::EventQueue;
use wayrs_client::global::GlobalsExt;
use wayrs_client::protocol::*;
use wayrs_client::proxy::{Dispatch, Dispatcher};
use wl_protocol::*;

#[derive(Deserialize, Debug, SmartDefault)]
#[serde(default)]
pub struct Config {
    format: FormatConfig,
    driver: Driver,
}

#[derive(Deserialize, Debug, SmartDefault)]
#[serde(rename_all = "snake_case")]
enum Driver {
    #[default]
    Auto,
    SwayIpc,
    WlrToplevelManagement,
}

pub async fn run(config: Config, mut api: CommonApi) -> Result<()> {
    let mut widget = Widget::new().with_format(config.format.with_default(" $title.str(0,21) |")?);

    let mut backend: Box<dyn Backend> = match config.driver {
        Driver::Auto => match SwayIpc::new().await {
            Ok(swayipc) => Box::new(swayipc),
            Err(_) => Box::new(WlrToplevelManagement::new().await?),
        },
        Driver::SwayIpc => Box::new(SwayIpc::new().await?),
        Driver::WlrToplevelManagement => Box::new(WlrToplevelManagement::new().await?),
    };

    loop {
        select! {
            _ = api.event() => (),
            info = backend.get_info() => {
                let Info { title, marks } = info?;
                if title.is_empty() {
                    widget.set_values(default());
                } else {
                    widget.set_values(map! {
                        "title" => Value::text(title.clone()),
                        "marks" => Value::text(marks.iter().map(|m| format!("[{m}]")).collect()),
                        "visible_marks" => Value::text(marks.iter().filter(|m| !m.starts_with('_')).map(|m| format!("[{m}]")).collect()),
                    });
                }
                api.set_widget(&widget).await?;
            }
        }
    }
}

#[async_trait]
trait Backend {
    async fn get_info(&mut self) -> Result<Info>;
}

#[derive(Clone, Default)]
struct Info {
    title: String,
    marks: Vec<String>,
}

struct SwayIpc {
    events: EventStream,
    info: Info,
}

impl SwayIpc {
    async fn new() -> Result<Self> {
        Ok(Self {
            events: Connection::new()
                .await
                .error("failed to open connection with swayipc")?
                .subscribe(&[EventType::Window, EventType::Workspace])
                .await
                .error("could not subscribe to window events")?,
            info: default(),
        })
    }
}

#[async_trait]
impl Backend for SwayIpc {
    async fn get_info(&mut self) -> Result<Info> {
        loop {
            let event = self
                .events
                .next()
                .await
                .error("swayipc channel closed")?
                .error("bad event")?;
            match event {
                Event::Window(e) => match e.change {
                    WindowChange::Mark => {
                        self.info.marks = e.container.marks;
                    }
                    WindowChange::Focus => {
                        self.info.title.clear();
                        if let Some(new_title) = &e.container.name {
                            self.info.title.push_str(new_title);
                        }
                        self.info.marks = e.container.marks;
                    }
                    WindowChange::Title => {
                        if e.container.focused {
                            self.info.title.clear();
                            if let Some(new_title) = &e.container.name {
                                self.info.title.push_str(new_title);
                            }
                        } else {
                            continue;
                        }
                    }
                    WindowChange::Close => {
                        self.info.title.clear();
                        self.info.marks.clear();
                    }
                    _ => continue,
                },
                Event::Workspace(e) if e.change == WorkspaceChange::Init => {
                    self.info.title.clear();
                    self.info.marks.clear();
                }
                _ => continue,
            }

            return Ok(self.info.clone());
        }
    }
}

struct WlrToplevelManagement {
    event_queue: EventQueue<WlrToplevelManagementState>,
    state: WlrToplevelManagementState,
}

#[derive(Default)]
struct WlrToplevelManagementState {
    new_title: Option<String>,
    toplevels: HashMap<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, WlrToplevel>,
    active: Option<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>,
}

#[derive(Default)]
struct WlrToplevel {
    title: Option<String>,
    active: bool,
}

impl WlrToplevelManagement {
    async fn new() -> Result<Self> {
        let (globals, mut event_queue) = EventQueue::async_init().await.error("wayland error")?;
        event_queue.set_callback::<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>();
        let _: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 = globals
            .bind(&mut event_queue, 1..=3)
            .error("unsupported compositor")?;
        event_queue
            .connection()
            .async_flush()
            .await
            .error("wayland error")?;
        Ok(Self {
            event_queue,
            state: default(),
        })
    }
}

#[async_trait]
impl Backend for WlrToplevelManagement {
    async fn get_info(&mut self) -> Result<Info> {
        loop {
            self.event_queue
                .async_recv_events()
                .await
                .error("wayland error")?;

            self.event_queue.dispatch_events(&mut self.state)?;

            self.event_queue
                .connection()
                .async_flush()
                .await
                .error("wayland error")?;

            if let Some(title) = self.state.new_title.take() {
                return Ok(Info {
                    title,
                    marks: default(),
                });
            }
        }
    }
}

impl Dispatcher for WlrToplevelManagementState {
    type Error = Error;
}

impl Dispatch<wl_registry::WlRegistry> for WlrToplevelManagementState {}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1>
    for WlrToplevelManagementState
{
    fn try_event(
        &mut self,
        _: &mut EventQueue<Self>,
        _: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
    ) -> Result<()> {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel(toplevel) => {
                self.toplevels.insert(toplevel, default());
                Ok(())
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                Err(Error::new("unexpected 'finished' event"))
            }
        }
    }
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>
    for WlrToplevelManagementState
{
    fn event(
        &mut self,
        event_queue: &mut EventQueue<Self>,
        wlr_toplevel: zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
    ) {
        let toplevel = self.toplevels.get_mut(&wlr_toplevel).unwrap();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title(title) => {
                toplevel.title = Some(String::from_utf8_lossy(title.as_bytes()).into());
            }
            zwlr_foreign_toplevel_handle_v1::Event::State(state) => {
                toplevel.active = state
                    .chunks_exact(4)
                    .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
                    .any(|s| s == zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                if self.active == Some(wlr_toplevel) {
                    self.active = None;
                    self.new_title = Some(default());
                }

                wlr_toplevel.destroy(event_queue);
                self.toplevels.remove(&wlr_toplevel);
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                if toplevel.active {
                    self.active = Some(wlr_toplevel);
                    self.new_title = Some(toplevel.title.clone().unwrap_or_default());
                } else if self.active == Some(wlr_toplevel) {
                    self.active = None;
                    self.new_title = Some(default());
                }
            }
            _ => (),
        }
    }
}
