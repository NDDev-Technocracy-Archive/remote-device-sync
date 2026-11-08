//! Bounded native workspace intent. Transport and credentials remain caller-owned.
use serde::{Deserialize, Serialize};
mod appearance;
pub use appearance::Appearance;

#[cfg(feature = "viewer")]
pub(crate) mod chrome;
#[cfg(feature = "viewer")]
pub use chrome::DeviceView;

pub const MAX_TABS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabId(u64);

impl TabId {
    pub fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VideoSize {
    Hd,
    #[default]
    FullHd,
    Native,
}

impl VideoSize {
    pub fn height(self) -> u32 {
        match self {
            Self::Hd => 720,
            Self::FullHd => 1080,
            Self::Native => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TabProfile {
    pub video_size: VideoSize,
    pub max_fps: u32,
    pub clipboard: bool,
    /// Local input preference; never expands the remote grant's permission.
    pub interactive: bool,
    pub payload_receipts: bool,
}

impl Default for TabProfile {
    fn default() -> Self {
        Self {
            video_size: VideoSize::FullHd,
            max_fps: 30,
            clipboard: true,
            interactive: true,
            payload_receipts: false,
        }
    }
}

impl TabProfile {
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        if !(1..=240).contains(&self.max_fps) {
            return Err(WorkspaceError::InvalidFrameRate);
        }
        Ok(())
    }
}

/// An opaque caller-owned device key. It does not contain credentials, an
/// endpoint key, or a selected-session shortcut that another tab can redirect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabSpec {
    pub device: String,
    pub label: String,
    pub display: u32,
    #[serde(default)]
    pub profile: TabProfile,
}

impl TabSpec {
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        if self.device.is_empty()
            || self.device.len() > 8192
            || self.device.chars().any(char::is_control)
            || self.label.is_empty()
            || self.label.len() > 256
            || self.label.chars().any(char::is_control)
        {
            return Err(WorkspaceError::InvalidDevice);
        }
        self.profile.validate()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    #[error("Select a device with a valid name")]
    InvalidDevice,
    #[error("Frame rate must be between 1 and 240 FPS")]
    InvalidFrameRate,
    #[error("At most eight displays can be open at once")]
    Capacity,
    #[error("This display is already open with different settings")]
    ConflictingProfile,
    #[error("This tab is no longer open")]
    MissingTab,
    #[error("Workspace identity exhausted; reopen the application")]
    Exhausted,
}

#[derive(Clone, Debug)]
pub struct Tab {
    pub id: TabId,
    pub spec: TabSpec,
    /// Settings/reconnect attempts cannot apply stale completion to a new one.
    pub revision: u64,
}

#[derive(Clone, Default)]
pub struct WorkspaceModel {
    tabs: Vec<Tab>,
    active: Option<TabId>,
    next_id: u64,
}

impl WorkspaceModel {
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active(&self) -> Option<TabId> {
        self.active
    }

    pub fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    /// Returns (identity, newly opened). Duplicate intent activates the existing
    /// tab; it does not allocate a second capture or silently change its profile.
    pub fn open(&mut self, spec: TabSpec) -> Result<(TabId, bool), WorkspaceError> {
        spec.validate()?;
        if let Some(tab) = self
            .tabs
            .iter()
            .find(|tab| tab.spec.device == spec.device && tab.spec.display == spec.display)
        {
            if tab.spec.profile != spec.profile {
                return Err(WorkspaceError::ConflictingProfile);
            }
            self.active = Some(tab.id);
            return Ok((tab.id, false));
        }
        if self.tabs.len() == MAX_TABS {
            return Err(WorkspaceError::Capacity);
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or(WorkspaceError::Exhausted)?;
        let id = TabId(next);
        self.next_id = next;
        self.tabs.push(Tab {
            id,
            spec,
            revision: 0,
        });
        self.active = Some(id);
        Ok((id, true))
    }

    pub fn select(&mut self, id: TabId) -> Result<(), WorkspaceError> {
        self.tab(id).ok_or(WorkspaceError::MissingTab)?;
        self.active = Some(id);
        Ok(())
    }

    pub fn cycle(&mut self, backwards: bool) -> Option<TabId> {
        let index = self
            .tabs
            .iter()
            .position(|tab| Some(tab.id) == self.active)?;
        let offset = if backwards { self.tabs.len() - 1 } else { 1 };
        let id = self.tabs[(index + offset) % self.tabs.len()].id;
        self.active = Some(id);
        Some(id)
    }

    pub fn close(&mut self, id: TabId) -> Result<Tab, WorkspaceError> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(WorkspaceError::MissingTab)?;
        let removed = self.tabs.remove(index);
        if self.active == Some(id) {
            self.active = self
                .tabs
                .get(index)
                .or_else(|| self.tabs.last())
                .map(|tab| tab.id);
        }
        Ok(removed)
    }

    pub fn move_tab(&mut self, id: TabId, position: usize) -> Result<(), WorkspaceError> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == id)
            .ok_or(WorkspaceError::MissingTab)?;
        let tab = self.tabs.remove(index);
        self.tabs.insert(position.min(self.tabs.len()), tab);
        Ok(())
    }

    pub fn configure(&mut self, id: TabId, profile: TabProfile) -> Result<u64, WorkspaceError> {
        profile.validate()?;
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .ok_or(WorkspaceError::MissingTab)?;
        let revision = tab
            .revision
            .checked_add(1)
            .ok_or(WorkspaceError::Exhausted)?;
        tab.spec.profile = profile;
        tab.revision = revision;
        Ok(revision)
    }

    pub fn accepts(&self, id: TabId, revision: u64) -> bool {
        self.tab(id).is_some_and(|tab| tab.revision == revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(device: &str, display: u32) -> TabSpec {
        TabSpec {
            device: device.into(),
            label: device.into(),
            display,
            profile: TabProfile::default(),
        }
    }

    #[test]
    fn devices_and_monitors_keep_independent_identity_through_reorder_and_close() {
        let mut workspace = WorkspaceModel::default();
        let (a, _) = workspace.open(spec("alpha", 0)).unwrap();
        let (b, _) = workspace.open(spec("alpha", 7)).unwrap();
        let (c, _) = workspace.open(spec("beta", 0)).unwrap();
        workspace.move_tab(a, 2).unwrap();
        assert_eq!(workspace.active(), Some(c));
        workspace.close(b).unwrap();
        assert_eq!(workspace.active(), Some(c));
        assert_eq!(workspace.cycle(false), Some(a));
        workspace.close(a).unwrap();
        assert_eq!(workspace.active(), Some(c));
        assert_eq!(workspace.tab(c).unwrap().spec.device, "beta");
        workspace.close(c).unwrap();
        assert_eq!(workspace.active(), None);
        assert_eq!(workspace.cycle(true), None);
    }

    #[test]
    fn duplicate_open_selects_existing_without_reconfiguring_or_bypassing_bounds() {
        let mut workspace = WorkspaceModel::default();
        let (first, _) = workspace.open(spec("alpha", 0)).unwrap();
        for display in 1..MAX_TABS as u32 {
            workspace.open(spec("alpha", display)).unwrap();
        }
        assert_eq!(workspace.open(spec("alpha", 0)), Ok((first, false)));
        assert_eq!(
            workspace.open(spec("beta", 0)),
            Err(WorkspaceError::Capacity)
        );
        let mut changed = spec("alpha", 0);
        changed.profile.max_fps = 60;
        assert_eq!(
            workspace.open(changed),
            Err(WorkspaceError::ConflictingProfile)
        );
        assert_eq!(workspace.tab(first).unwrap().spec.profile.max_fps, 30);
    }

    #[test]
    fn late_results_cannot_reconfigure_another_tab_or_a_reopened_display() {
        let mut workspace = WorkspaceModel::default();
        let (a, _) = workspace.open(spec("alpha", 0)).unwrap();
        let (b, _) = workspace.open(spec("beta", 0)).unwrap();
        let revised = TabProfile {
            max_fps: 60,
            ..Default::default()
        };
        assert_eq!(workspace.configure(a, revised), Ok(1));
        assert!(!workspace.accepts(a, 0));
        assert!(workspace.accepts(b, 0));
        workspace.close(a).unwrap();
        let (new, _) = workspace.open(spec("alpha", 0)).unwrap();
        assert_ne!(a, new);
        assert!(!workspace.accepts(a, 1));
        assert!(workspace.accepts(new, 0));
    }

    #[test]
    fn invalid_settings_fail_before_mutation() {
        let mut workspace = WorkspaceModel::default();
        let (id, _) = workspace.open(spec("alpha", 0)).unwrap();
        for fps in [0, 241, u32::MAX] {
            let profile = TabProfile {
                max_fps: fps,
                ..Default::default()
            };
            assert_eq!(
                workspace.configure(id, profile),
                Err(WorkspaceError::InvalidFrameRate)
            );
            assert!(workspace.accepts(id, 0));
        }
        assert_eq!(
            workspace.open(spec("\n", 0)),
            Err(WorkspaceError::InvalidDevice)
        );
        assert_eq!(workspace.tabs().len(), 1);
    }
}
