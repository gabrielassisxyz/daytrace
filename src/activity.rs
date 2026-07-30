#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityKind {
    Window,
    Idle,
    /// The machine was not running at all, rather than running with nobody at it.
    Suspended,
    Unknown,
}

impl ActivityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Idle => "idle",
            Self::Suspended => "suspended",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "window" => Self::Window,
            "idle" => Self::Idle,
            "suspended" => Self::Suspended,
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

    /// A stretch the machine spent powered down.
    ///
    /// A separate kind from idle, not a longer idle: sitting still and being switched off are
    /// the same absence of input but not the same fact about the day, and a report that cannot
    /// tell them apart credits a suspended night to somebody being away from their desk.
    pub fn suspended() -> Self {
        Self {
            kind: ActivityKind::Suspended,
            app_class: None,
            title: Some("Machine suspended".to_string()),
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
