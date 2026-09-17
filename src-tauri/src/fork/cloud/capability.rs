//! The two capabilities routed through the credential pool.
//!
//! An explicit enum rather than a string, so the compiler enforces the central
//! rule: rotation state is keyed by (credential, capability), never by
//! credential alone. Providers meter these in independent buckets, so a key
//! exhausted for chat still has its whole transcription allowance.

use serde::{Deserialize, Serialize};
use specta::Type;
use std::fmt;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Type)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Stt,
    PostProcess,
}

impl Capability {
    pub const ALL: [Capability; 2] = [Capability::Stt, Capability::PostProcess];

    /// Persisted key. Deliberately not derived from `Debug`, which would drift
    /// if the enum were renamed.
    pub fn as_key(self) -> &'static str {
        match self {
            Capability::Stt => "stt",
            Capability::PostProcess => "post_process",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sqlite_key_matches_the_wire_format() {
        // The same string identifies a capability in the state database and in
        // the settings JSON. If they drifted, rotation state would be written
        // under one name and read under another.
        for capability in Capability::ALL {
            assert_eq!(
                serde_json::to_string(&capability).unwrap(),
                format!("\"{}\"", capability.as_key())
            );
        }
    }
}
