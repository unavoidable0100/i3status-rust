use super::{Backend, Info};
use crate::blocks::prelude::*;

mod wl_protocol {
    use wayrs_client;
    use wayrs_client::protocol::*;
    wayrs_scanner::generate!("wl-protocol/wlr-foreign-toplevel-management-unstable-v1.xml");
}
use wayrs_client::connection::Connection;
use wayrs_client::global::GlobalsExt;
use wayrs_client::protocol::*;
use wayrs_client::proxy::{Dispatch, Dispatcher};
use wl_protocol::*;

pub(super) struct WlrToplevelManagement {
    conn: Connection<State>,
    state: State,
}

#[derive(Default)]
struct State {
    new_title: Option<String>,
    toplevels: HashMap<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, Toplevel>,
    active_toplevel: Option<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>,
}

#[derive(Default)]
struct Toplevel {
    title: Option<String>,
    is_active: bool,
}

impl WlrToplevelManagement {
    pub(super) async fn new() -> Result<Self> {
        let mut conn = Connection::connect().error("failed to connect to wayland")?;
        let globals = conn
            .async_collect_initial_globals()
            .await
            .error("wayland error")?;

        conn.set_callback::<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>();

        let _: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 = globals
            .bind(&mut conn, 1..=3)
            .error("unsupported compositor")?;

        conn.async_flush().await.error("wayland error")?;

        Ok(Self {
            conn,
            state: default(),
        })
    }
}

#[async_trait]
impl Backend for WlrToplevelManagement {
    async fn get_info(&mut self) -> Result<Info> {
        loop {
            self.conn.async_recv_events().await.error("wayland error")?;
            self.conn.dispatch_events(&mut self.state)?;
            self.conn.async_flush().await.error("wayland error")?;

            if let Some(title) = self.state.new_title.take() {
                return Ok(Info {
                    title,
                    marks: default(),
                });
            }
        }
    }
}

impl Dispatcher for State {
    type Error = Error;
}

impl Dispatch<wl_registry::WlRegistry> for State {}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1> for State {
    fn try_event(
        &mut self,
        _: &mut Connection<Self>,
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

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1> for State {
    fn event(
        &mut self,
        conn: &mut Connection<Self>,
        wlr_toplevel: zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;

        let toplevel = self.toplevels.get_mut(&wlr_toplevel).unwrap();

        match event {
            Event::Title(title) => {
                toplevel.title = Some(String::from_utf8_lossy(title.as_bytes()).into());
            }
            Event::State(state) => {
                toplevel.is_active = state
                    .chunks_exact(4)
                    .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
                    .any(|s| s == zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
            }
            Event::Closed => {
                if self.active_toplevel == Some(wlr_toplevel) {
                    self.active_toplevel = None;
                    self.new_title = Some(default());
                }

                wlr_toplevel.destroy(conn);
                self.toplevels.remove(&wlr_toplevel);
            }
            Event::Done => {
                if toplevel.is_active {
                    self.active_toplevel = Some(wlr_toplevel);
                    self.new_title = Some(toplevel.title.clone().unwrap_or_default());
                } else if self.active_toplevel == Some(wlr_toplevel) {
                    self.active_toplevel = None;
                    self.new_title = Some(default());
                }
            }
            _ => (),
        }
    }
}
