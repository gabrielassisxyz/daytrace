#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityKind {
    Window,
    Idle,
    Unknown,
}

impl ActivityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "window" => Self::Window,
            "idle" => Self::Idle,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivitySnapshot {
    pub kind: ActivityKind,
    pub app_class: Option<String>,
    pub title: Option<String>,
    pub workspace: Option<String>,
    pub monitor: Option<i64>,
}

impl ActivitySnapshot {
    pub fn idle() -> Self {
        Self {
            kind: ActivityKind::Idle,
            app_class: None,
            title: Some("AFK".to_string()),
            workspace: None,
            monitor: None,
        }
    }

    pub fn unknown() -> Self {
        Self {
            kind: ActivityKind::Unknown,
            app_class: None,
            title: Some("No active window".to_string()),
            workspace: None,
            monitor: None,
        }
    }

    pub fn window(
        app_class: Option<String>,
        title: Option<String>,
        workspace: Option<String>,
        monitor: Option<i64>,
    ) -> Self {
        Self {
            kind: ActivityKind::Window,
            app_class,
            title,
            workspace,
            monitor,
        }
    }

    pub fn is_recordable(&self) -> bool {
        self.kind != ActivityKind::Unknown
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineSegment {
    pub started_at: i64,
    pub ended_at: i64,
    pub snapshot: ActivitySnapshot,
}
