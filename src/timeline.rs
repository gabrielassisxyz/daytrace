use crate::activity::{ActivityKind, TimelineSegment};
use chrono::{Local, TimeZone};

pub fn today_bounds(now: i64) -> Result<(i64, i64), String> {
    let now_local = Local
        .timestamp_opt(now, 0)
        .single()
        .ok_or_else(|| "current timestamp is outside supported range".to_string())?;
    let start_naive = now_local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| "failed to build local day start".to_string())?;
    let next_start_naive = now_local
        .date_naive()
        .succ_opt()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .ok_or_else(|| "failed to build next local day start".to_string())?;
    let start_local = Local
        .from_local_datetime(&start_naive)
        .earliest()
        .ok_or_else(|| "local day start does not exist".to_string())?;
    let next_start_local = Local
        .from_local_datetime(&next_start_naive)
        .earliest()
        .ok_or_else(|| "next local day start does not exist".to_string())?;
    Ok((start_local.timestamp(), next_start_local.timestamp()))
}

pub fn render_today(segments: &[TimelineSegment], now: i64) -> Result<String, String> {
    let (start, _) = today_bounds(now)?;
    let date = Local
        .timestamp_opt(start, 0)
        .single()
        .ok_or_else(|| "day start timestamp is outside supported range".to_string())?
        .format("%Y-%m-%d");

    if segments.is_empty() {
        return Ok("No activity events recorded for today.\n".to_string());
    }

    let mut output = format!("Timeline for {date}\n");
    for segment in segments {
        output.push_str(&format_segment(segment)?);
        output.push('\n');
    }
    Ok(output)
}

pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn format_segment(segment: &TimelineSegment) -> Result<String, String> {
    let started = format_clock(segment.started_at)?;
    let ended = format_clock(segment.ended_at)?;
    let duration = format_duration(segment.ended_at.saturating_sub(segment.started_at));
    let label = match segment.snapshot.kind {
        ActivityKind::Idle => "AFK".to_string(),
        ActivityKind::Window => {
            let app = segment
                .snapshot
                .app_class
                .as_deref()
                .unwrap_or("unknown app");
            let title = segment.snapshot.title.as_deref().unwrap_or("untitled");
            format!("{app} - {title}")
        }
        ActivityKind::Unknown => "Unknown".to_string(),
    };
    let location = match (&segment.snapshot.workspace, segment.snapshot.monitor) {
        (Some(workspace), Some(monitor)) => format!(" workspace {workspace}, monitor {monitor}"),
        (Some(workspace), None) => format!(" workspace {workspace}"),
        (None, Some(monitor)) => format!(" monitor {monitor}"),
        (None, None) => String::new(),
    };

    Ok(format!(
        "{started}-{ended}  {duration:<6}  {label}{location}"
    ))
}

fn format_clock(timestamp: i64) -> Result<String, String> {
    Ok(Local
        .timestamp_opt(timestamp, 0)
        .single()
        .ok_or_else(|| "timeline timestamp is outside supported range".to_string())?
        .format("%H:%M")
        .to_string())
}

fn format_duration(seconds: i64) -> String {
    let minutes = (seconds.max(0) + 30) / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    let remaining = minutes % 60;
    if remaining == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{remaining:02}m")
    }
}
