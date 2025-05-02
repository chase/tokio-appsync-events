// SPDX-FileCopyrightText: 2025 Chase Colman
// SPDX-License-Identifier: MPL-2.0

pub fn events(realtime_host: &str) -> String {
    format!("https://{}/event", events_host(realtime_host))
}

pub fn events_realtime(realtime_host: &str) -> String {
    format!("wss://{}/event/realtime", realtime_host)
}

pub fn events_host(realtime_host: &str) -> String {
    realtime_host.replace("appsync-realtime-api", "appsync-api")
}
