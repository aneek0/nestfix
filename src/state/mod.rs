use crate::config::{Config, FilterMode};
use chrono::Utc;
use hyprland::{
    data::{Clients, FullscreenMode},
    dispatch::{Dispatch, DispatchType, WindowIdentifier, WorkspaceIdentifierWithSpecial},
    error::HyprError,
    shared::{Address, HyprData},
};
use log::{debug, info};
use std::{
    collections::HashMap,
    num::ParseIntError,
    str::ParseBoolError,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};
use thiserror::Error;

mod safemap;
pub use safemap::SafeMap;

mod program;
pub use program::{PendingMove, Program};

mod window;
pub use window::Window;

mod workspace;
pub use workspace::Workspace;

mod floatingwindow;
pub use floatingwindow::FloatingWindow;

#[derive(Error, Debug)]
pub enum Error {
    #[error("hyprland error: {0}")]
    HyprError(#[from] HyprError),
    #[error("address not mapped to a class")]
    BlankAddress,
    #[error("class not mapped to a program")]
    BlankClass,
}

/// Outcome of a window placement attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The dispatcher moved the window.
    Moved,
    /// The placement was skipped because the window is in a fullscreen mode
    /// (maximized or fullscreen). Hyprland refuses to move or resize such a
    /// window, and its layout owns the geometry until the mode is left, so the
    /// remembered placement is deliberately not applied.
    Unchanged,
    /// The window is filtered out of placement in the configured mode.
    Filtered,
}

/// Hyprland answers some dispatches with a deliberate no-op instead of `ok`.
/// The `hyprland` crate maps every non-`ok` reply to
/// [`HyprError::NotOkDispatch`], so the reply text is the only way to tell such
/// a no-op apart from a genuine failure.
fn is_noop_dispatch(error: &HyprError) -> bool {
    matches!(
        error,
        HyprError::NotOkDispatch(reason)
            if reason.contains("didn't change") || reason.contains("Previous workspace doesn't exist")
    )
}

/// Hyprland sends window addresses without a `0x` prefix and the `hyprland`
/// crate adds one, but `Address::fmt_new` adds it unconditionally, which yields
/// `0x0x...` for an already prefixed address. Addresses are therefore
/// normalised to exactly one prefix before being used as map keys or dispatch
/// arguments.
fn normalize_address(address: Address) -> Address {
    let raw = address.to_string();
    Address::new(raw.trim_start_matches("0x"))
}

/// Nest remembers the moves it dispatches until their `movewindow` event
/// arrives; the list stays bounded by the number of windows nest places.
const PENDING_MOVE_LIMIT: usize = 8;

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("invalid format found")]
    InvalidFormat,
    #[error("could parse int: {0}")]
    Int(#[from] ParseIntError),
    #[error("could parse bool: {0}")]
    Bool(#[from] ParseBoolError),
}

#[derive(Clone)]
pub struct State {
    addresses: SafeMap<Address, Window>,
    programs: SafeMap<String, Program>,
    current_workspace: Arc<AtomicI32>,
    workspace_list: Arc<[String]>,
    workspace_mode: FilterMode,
    workspace_buffer: usize,
    floating_list: Arc<[String]>,
    floating_mode: FilterMode,
    restore_list: Arc<[String]>,
    restore_mode: FilterMode,
    restore_timeout: i64,
    pub changed: Arc<AtomicBool>,
}

pub type WorkspaceConfig = (Arc<[String]>, FilterMode, usize);
pub type FloatingConfig = (Arc<[String]>, FilterMode);
pub type RestoreConfig = (Arc<[String]>, FilterMode, i64);

impl State {
    pub fn new(
        workspace_config: WorkspaceConfig,
        floating_config: FloatingConfig,
        restore_config: RestoreConfig,
    ) -> Self {
        Self {
            addresses: SafeMap::new(),
            programs: SafeMap::new(),
            workspace_list: workspace_config.0,
            workspace_mode: workspace_config.1,
            workspace_buffer: workspace_config.2,
            floating_list: floating_config.0,
            floating_mode: floating_config.1,
            restore_list: restore_config.0,
            restore_mode: restore_config.1,
            restore_timeout: restore_config.2,
            current_workspace: Arc::new(AtomicI32::new(1)),
            changed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn load(programs: Vec<Program>, config: Config) -> Self {
        let state = Self::new(
            (
                config.workspace.filter.programs.into(),
                config.workspace.filter.mode,
                config.workspace.buffer,
            ),
            (
                config.floating.filter.programs.into(),
                config.floating.filter.mode,
            ),
            (
                config.restore.filter.programs.into(),
                config.restore.filter.mode,
                config.restore.timeout,
            ),
        );
        let mut programs_map = state.programs.0.lock().await;
        for program in programs {
            programs_map.insert(program.class.clone(), program);
        }
        state.clone()
    }
    pub async fn add_window(&self, class: String, address: Address) {
        let address = normalize_address(address);
        {
            // Creates new program if none exists
            let mut programs = self.programs.0.lock().await;
            if !programs.contains_key(&class) {
                let positions = vec![Workspace {
                    workspace_id: self.current_workspace.load(Ordering::Relaxed),
                    timestamp: Utc::now().timestamp(),
                }];
                let _ = programs.insert(
                    class.clone(),
                    Program {
                        class: class.clone(),
                        workspaces: positions,
                        floating_window: None,
                        pending_moves: Vec::new(),
                    },
                );
            }
        }
        {
            // Maps the address to the program
            let window = Window {
                class: class.clone(),
                timestamp: Utc::now(),
                origin: self.current_workspace(),
            };
            let mut addresses = self.addresses.0.lock().await;
            addresses.insert(address.clone(), window);
        }
        debug!("Window {address} of type {class} added");
    }

    // Removes mapping between window and program, it will never remove a programs state
    pub async fn remove_window(&self, address: Address) -> Result<(), Error> {
        let address = normalize_address(address);
        let mut addresses = self.addresses.0.lock().await;
        if let Some(window) = addresses.remove(&address) {
            let diff = Utc::now() - window.timestamp;
            let is_in_list = self.restore_list.contains(&window.class);
            if self.restore_timeout >= diff.num_seconds()
                && ((is_in_list && self.restore_mode == FilterMode::Include)
                    || (!is_in_list && self.restore_mode == FilterMode::Exclude))
            {
                match Dispatch::call_async(DispatchType::Workspace(
                    WorkspaceIdentifierWithSpecial::Id(window.origin),
                ))
                .await
                {
                    Ok(()) => (),
                    // The origin workspace is still the active one, so Hyprland
                    // refuses the switch as a no-op.
                    Err(err) if is_noop_dispatch(&err) => (),
                    Err(err) => return Err(Error::HyprError(err)),
                }
            }
            // A window that is gone will never report its move back.
            let mut programs = self.programs.0.lock().await;
            if let Some(program) = programs.get_mut(&window.class) {
                program.pending_moves.retain(|p| p.address != address);
            }
            debug!(
                "Window {address} of type {} removed after {}s",
                window.class,
                diff.num_seconds()
            );
        }
        Ok(())
    }

    pub async fn window_moved(&self, address: Address, workspace_id: i32) -> Result<(), Error> {
        let address = normalize_address(address);
        let addresses = self.addresses.0.lock().await;
        let window = match addresses.get(&address) {
            Some(val) => val,
            None => {
                return Err(Error::BlankAddress);
            }
        };

        let mut programs = self.programs.0.lock().await;
        let program = match programs.get_mut(&window.class) {
            Some(val) => val,
            None => {
                return Err(Error::BlankClass);
            }
        };
        // Nest ignores the moves it dispatched itself, matched by address: a
        // boolean flag cannot do this, because a dispatch onto the workspace a
        // window already occupies emits no event at all on Hyprland 0.56 and
        // would leave the flag set for the next, genuine move.
        if let Some(index) = program
            .pending_moves
            .iter()
            .position(|pending| pending.address == address)
        {
            let pending = program.pending_moves.remove(index);
            debug!("Internal move, ignoring results");
            // The move landed somewhere else than requested, so it was a
            // genuine user move and nest learns from it.
            if pending.workspace_id == workspace_id {
                return Ok(());
            }
            let position = Workspace {
                workspace_id,
                timestamp: Utc::now().timestamp(),
            };
            program.workspaces.push(position);
            while program.workspaces.len() > self.workspace_buffer {
                program.workspaces.remove(0);
            }
            self.changed.store(true, Ordering::Relaxed);
            info!(
                "Program of type {} got moved to workspace {}",
                window.class, workspace_id
            );
            return Ok(());
        }

        let position = Workspace {
            workspace_id,
            timestamp: Utc::now().timestamp(),
        };
        program.workspaces.push(position);

        while program.workspaces.len() > self.workspace_buffer {
            program.workspaces.remove(0);
        }

        self.changed.store(true, Ordering::Relaxed);
        info!(
            "Program of type {} got moved to workspace {}",
            window.class, workspace_id
        );

        Ok(())
    }

    pub async fn move_window(
        &self,
        address: &Address,
        workspace_id: i32,
    ) -> Result<Placement, Error> {
        let address = normalize_address(address.clone());
        let addresses = self.addresses.0.lock().await;
        let mut programs = self.programs.0.lock().await;

        let window = match addresses.get(&address) {
            Some(val) => val,
            None => return Err(Error::BlankAddress),
        };

        let is_in_list = self.workspace_list.contains(&window.class);
        if (!is_in_list && self.workspace_mode == FilterMode::Include)
            || (is_in_list && self.workspace_mode == FilterMode::Exclude)
        {
            return Ok(Placement::Filtered);
        }

        let program = match programs.get_mut(&window.class) {
            Some(val) => val,
            None => return Err(Error::BlankClass),
        };

        // Records the intent so the `movewindow` event this dispatch may
        // trigger is recognised as nest's own work rather than a user move.
        program.pending_moves.push(PendingMove {
            address: address.clone(),
            workspace_id,
        });
        if program.pending_moves.len() > PENDING_MOVE_LIMIT {
            program.pending_moves.remove(0);
        }

        match Dispatch::call_async(DispatchType::MoveToWorkspace(
            WorkspaceIdentifierWithSpecial::Id(workspace_id),
            Some(WindowIdentifier::Address(address.clone())),
        ))
        .await
        {
            Ok(()) => Ok(Placement::Moved),
            // Hyprland refuses a dispatch that targets the workspace the window
            // already occupies, and on 0.56 it does so silently for a window
            // that is alone on that workspace: no event follows, so the record
            // is dropped here rather than left to be matched by a later move.
            Err(err) if is_noop_dispatch(&err) => {
                program.pending_moves.retain(|p| p.address != address);
                Ok(Placement::Unchanged)
            }
            Err(err) => {
                program.pending_moves.retain(|p| p.address != address);
                Err(Error::HyprError(err))
            }
        }
    }

    pub async fn add_floating_window(
        &self,
        class: &str,
        window: FloatingWindow,
    ) -> Result<(), Error> {
        let mut programs = self.programs.0.lock().await;

        let program = match programs.get_mut(class) {
            Some(val) => val,
            None => return Err(Error::BlankClass),
        };

        let change = match &program.floating_window {
            Some(last) => last.at != window.at || last.size != window.size,
            None => true,
        };

        if change {
            program.floating_window = Some(window);
            self.changed.store(true, Ordering::Relaxed);
        }

        Ok(())
    }

    pub async fn remove_floating_window(&self, class: &str) -> Result<(), Error> {
        let mut programs = self.programs.0.lock().await;

        let program = match programs.get_mut(class) {
            Some(val) => val,
            None => return Err(Error::BlankClass),
        };

        program.floating_window = None;
        self.changed.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub async fn move_float_window(
        &self,
        address: &Address,
        at: (i16, i16),
        size: (i16, i16),
    ) -> Result<Placement, Error> {
        let address = normalize_address(address.clone());
        let addresses = self.addresses.0.lock().await;

        let window = match addresses.get(&address) {
            Some(val) => val,
            None => return Err(Error::BlankAddress),
        };

        let is_in_list = self.floating_list.contains(&window.class);
        if (!is_in_list && self.floating_mode == FilterMode::Include)
            || (is_in_list && self.floating_mode == FilterMode::Exclude)
        {
            return Ok(Placement::Filtered);
        }

        {
            let programs = self.programs.0.lock().await;
            if !programs.contains_key(&window.class) {
                return Err(Error::BlankClass);
            }
        }

        // Hyprland 0.54 and newer refuse `resizewindowpixel`/`movewindowpixel`
        // for a window in any fullscreen mode (maximized included) with
        // "Window is fullscreen", and the layout owns the geometry anyway. A
        // client that asked to be maximized therefore keeps its mode instead of
        // being forced back into the remembered rectangle.
        if self.is_window_fullscreen(&address).await? {
            debug!(
                "Window {address} is fullscreen; keeping its geometry instead of restoring {:?} at {:?}",
                size, at
            );
            return Ok(Placement::Unchanged);
        }

        // `togglefloating` flips the floating state, so applying it to a window
        // that is already floating tiles it again. The idempotent `setfloating`
        // dispatcher is used instead; the crate has no variant for it yet.
        let identifier = format!("address:{address}");
        Dispatch::call_async(DispatchType::Custom("setfloating", &identifier)).await?;

        // Resize first: `resizewindowpixel` keeps the window centre fixed, so a
        // resize after the move would shift the window away from `at`.
        Dispatch::call_async(DispatchType::ResizeWindowPixel(
            hyprland::dispatch::Position::Exact(size.0, size.1),
            WindowIdentifier::Address(address.clone()),
        ))
        .await?;

        Dispatch::call_async(DispatchType::MoveWindowPixel(
            hyprland::dispatch::Position::Exact(at.0, at.1),
            WindowIdentifier::Address(address.clone()),
        ))
        .await?;

        Ok(Placement::Moved)
    }

    /// Reports whether Hyprland has the window in a fullscreen mode other than
    /// `None`: maximized, fullscreen, or both. The dispatchers that apply
    /// nest's remembered geometry refuse to act on such a window.
    async fn is_window_fullscreen(&self, address: &Address) -> Result<bool, Error> {
        let clients = Clients::get_async().await?;
        Ok(clients.iter().any(|client| {
            normalize_address(client.address.clone()) == *address
                && client.fullscreen != FullscreenMode::None
        }))
    }

    pub async fn get_program(&self, class: String) -> Option<Program> {
        let programs = self.programs.0.lock().await;
        programs.get(&class).cloned()
    }

    pub async fn get_programs(&self) -> Vec<Program> {
        let programs = self.programs.0.lock().await;
        let val: Vec<Program> = programs
            .clone()
            .into_iter()
            .map(|val| val.1.clone())
            .collect();
        val
    }

    pub async fn get_mapped_programs(&self) -> HashMap<String, Program> {
        let programs = self.programs.0.lock().await;
        programs.clone()
    }

    pub fn workspace_changed(&self, id: i32) {
        self.current_workspace.store(id, Ordering::Relaxed);
    }

    pub fn current_workspace(&self) -> i32 {
        self.current_workspace.load(Ordering::Relaxed)
    }
}
