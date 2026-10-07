//! The live fan-out frame and its bus — split from the registry file so a
//! subscriber-side serialization decision has a home that isn't the
//! session table.
use tokio::sync::broadcast;

/// One fan-out frame: the structured `Value` for consumers that inspect
/// fields (IM lanes, tests), plus the wire text serialized ONCE at emit —
/// a ws subscriber used to `to_string` the same frame all over again.
#[derive(Debug)]
pub struct LiveFrame {
    pub value: serde_json::Value,
    pub text: std::sync::Arc<str>,
}

impl LiveFrame {
    pub fn new(v: serde_json::Value) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            text: v.to_string().into(),
            value: v,
        })
    }
}

/// Thin bus wrapper so `live.send(value)` keeps its call signature while
/// the serialize-per-subscriber cost moves to the single emit point.
#[derive(Clone)]
pub struct LiveBus(pub broadcast::Sender<std::sync::Arc<LiveFrame>>);

impl LiveBus {
    pub fn send(&self, v: serde_json::Value) {
        let _ = self.0.send(LiveFrame::new(v));
    }
    pub fn subscribe(&self) -> broadcast::Receiver<std::sync::Arc<LiveFrame>> {
        self.0.subscribe()
    }
}
