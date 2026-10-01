use std::{
    env,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use hyprland::{error::HyprError, shared::Address};
use log::debug;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("could not locate the hyprland instance socket")]
    MissingInstance,
    #[error("could not reach hyprland: {0}")]
    IO(#[from] std::io::Error),
}

/// How the running Hyprland reads the arguments of a `dispatch` request.
///
/// Hyprland 0.56 added a Lua configuration provider. When a Lua config is in
/// use, `dispatchRequest` splices the whole argument string into
/// `return hl.dispatch(<args>)` and evaluates it as Lua, so the legacy
/// `name arg` form is a syntax error rather than a dispatch. The two providers
/// therefore need the same intent expressed differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// `hyprlang` config, the historical `name arg` syntax.
    Legacy,
    /// Lua config, arguments written as a `hl.dsp` table.
    Lua,
}

impl Dialect {
    /// Reads the config provider out of a `status` reply.
    ///
    /// A compositor old enough to not report `configProvider` predates the Lua
    /// provider and is treated as [`Dialect::Legacy`].
    fn from_status(reply: &str) -> Self {
        let provider = reply
            .lines()
            .find_map(|line| line.trim().strip_prefix("configProvider:"));
        match provider.map(str::trim) {
            Some(provider) if provider.eq_ignore_ascii_case("lua") => Dialect::Lua,
            _ => Dialect::Legacy,
        }
    }
}

/// A placement or focus request that both dialects can express.
///
/// The variants mirror the dispatchers nest needs, not the wire syntax, so the
/// dialect stays a rendering concern. The window address is borrowed because
/// rendering only formats it and [`Address`] is not `Copy`.
#[derive(Clone, Copy, Debug)]
pub enum Request<'a> {
    /// Move a window to a workspace.
    MoveToWorkspace { window: &'a Address, workspace: i32 },
    /// Focus a workspace.
    FocusWorkspace { workspace: i32 },
    /// Make a window floating without toggling an already floating one.
    SetFloating { window: &'a Address },
    /// Resize a window to an exact size.
    Resize {
        window: &'a Address,
        size: (i16, i16),
    },
    /// Move a window to an exact position.
    Move { window: &'a Address, at: (i16, i16) },
}

impl Request<'_> {
    /// Renders the request the way `dialect` expects it, without the leading
    /// `dispatch` keyword.
    fn render(&self, dialect: Dialect) -> String {
        match (self, dialect) {
            (Request::MoveToWorkspace { window, workspace }, Dialect::Legacy) => {
                format!("movetoworkspace {workspace},address:{window}")
            }
            (Request::MoveToWorkspace { window, workspace }, Dialect::Lua) => format!(
                "hl.dsp.window.move({{ workspace = {workspace}, window = \"address:{window}\" }})"
            ),
            (Request::FocusWorkspace { workspace }, Dialect::Legacy) => {
                format!("workspace {workspace}")
            }
            (Request::FocusWorkspace { workspace }, Dialect::Lua) => {
                format!("hl.dsp.focus({{ workspace = {workspace} }})")
            }
            (Request::SetFloating { window }, Dialect::Legacy) => {
                format!("setfloating address:{window}")
            }
            (Request::SetFloating { window }, Dialect::Lua) => format!(
                "hl.dsp.window.float({{ action = \"enable\", window = \"address:{window}\" }})"
            ),
            (Request::Resize { window, size }, Dialect::Legacy) => format!(
                "resizewindowpixel exact {} {},address:{window}",
                size.0, size.1
            ),
            (Request::Resize { window, size }, Dialect::Lua) => format!(
                "hl.dsp.window.resize({{ x = {}, y = {}, window = \"address:{window}\" }})",
                size.0, size.1
            ),
            (Request::Move { window, at }, Dialect::Legacy) => {
                format!("movewindowpixel exact {} {},address:{window}", at.0, at.1)
            }
            (Request::Move { window, at }, Dialect::Lua) => format!(
                "hl.dsp.window.move({{ x = {}, y = {}, window = \"address:{window}\" }})",
                at.0, at.1
            ),
        }
    }
}

/// Talks to the running Hyprland over its command socket.
///
/// The `hyprland` crate hardcodes the legacy dispatch syntax and keeps its
/// socket writer private, so nest speaks to the socket directly to be able to
/// pick the dialect the running compositor actually understands.
#[derive(Clone, Debug)]
pub struct Ipc {
    socket: PathBuf,
    dialect: Dialect,
}

impl Ipc {
    /// Connects to the Hyprland instance named by the environment and detects
    /// which config provider it runs.
    pub fn connect() -> Result<Self, Error> {
        let runtime = env::var_os("XDG_RUNTIME_DIR").ok_or(Error::MissingInstance)?;
        let signature =
            env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| Error::MissingInstance)?;

        let mut socket = PathBuf::from(runtime);
        socket.push("hypr");
        socket.push(signature);
        socket.push(".socket.sock");

        let dialect = Self::probe(&socket)?;
        debug!("Hyprland speaks the {dialect:?} dispatch dialect");
        Ok(Self { socket, dialect })
    }

    /// Asks Hyprland which config provider is active.
    ///
    /// A compositor old enough to not report `configProvider` predates the Lua
    /// provider and is treated as [`Dialect::Legacy`]; only an unreachable
    /// socket is an error.
    fn probe(socket: &Path) -> Result<Dialect, Error> {
        let reply = Self::send(socket, "status")?;
        Ok(Dialect::from_status(&reply))
    }

    /// Runs `request` against the compositor.
    ///
    /// Hyprland answers a dispatch with exactly `ok` on success and with the
    /// failure reason otherwise; the reply text is the only place that reason
    /// appears.
    pub fn dispatch(&self, request: &Request<'_>) -> Result<(), HyprError> {
        let rendered = request.render(self.dialect);
        debug!("Dispatching {rendered}");
        let reply = Self::send(&self.socket, &format!("/dispatch {rendered}"))?;
        if reply == "ok" {
            Ok(())
        } else {
            Err(HyprError::NotOkDispatch(reply))
        }
    }

    fn send(socket: &Path, command: &str) -> std::io::Result<String> {
        let mut stream = UnixStream::connect(socket)?;
        stream.write_all(command.as_bytes())?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply)?;
        Ok(String::from_utf8_lossy(&reply).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> Address {
        Address::new("0x56483a1379f0")
    }

    /// The legacy strings are the exact bytes the `hyprland` crate used to
    /// send, so a `hyprlang` compositor sees no change from the refactor.
    #[test]
    fn legacy_rendering_matches_the_crate() {
        let window = address();
        let cases = [
            (
                Request::MoveToWorkspace {
                    window: &window,
                    workspace: 7,
                },
                "movetoworkspace 7,address:0x56483a1379f0",
            ),
            (Request::FocusWorkspace { workspace: 3 }, "workspace 3"),
            (
                Request::SetFloating { window: &window },
                "setfloating address:0x56483a1379f0",
            ),
            (
                Request::Resize {
                    window: &window,
                    size: (500, 400),
                },
                "resizewindowpixel exact 500 400,address:0x56483a1379f0",
            ),
            (
                Request::Move {
                    window: &window,
                    at: (100, 100),
                },
                "movewindowpixel exact 100 100,address:0x56483a1379f0",
            ),
        ];

        for (request, expected) in cases {
            assert_eq!(request.render(Dialect::Legacy), expected);
        }
    }

    /// The Lua strings are the ones a 0.56 compositor with a Lua config
    /// answered `ok` to.
    #[test]
    fn lua_rendering_uses_dsp_tables() {
        let window = address();
        let cases = [
            (
                Request::MoveToWorkspace {
                    window: &window,
                    workspace: 7,
                },
                "hl.dsp.window.move({ workspace = 7, window = \"address:0x56483a1379f0\" })",
            ),
            (
                Request::FocusWorkspace { workspace: 3 },
                "hl.dsp.focus({ workspace = 3 })",
            ),
            (
                Request::SetFloating { window: &window },
                "hl.dsp.window.float({ action = \"enable\", window = \"address:0x56483a1379f0\" })",
            ),
            (
                Request::Resize {
                    window: &window,
                    size: (500, 400),
                },
                "hl.dsp.window.resize({ x = 500, y = 400, window = \"address:0x56483a1379f0\" })",
            ),
            (
                Request::Move {
                    window: &window,
                    at: (100, 100),
                },
                "hl.dsp.window.move({ x = 100, y = 100, window = \"address:0x56483a1379f0\" })",
            ),
        ];

        for (request, expected) in cases {
            assert_eq!(request.render(Dialect::Lua), expected);
        }
    }

    #[test]
    fn status_reply_selects_the_dialect() {
        assert_eq!(
            Dialect::from_status("\nconfigProvider: lua\nbackend: wayland\n"),
            Dialect::Lua
        );
        assert_eq!(
            Dialect::from_status("\nconfigProvider: hyprlang\nbackend: wayland\n"),
            Dialect::Legacy
        );
        // A compositor that predates the Lua provider reports no provider at
        // all and can only understand the legacy syntax.
        assert_eq!(Dialect::from_status("backend: wayland\n"), Dialect::Legacy);
    }
}
