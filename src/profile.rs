use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

pub const MAX_DPI_POINTS: usize = 5;
pub const MIN_DPI: u16 = 100;
pub const MAX_DPI: u16 = 25_600;
pub const REPORT_RATES: [u32; 7] = [125, 250, 500, 1_000, 2_000, 4_000, 8_000];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProfileLibrary {
    pub profiles: Vec<Profile>,
    pub selected: usize,
}

impl Default for ProfileLibrary {
    fn default() -> Self {
        Self {
            profiles: vec![Profile::default()],
            selected: 0,
        }
    }
}

impl ProfileLibrary {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(AppError::Other(format!(
                    "cannot read profile library {}: {error}",
                    path.display()
                )));
            }
        };
        let mut library: Self = serde_json::from_str(&text)
            .map_err(|error| AppError::Other(format!("cannot decode profile library: {error}")))?;
        library.normalize();
        Ok(library)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                AppError::Other(format!(
                    "cannot create profile directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let temporary = temporary_path(path);
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| AppError::Other(format!("cannot encode profiles: {error}")))?;
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)
                .map_err(|error| {
                    AppError::Other(format!("cannot create {}: {error}", temporary.display()))
                })?;
            file.write_all(text.as_bytes())
                .map_err(|error| AppError::Other(format!("cannot write {}: {error}", temporary.display())))?;
            file.sync_all()
                .map_err(|error| AppError::Other(format!("cannot sync {}: {error}", temporary.display())))?;
            fs::rename(&temporary, path)
                .map_err(|error| AppError::Other(format!("cannot save {}: {error}", path.display())))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    pub fn selected(&self) -> &Profile {
        &self.profiles[self.selected]
    }

    pub fn selected_mut(&mut self) -> &mut Profile {
        &mut self.profiles[self.selected]
    }

    pub fn add(&mut self) {
        let number = self.profiles.len() + 1;
        let profile = Profile {
            name: format!("Profile {number}"),
            ..Profile::default()
        };
        self.profiles.push(profile);
        self.selected = self.profiles.len() - 1;
    }

    pub fn duplicate_selected(&mut self) {
        let mut profile = self.selected().clone();
        profile.name = format!("{} Copy", profile.name);
        self.profiles.push(profile);
        self.selected = self.profiles.len() - 1;
    }

    pub fn remove_selected(&mut self) {
        if self.profiles.len() <= 1 {
            return;
        }
        self.profiles.remove(self.selected);
        self.selected = self.selected.min(self.profiles.len() - 1);
    }

    fn normalize(&mut self) {
        if self.profiles.is_empty() {
            self.profiles.push(Profile::default());
        }
        for profile in &mut self.profiles {
            profile.normalize();
        }
        self.selected = self.selected.min(self.profiles.len() - 1);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub dpi_points: Vec<u16>,
    pub active_dpi: usize,
    pub shift_dpi: Option<usize>,
    pub report_rate: u32,
    pub bindings: Vec<ButtonBinding>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "Desktop Default".to_owned(),
            dpi_points: vec![400, 800, 1_600, 3_200],
            active_dpi: 1,
            shift_dpi: Some(0),
            report_rate: 1_000,
            bindings: MouseButton::ALL
                .into_iter()
                .map(|button| ButtonBinding {
                    button,
                    action: button.default_action(),
                })
                .collect(),
        }
    }
}

impl Profile {
    pub fn active_dpi_value(&self) -> u16 {
        self.dpi_points[self.active_dpi]
    }

    pub fn action(&self, button: MouseButton) -> &ButtonAction {
        self.bindings
            .iter()
            .find(|binding| binding.button == button)
            .map(|binding| &binding.action)
            .unwrap_or(&ButtonAction::Disabled)
    }

    pub fn assign(&mut self, button: MouseButton, action: ButtonAction) {
        if let Some(binding) = self.bindings.iter_mut().find(|binding| binding.button == button) {
            binding.action = action;
        } else {
            self.bindings.push(ButtonBinding { button, action });
        }
    }

    pub fn add_dpi_point(&mut self, value: u16) -> bool {
        if self.dpi_points.len() >= MAX_DPI_POINTS {
            return false;
        }
        self.dpi_points.push(value.max(MIN_DPI));
        self.dpi_points.sort_unstable();
        self.dpi_points.dedup();
        true
    }

    pub fn select_dpi_value(&mut self, value: u16) {
        let value = value.max(MIN_DPI);
        let shift_value = self.shift_dpi.map(|index| self.dpi_points[index]);
        if !self.dpi_points.contains(&value) {
            if self.dpi_points.len() < MAX_DPI_POINTS {
                self.dpi_points.push(value);
            } else {
                self.dpi_points[self.active_dpi] = value;
            }
            self.dpi_points.sort_unstable();
            self.dpi_points.dedup();
        }
        self.active_dpi = self
            .dpi_points
            .iter()
            .position(|candidate| *candidate == value)
            .unwrap_or(0);
        self.shift_dpi =
            shift_value.and_then(|shift| self.dpi_points.iter().position(|candidate| *candidate == shift));
    }

    pub fn remove_dpi_point(&mut self, index: usize) -> bool {
        if self.dpi_points.len() <= 1 || index >= self.dpi_points.len() {
            return false;
        }
        self.dpi_points.remove(index);
        self.active_dpi = self.active_dpi.min(self.dpi_points.len() - 1);
        self.shift_dpi = self.shift_dpi.and_then(|shift| match shift.cmp(&index) {
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(shift - 1),
            std::cmp::Ordering::Less => Some(shift),
        });
        true
    }

    fn normalize(&mut self) {
        self.name = self.name.trim().to_owned();
        if self.name.is_empty() {
            self.name = "Unnamed Profile".to_owned();
        }
        self.dpi_points.retain(|value| *value >= MIN_DPI);
        self.dpi_points.sort_unstable();
        self.dpi_points.dedup();
        self.dpi_points.truncate(MAX_DPI_POINTS);
        if self.dpi_points.is_empty() {
            self.dpi_points.push(800);
        }
        self.active_dpi = self.active_dpi.min(self.dpi_points.len() - 1);
        self.shift_dpi = self.shift_dpi.filter(|index| *index < self.dpi_points.len());
        if !REPORT_RATES.contains(&self.report_rate) {
            self.report_rate = 1_000;
        }
        for button in MouseButton::ALL {
            if !self.bindings.iter().any(|binding| binding.button == button) {
                self.bindings.push(ButtonBinding {
                    button,
                    action: button.default_action(),
                });
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ButtonBinding {
    pub button: MouseButton,
    pub action: ButtonAction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Primary,
    Secondary,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    pub const ALL: [Self; 5] = [
        Self::Primary,
        Self::Secondary,
        Self::Middle,
        Self::Back,
        Self::Forward,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Secondary => "Secondary",
            Self::Middle => "Middle",
            Self::Back => "Back",
            Self::Forward => "Forward",
        }
    }

    fn default_action(self) -> ButtonAction {
        match self {
            Self::Primary => ButtonAction::PrimaryClick,
            Self::Secondary => ButtonAction::SecondaryClick,
            Self::Middle => ButtonAction::MiddleClick,
            Self::Back => ButtonAction::Back,
            Self::Forward => ButtonAction::Forward,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ButtonAction {
    PrimaryClick,
    SecondaryClick,
    MiddleClick,
    Back,
    Forward,
    DpiUp,
    DpiDown,
    DpiCycle,
    DpiShift,
    Keystroke(String),
    Macro(String),
    OnboardRaw(String),
    Disabled,
}

impl ButtonAction {
    pub fn label(&self) -> String {
        match self {
            Self::PrimaryClick => "Primary Click".to_owned(),
            Self::SecondaryClick => "Secondary Click".to_owned(),
            Self::MiddleClick => "Middle Click".to_owned(),
            Self::Back => "Back".to_owned(),
            Self::Forward => "Forward".to_owned(),
            Self::DpiUp => "DPI Up".to_owned(),
            Self::DpiDown => "DPI Down".to_owned(),
            Self::DpiCycle => "DPI Cycle".to_owned(),
            Self::DpiShift => "DPI Shift".to_owned(),
            Self::Keystroke(value) => format!("Key {value}"),
            Self::Macro(value) => format!("Macro {value}"),
            Self::OnboardRaw(value) => format!("Onboard {value}"),
            Self::Disabled => "Disabled".to_owned(),
        }
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_profiles() {
        let path =
            std::env::temp_dir().join(format!("open-mouse-memory-profiles-{}.json", std::process::id()));
        let mut library = ProfileLibrary::default();
        library.add();
        library.selected_mut().name = "Gaming".to_owned();
        library
            .selected_mut()
            .assign(MouseButton::Back, ButtonAction::DpiShift);
        library.save(&path).unwrap();
        let loaded = ProfileLibrary::load(&path).unwrap();
        let _ = fs::remove_file(path);
        assert_eq!(loaded.profiles.len(), 2);
        assert_eq!(loaded.selected().name, "Gaming");
        assert_eq!(
            loaded.selected().action(MouseButton::Back),
            &ButtonAction::DpiShift
        );
    }

    #[test]
    fn maintains_valid_dpi_indexes() {
        let mut profile = Profile {
            active_dpi: 3,
            shift_dpi: Some(2),
            ..Profile::default()
        };
        assert!(profile.remove_dpi_point(2));
        assert_eq!(profile.active_dpi, 2);
        assert_eq!(profile.shift_dpi, None);
    }

    #[test]
    fn loads_device_dpi_above_the_fallback_range() {
        let mut profile = Profile::default();
        profile.select_dpi_value(44_000);
        assert_eq!(profile.active_dpi_value(), 44_000);
        assert!(profile.dpi_points.contains(&44_000));
    }
}
