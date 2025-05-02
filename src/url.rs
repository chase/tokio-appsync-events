pub fn events(app_id: &str, region: &str) -> String {
    format!("https://{}/event", events_host(app_id, region))
}

pub fn events_host(app_id: &str, region: &str) -> String {
    format!("{}.appsync-api.{}.amazonaws.com", app_id, region)
}

pub fn events_realtime(app_id: &str, region: &str) -> String {
    format!("wss://{}.appsync-realtime-api.{}.amazonaws.com/event/realtime", app_id, region)
}
