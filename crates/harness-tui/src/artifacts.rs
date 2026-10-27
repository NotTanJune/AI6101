//! Durable, per-adaptation evidence. Tool KB and event files keep the shared contracts.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use harness_core::clock::SystemClock;
use harness_core::events::{
    AdaptationOutcomeEvent, AdaptationStatus, EventBody, EventEmitter, EventSource, JsonlFileSink,
};
use harness_core::redact::Redactor;
use harness_core::strategy::kb_hash;
use harness_core::{Strategy, ToolKb};
use serde_json::{Value, json};

static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

pub(crate) struct AdaptationJournal {
    pub(crate) emitter: EventEmitter,
    dir: PathBuf,
    redactor: Redactor,
    strategy: Strategy,
    prior_hash: String,
}

impl AdaptationJournal {
    pub(crate) fn create(
        session_dir: &Path,
        strategy: Strategy,
        source: &ToolKb,
        target: &ToolKb,
        redactor: Redactor,
    ) -> Result<Self, String> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let run_id = format!(
            "drift-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_RUN.fetch_add(1, Ordering::Relaxed)
        );
        let parent = session_dir.join("adaptations");
        fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        let dir = parent.join(&run_id);
        fs::create_dir(&dir).map_err(|e| e.to_string())?;
        let sink = JsonlFileSink::open(dir.join("events.jsonl")).map_err(|e| e.to_string())?;
        let journal = Self {
            emitter: EventEmitter::new(
                run_id,
                EventSource::Rust,
                Arc::new(SystemClock),
                redactor.clone(),
                Box::new(sink),
            ),
            dir,
            redactor,
            strategy,
            prior_hash: kb_hash(source),
        };
        journal.write_kb("source.json", source)?;
        journal.write_kb("target.json", target)?;
        journal.state("running", None, None)?;
        Ok(journal)
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        self.redactor.redact_str(text)
    }

    /// Refuse to export credentials rather than silently changing the KB and its hash.
    pub(crate) fn write_kb(&self, name: &str, kb: &ToolKb) -> Result<(), String> {
        let value = serde_json::to_value(kb).map_err(|e| e.to_string())?;
        let mut redacted = value.clone();
        self.redactor.redact_json(&mut redacted);
        if redacted != value {
            return Err("Tool KB contains credential-like content; export refused".to_owned());
        }
        let path = self.dir.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| e.to_string())?;
        let written = file
            .write_all(&kb.canonical_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string());
        drop(file);
        if written.is_err() {
            // Only clean up a file that this call successfully created.
            let _ = fs::remove_file(&path);
        }
        written
    }

    pub(crate) fn state(
        &self,
        state: &str,
        candidate: Option<&ToolKb>,
        reason: Option<&str>,
    ) -> Result<(), String> {
        let mut value = json!({
            "state": state,
            "strategy": self.strategy,
            "run_id": self.emitter.run_id(),
            "prior_hash": self.prior_hash,
            "candidate_hash": candidate.map(kb_hash),
            "reason": reason,
        });
        self.redactor.redact_json(&mut value);
        // A crash can leave the temporary file, but never a truncated status file.
        let temporary = self.dir.join("state.tmp");
        fs::write(&temporary, value.to_string()).map_err(|e| e.to_string())?;
        fs::rename(&temporary, self.dir.join("state.json")).map_err(|e| e.to_string())
    }

    pub(crate) fn outcome(
        &mut self,
        status: AdaptationStatus,
        reason: &str,
        candidate: &ToolKb,
        changed_lineage_ids: Vec<String>,
    ) -> Result<(), String> {
        self.emitter
            .emit(
                Some(self.strategy),
                EventBody::AdaptationOutcome(AdaptationOutcomeEvent {
                    status,
                    reason: reason.to_owned(),
                    candidate_hash: Some(kb_hash(candidate)),
                    prior_hash: self.prior_hash.clone(),
                    changed_lineage_ids,
                }),
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub(crate) fn metadata(&self, mut value: Value) -> Result<(), String> {
        self.redactor.redact_json(&mut value);
        fs::write(self.dir.join("settings.json"), value.to_string()).map_err(|e| e.to_string())
    }
}
