use serde::Serialize;

use crate::VERSION;

#[derive(Debug, Serialize)]
pub struct InspectReport {
    pub name: &'static str,
    pub version: &'static str,
    pub schema_version: u8,
    pub capabilities: Capabilities,
    pub snapshot_engines: [&'static str; 4],
    pub rollback_guarantees: [&'static str; 6],
}

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub run: bool,
    pub diff: bool,
    pub undo: bool,
    pub json: bool,
    pub sandbox: bool,
    pub mcp: bool,
}

pub fn inspect_report() -> InspectReport {
    InspectReport {
        name: "txpt",
        version: VERSION,
        schema_version: 1,
        capabilities: Capabilities {
            run: true,
            diff: true,
            undo: true,
            json: true,
            sandbox: false,
            mcp: false,
        },
        snapshot_engines: ["apfs-clonefile", "linux-ficlone", "copy", "record-only"],
        rollback_guarantees: [
            "full",
            "cleanup_only",
            "metadata_partial",
            "conflict",
            "unprotected",
            "unsupported",
        ],
    }
}
