//! per window volume thru pactl using every stream from the app and its child processes

use std::collections::HashMap;

/// the playback streams from a windows app and their volume
#[derive(Clone, Debug, Default)]
pub struct Streams {
    pub indices: Vec<u32>,
    /// the first uhh stream volume in percent
    pub percent: Option<u32>,
}

/// whose audio to look for
pub enum Owner {
    /// a process and its children
    Process(i32),
    /// an app by name for x11 apps that all share xwayland-satellites process
    Name(String),
}

/// how long to wait for pactl so a stuck audio server cant freeze us
const PACTL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// find the streams owner is playing
pub fn streams_for(owner: Owner) -> Streams {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let output = std::process::Command::new("pactl")
            .args(["-f", "json", "list", "sink-inputs"])
            .output();
        let _ = send.send(output);
    });
    let Ok(Ok(output)) = receive.recv_timeout(PACTL_TIMEOUT) else {
        tracing::warn!("volume: pactl didn't answer");
        return Streams::default();
    };
    let Ok(inputs) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Streams::default();
    };
    let mut parents = HashMap::new();
    let mut streams = Streams::default();
    for input in inputs.as_array().into_iter().flatten() {
        let properties = &input["properties"];
        let ours = match &owner {
            Owner::Process(pid) => properties["application.process.id"]
                .as_str()
                .and_then(|p| p.parse::<i32>().ok())
                .is_some_and(|stream_pid| descends_from(stream_pid, *pid, &mut parents)),
            Owner::Name(name) => {
                !name.is_empty()
                    && ["application.name", "application.process.binary"]
                        .iter()
                        .filter_map(|key| properties[*key].as_str())
                        .any(|value| value.eq_ignore_ascii_case(name))
            }
        };
        if !ours {
            continue;
        }
        let Some(index) = input["index"].as_u64() else {
            continue;
        };
        streams.indices.push(index as u32);
        if streams.percent.is_none() {
            // the first channels volume like front-left value_percent 80%
            streams.percent = input["volume"]
                .as_object()
                .and_then(|channels| channels.values().next())
                .and_then(|channel| channel["value_percent"].as_str())
                .and_then(|p| p.trim_end_matches('%').trim().parse().ok());
        }
    }
    streams
}

/// set every stream to percent without waiting for pactl
pub fn set_volume(streams: &Streams, percent: u32) {
    for index in &streams.indices {
        let spawned = std::process::Command::new("pactl")
            .args([
                "set-sink-input-volume",
                &index.to_string(),
                &format!("{percent}%"),
            ])
            .spawn();
        match spawned {
            // reap it off the event loop thread
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(err) => tracing::warn!("volume: couldn't run pactl ({err})"),
        }
    }
}

/// whether pid is ancestor or one of its kids
fn descends_from(mut pid: i32, ancestor: i32, parents: &mut HashMap<i32, i32>) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let parent = *parents
            .entry(pid)
            .or_insert_with(|| parent_of(pid).unwrap_or(0));
        pid = parent;
    }
    false
}

/// a process parent from /proc/pid/stat
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // the command name can have spaces and parens so read the fields after the last closing paren
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}
