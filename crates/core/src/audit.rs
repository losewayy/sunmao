//! Audit ledger — v0.1 minimal: records permission-relevant facts in memory.
//! The seam (`ctx.audit`) exists now so gates can be inserted without
//! restructuring the tool layer later.

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub kind: &'static str,
    pub detail: String,
}

#[derive(Default)]
pub struct AuditLog {
    entries: Vec<AuditEntry>,
}

impl AuditLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, kind: &'static str, detail: impl Into<String>) {
        self.entries.push(AuditEntry {
            kind,
            detail: detail.into(),
        });
    }

    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }
}
