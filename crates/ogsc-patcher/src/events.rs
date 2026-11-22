//! Structured progress and audit-log events emitted during patch execution and rollback.

/// Structured event stream consumed by the CLI and the `egui` Patch Advisor UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchEvent {
    /// Execution began for a selectable [`FixOption`](ogsc_recipes::FixOption).
    FixStarted {
        /// Fix identifier.
        fix_id: String,
        /// Human-readable fix title.
        title: String,
        /// Number of steps scheduled for this fix (filtered by release & Windows build).
        total_steps: usize,
    },

    /// Execution began for an individual [`PatchStep`](ogsc_recipes::PatchStep).
    StepStarted {
        /// Owning fix ID.
        fix_id: String,
        /// Step identifier.
        step_id: String,
        /// Human-readable step name.
        step_name: String,
    },

    /// Downloading an archive or binary patch over HTTPS (or reusing verified local cache).
    DownloadStarted {
        /// Step identifier.
        step_id: String,
        /// HTTPS URL being fetched.
        url: String,
        /// `true` if a SHA-256-verified copy was already present in the download cache.
        from_cache: bool,
    },

    /// Streaming download byte progress notification.
    DownloadProgress {
        /// Step identifier.
        step_id: String,
        /// Cumulative bytes written so far.
        bytes_downloaded: u64,
        /// Total expected bytes from `Content-Length` if provided by the server.
        total_bytes: Option<u64>,
    },

    /// Downloaded payload passed cryptographic SHA-256 verification.
    DownloadVerified {
        /// Step identifier.
        step_id: String,
        /// Verified lowercase SHA-256 digest.
        sha256: String,
        /// Final size in bytes.
        bytes: u64,
    },

    /// Pre-patch original file snapshot was saved in `.ogsc_backup`.
    FileBackedUp {
        /// Owning fix ID.
        fix_id: String,
        /// Game-relative path backed up.
        relative_path: String,
    },

    /// Patch step created or updated a file in the game directory.
    FileWritten {
        /// Owning fix ID.
        fix_id: String,
        /// Game-relative path written.
        relative_path: String,
    },

    /// Compatibility wrapper files and generated configs were deployed.
    WrapperDeployed {
        /// Owning fix ID.
        fix_id: String,
        /// Wrapper slug (`dgvoodoo2`, `dxwrapper`, `wined3d`, `xinput_plus`, `dosbox_x`, `86box`).
        wrapper: String,
        /// Game-relative files placed into the game directory.
        files: Vec<String>,
    },

    /// Windows Registry compatibility layer entry was written.
    RegistryUpdated {
        /// Owning fix ID.
        fix_id: String,
        /// Registry key path (for example `HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers`).
        key_path: String,
        /// Value name (target executable path).
        value_name: String,
        /// Resulting compatibility layer string.
        new_value: String,
    },

    /// CPU affinity / priority launch constraint recorded from a fix step.
    CpuAffinityConfigured {
        /// Owning fix ID.
        fix_id: String,
        /// Human-readable description of the configured affinity policy.
        summary: String,
    },

    /// Step skipped due to release or OS build filtering.
    StepSkipped {
        /// Owning fix ID.
        fix_id: String,
        /// Skipped step ID.
        step_id: String,
        /// Explanation for skipping.
        reason: String,
    },

    /// Individual step finished successfully.
    StepCompleted {
        /// Owning fix ID.
        fix_id: String,
        /// Completed step ID.
        step_id: String,
    },

    /// Entire selectable fix group finished cleanly.
    FixCompleted {
        /// Completed fix ID.
        fix_id: String,
    },

    /// Non-fatal advisory or rollback warning emitted during execution.
    Warning {
        /// Human-readable warning text.
        message: String,
    },
}

/// Receiver interface for [`PatchEvent`] progress and log notifications.
pub trait EventSink {
    /// Delivers a single [`PatchEvent`].
    fn emit(&mut self, event: PatchEvent);
}

impl<F: FnMut(PatchEvent)> EventSink for F {
    fn emit(&mut self, event: PatchEvent) {
        self(event);
    }
}

/// Discard sink used when callers do not inspect structured events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&mut self, _event: PatchEvent) {}
}

/// In-memory collecting sink convenient for tests and UI log buffers.
#[derive(Debug, Default, Clone)]
pub struct VecEventSink {
    /// Ordered events received during execution.
    pub events: Vec<PatchEvent>,
}

impl EventSink for VecEventSink {
    fn emit(&mut self, event: PatchEvent) {
        self.events.push(event);
    }
}
