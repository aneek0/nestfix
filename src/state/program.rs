use std::{fmt::Display, str::FromStr};

use crate::state::{Address, FloatingWindow, ParseError, Workspace};

/// A move nest dispatched that Hyprland has not reported back yet.
#[derive(Clone, Debug)]
pub struct PendingMove {
    pub address: Address,
    pub workspace_id: i32,
}

#[derive(Clone, Debug)]
pub struct Program {
    pub class: String,
    pub workspaces: Vec<Workspace>,
    pub floating_window: Option<FloatingWindow>,
    /// Moves nest dispatched that are still awaiting their `movewindow` event.
    /// Matching by address is what keeps nest from learning its own moves: a
    /// single boolean cannot, because Hyprland sends no event for a dispatch
    /// that targets the workspace the window already sits on, and the stuck
    /// flag would then swallow the next genuine move.
    pub pending_moves: Vec<PendingMove>,
}

impl Display for Program {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:[", self.class)?;
        for (i, workspace) in self.workspaces.iter().enumerate() {
            write!(f, "{}", workspace)?;
            if i != self.workspaces.len() - 1 {
                write!(f, ",")?;
            }
        }
        match &self.floating_window {
            Some(floating_window) => write!(f, "]&[{}]", floating_window),
            None => write!(f, "]&[]"),
        }
    }
}

impl FromStr for Program {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let split: Vec<&str> = s.split(':').collect();
        let class = split.first().unwrap_or(&"0");
        let data: Vec<&str> = split.last().unwrap_or(&"0").split('&').collect();

        let workspaces_str: Vec<&str> = data
            .first()
            .unwrap_or(&"0")
            .trim()
            .trim_matches(['[', ']'])
            .split(',')
            .collect();

        let mut workspaces: Vec<Workspace> = Vec::with_capacity(workspaces_str.len());
        for workspace_str in workspaces_str.iter() {
            let workspace = Workspace::from_str(workspace_str)?;
            workspaces.push(workspace);
        }

        let window_str = data.last().unwrap_or(&"0").trim().trim_matches(['[', ']']);

        let floating_window = FloatingWindow::from_str(window_str).ok();

        Ok(Program {
            class: class.to_string(),
            workspaces,
            floating_window,
            pending_moves: Vec::new(),
        })
    }
}
