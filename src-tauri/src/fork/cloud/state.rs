//! Persistent rotation state, keyed by (credential, capability, model).
//!
//! Its own SQLite file, not `AppSettings`: every settings write reserializes
//! the whole object and this mutates on every request, and `salvage_settings`
//! silently drops fields that fail to deserialize. Separate from `history.db`
//! too, so the fork's migration chain cannot collide with upstream's.

use crate::fork::cloud::binding::MAX_COOLDOWN_SECS;
use crate::fork::cloud::{now_ms, Capability};
use anyhow::Result;
use log::{debug, warn};
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// One quota bucket: (credential_id, model).
pub type PairKey = (String, String);

/// Append only. `rusqlite_migration` tracks progress with `user_version`, so
/// editing history would re-run or skip migrations on stores in the field.
static MIGRATIONS: &[M] = &[
    M::up(
        "CREATE TABLE IF NOT EXISTS credential_state (
        credential_id     TEXT    NOT NULL,
        capability        TEXT    NOT NULL,
        last_used_ms      INTEGER,
        cooldown_until_ms INTEGER,
        PRIMARY KEY (credential_id, capability)
    );
    CREATE TABLE IF NOT EXISTS credential_strikes (
        credential_id TEXT    NOT NULL,
        capability    TEXT    NOT NULL,
        struck_at_ms  INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_strikes_pair
        ON credential_strikes (credential_id, capability);
    CREATE TABLE IF NOT EXISTS rotation_cursor (
        capability TEXT    PRIMARY KEY,
        position   INTEGER NOT NULL
    );",
    ),
    // The model joins the key. Providers meter per model, so one key serving
    // llama-3.3-70b and llama-3.1-8b has two independent quotas, and a 429 on one
    // must not pause the other. Recreated rather than altered: this state is
    // cooldowns and recent failures, all of it disposable, and a rebuild is
    // cheaper than backfilling a column that has no correct value for old rows.
    M::up(
        "DROP TABLE IF EXISTS credential_state;
    DROP TABLE IF EXISTS credential_strikes;
    CREATE TABLE credential_state (
        credential_id     TEXT    NOT NULL,
        capability        TEXT    NOT NULL,
        model             TEXT    NOT NULL,
        last_used_ms      INTEGER,
        cooldown_until_ms INTEGER,
        PRIMARY KEY (credential_id, capability, model)
    );
    CREATE TABLE credential_strikes (
        credential_id TEXT    NOT NULL,
        capability    TEXT    NOT NULL,
        model         TEXT    NOT NULL,
        struck_at_ms  INTEGER NOT NULL
    );
    CREATE INDEX idx_strikes_pair
        ON credential_strikes (credential_id, capability, model);",
    ),
];

pub const STATE_DB_FILENAME: &str = "credential_state.db";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairState {
    pub last_used_ms: Option<i64>,
    pub cooldown_until_ms: Option<i64>,
    /// Strikes inside the caller-supplied rolling window.
    pub recent_strikes: u32,
}

impl PairState {
    /// Deadlines are absolute wall-clock, so one written while the clock was
    /// wrong could sit arbitrarily far out. Nothing legitimate exceeds the
    /// maximum cooldown.
    fn effective_deadline(&self, now: i64) -> Option<i64> {
        let until = self.cooldown_until_ms?;
        let ceiling = now.saturating_add((MAX_COOLDOWN_SECS * 1000) as i64);
        if until > ceiling {
            return None;
        }
        Some(until)
    }

    pub fn is_cooling_down(&self, now: i64) -> bool {
        self.effective_deadline(now)
            .is_some_and(|until| until > now)
    }

    pub fn cooldown_remaining(&self, now: i64) -> Option<Duration> {
        let until = self.effective_deadline(now)?;
        if until <= now {
            return None;
        }
        Some(Duration::from_millis((until - now) as u64))
    }
}

/// One mutex-guarded connection rather than a pool: writes are tiny and
/// serializing them removes any chance of two requests interleaving a cursor
/// read and write.
pub struct RotationStateStore {
    connection: Mutex<Connection>,
}

impl RotationStateStore {
    pub fn open_in(app_data_dir: &Path) -> Result<Self> {
        Self::open_at(&app_data_dir.join(STATE_DB_FILENAME))
    }

    pub fn open_at(path: &PathBuf) -> Result<Self> {
        debug!("Opening rotation state database at {:?}", path);
        let mut connection = Connection::open(path)?;
        Self::migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let mut connection = Connection::open_in_memory()?;
        Self::migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn migrate(connection: &mut Connection) -> Result<()> {
        let migrations = Migrations::new(MIGRATIONS.to_vec());
        #[cfg(debug_assertions)]
        migrations
            .validate()
            .expect("fork rotation-state migrations are invalid");
        migrations.to_latest(connection)?;
        Ok(())
    }

    /// Rotation state is an optimisation, not a correctness requirement: if
    /// the database is unavailable the pool must still place calls, so every
    /// accessor degrades to a default rather than propagating.
    fn with_connection<T>(
        &self,
        body: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Option<T> {
        let guard = match self.connection.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("Rotation state mutex was poisoned by a previous panic, recovering");
                poisoned.into_inner()
            }
        };
        match body(&guard) {
            Ok(value) => Some(value),
            Err(error) => {
                warn!("Rotation state query failed: {error}");
                None
            }
        }
    }

    /// Every bucket for one capability, in two prepared queries rather than two
    /// per candidate.
    pub fn states_for(
        &self,
        capability: Capability,
        window: Duration,
    ) -> HashMap<PairKey, PairState> {
        let cutoff = now_ms() - window.as_millis() as i64;

        self.with_connection(|conn| {
            let mut states: HashMap<PairKey, PairState> = HashMap::new();

            let mut statement = conn.prepare_cached(
                "SELECT credential_id, model, last_used_ms, cooldown_until_ms
                   FROM credential_state WHERE capability = ?1",
            )?;
            let rows = statement.query_map(params![capability.as_key()], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    PairState {
                        last_used_ms: row.get(2)?,
                        cooldown_until_ms: row.get(3)?,
                        recent_strikes: 0,
                    },
                ))
            })?;
            for row in rows {
                let (key, state) = row?;
                states.insert(key, state);
            }

            // Separate rather than joined: a bucket can hold strikes before it
            // holds a state row, and SQLite has no full outer join.
            let mut statement = conn.prepare_cached(
                "SELECT credential_id, model, COUNT(*) FROM credential_strikes
                  WHERE capability = ?1 AND struck_at_ms >= ?2
                  GROUP BY credential_id, model",
            )?;
            let rows = statement.query_map(params![capability.as_key(), cutoff], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    row.get::<_, i64>(2)? as u32,
                ))
            })?;
            for row in rows {
                let (key, count) = row?;
                states.entry(key).or_default().recent_strikes = count;
            }

            Ok(states)
        })
        .unwrap_or_default()
    }

    /// The latest cooldown deadline per credential, across every capability and
    /// model.
    ///
    /// Only consulted for credentials whose quota is shared. For those, one
    /// bucket running out means the account has, so sending the request anyway
    /// spends a round-trip on an answer already known to be 429.
    pub fn shared_cooldowns(&self) -> HashMap<String, i64> {
        self.with_connection(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT credential_id, MAX(cooldown_until_ms) FROM credential_state
                  WHERE cooldown_until_ms IS NOT NULL
                  GROUP BY credential_id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;

            let mut deadlines = HashMap::new();
            for row in rows {
                let (credential_id, until) = row?;
                deadlines.insert(credential_id, until);
            }
            Ok(deadlines)
        })
        .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn pair_state(
        &self,
        credential_id: &str,
        capability: Capability,
        model: &str,
        window: Duration,
    ) -> PairState {
        self.states_for(capability, window)
            .remove(&(credential_id.to_string(), model.to_string()))
            .unwrap_or_default()
    }

    /// Stamped when a credential is selected, before the call, so a request in
    /// flight does not look idle to a concurrent selection.
    pub fn mark_used(&self, credential_id: &str, capability: Capability, model: &str) {
        let now = now_ms();
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO credential_state (credential_id, capability, model, last_used_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(credential_id, capability, model)
                 DO UPDATE SET last_used_ms = excluded.last_used_ms",
                params![credential_id, capability.as_key(), model, now],
            )
        });
    }

    /// Records one strike and returns how many now fall inside `window`.
    pub fn record_strike(
        &self,
        credential_id: &str,
        capability: Capability,
        model: &str,
        window: Duration,
    ) -> u32 {
        let now = now_ms();
        let cutoff = now - window.as_millis() as i64;

        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO credential_strikes (credential_id, capability, model, struck_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![credential_id, capability.as_key(), model, now],
            )?;
            conn.execute(
                "DELETE FROM credential_strikes
                 WHERE credential_id = ?1 AND capability = ?2 AND model = ?3
                   AND struck_at_ms < ?4",
                params![credential_id, capability.as_key(), model, cutoff],
            )?;
            conn.query_row(
                "SELECT COUNT(*) FROM credential_strikes
                 WHERE credential_id = ?1 AND capability = ?2 AND model = ?3
                   AND struck_at_ms >= ?4",
                params![credential_id, capability.as_key(), model, cutoff],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u32)
        })
        .unwrap_or(0)
    }

    /// Called on any success. Without it a key accrues one strike per network
    /// blip and is eventually benched having never actually failed.
    pub fn clear_strikes(&self, credential_id: &str, capability: Capability, model: &str) {
        self.with_connection(|conn| {
            conn.execute(
                "DELETE FROM credential_strikes
                 WHERE credential_id = ?1 AND capability = ?2 AND model = ?3",
                params![credential_id, capability.as_key(), model],
            )
        });
    }

    pub fn start_cooldown(
        &self,
        credential_id: &str,
        capability: Capability,
        model: &str,
        duration: Duration,
    ) {
        let until = now_ms() + duration.as_millis() as i64;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO credential_state (credential_id, capability, model, cooldown_until_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(credential_id, capability, model)
                 DO UPDATE SET cooldown_until_ms = excluded.cooldown_until_ms",
                params![credential_id, capability.as_key(), model, until],
            )
        });
    }

    /// Lifts one bucket. Also forgets the strikes that caused the cooldown, or
    /// the next failure would immediately re-bench a key the user just told us
    /// they had fixed.
    pub fn clear_cooldown(&self, credential_id: &str, capability: Capability, model: &str) {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE credential_state SET cooldown_until_ms = NULL
                 WHERE credential_id = ?1 AND capability = ?2 AND model = ?3",
                params![credential_id, capability.as_key(), model],
            )?;
            conn.execute(
                "DELETE FROM credential_strikes
                 WHERE credential_id = ?1 AND capability = ?2 AND model = ?3",
                params![credential_id, capability.as_key(), model],
            )
        });
    }

    /// Key-wide reset, for when the secret itself changed.
    pub fn clear_all_cooldowns(&self, credential_id: &str) {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE credential_state SET cooldown_until_ms = NULL WHERE credential_id = ?1",
                params![credential_id],
            )?;
            conn.execute(
                "DELETE FROM credential_strikes WHERE credential_id = ?1",
                params![credential_id],
            )
        });
    }

    /// Called on delete, so a re-added id cannot inherit history.
    pub fn forget_credential(&self, credential_id: &str) {
        self.with_connection(|conn| {
            conn.execute(
                "DELETE FROM credential_state WHERE credential_id = ?1",
                params![credential_id],
            )?;
            conn.execute(
                "DELETE FROM credential_strikes WHERE credential_id = ?1",
                params![credential_id],
            )
        });
    }

    /// Persisted: in-memory only would start every launch at credential one,
    /// and the first key would absorb disproportionate load.
    pub fn cursor(&self, capability: Capability) -> u64 {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT position FROM rotation_cursor WHERE capability = ?1",
                params![capability.as_key()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0).max(0) as u64)
        })
        .unwrap_or(0)
    }

    pub fn set_cursor(&self, capability: Capability, position: u64) {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO rotation_cursor (capability, position) VALUES (?1, ?2)
                 ON CONFLICT(capability) DO UPDATE SET position = excluded.position",
                params![capability.as_key(), position as i64],
            )
        });
    }

    /// Test-only: the window logic is only exercised by genuinely old strikes,
    /// and the production path always stamps "now".
    #[cfg(test)]
    fn record_strike_at(
        &self,
        credential_id: &str,
        capability: Capability,
        model: &str,
        struck_at_ms: i64,
    ) {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO credential_strikes (credential_id, capability, model, struck_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![credential_id, capability.as_key(), model, struck_at_ms],
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(3600);
    const MODEL: &str = "llama-3.3-70b";

    #[test]
    fn state_is_keyed_by_capability_not_by_credential_alone() {
        let store = RotationStateStore::in_memory().unwrap();
        assert_eq!(
            store.pair_state("cred_1", Capability::Stt, MODEL, WINDOW),
            PairState::default()
        );

        store.start_cooldown(
            "cred_1",
            Capability::PostProcess,
            MODEL,
            Duration::from_secs(600),
        );

        let now = now_ms();
        assert!(store
            .pair_state("cred_1", Capability::PostProcess, MODEL, WINDOW)
            .is_cooling_down(now));
        assert!(!store
            .pair_state("cred_1", Capability::Stt, MODEL, WINDOW)
            .is_cooling_down(now));
    }

    #[test]
    fn strikes_are_counted_inside_the_window_and_pruned_outside_it() {
        let store = RotationStateStore::in_memory().unwrap();
        let now = now_ms();
        let a_week = 7 * 24 * 3600 * 1000;

        store.record_strike_at("cred_1", Capability::Stt, MODEL, now - a_week);
        store.record_strike_at("cred_1", Capability::Stt, MODEL, now - a_week * 2);

        // The returned count is what the pool compares against the threshold,
        // so it must already exclude the aged strikes.
        assert_eq!(
            store.record_strike("cred_1", Capability::Stt, MODEL, WINDOW),
            1
        );

        let total: i64 = store
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM credential_strikes", [], |row| {
                    row.get(0)
                })
            })
            .unwrap();
        assert_eq!(total, 1, "aged strikes should not accumulate forever");

        store.clear_strikes("cred_1", Capability::Stt, MODEL);
        assert_eq!(
            store
                .pair_state("cred_1", Capability::Stt, MODEL, WINDOW)
                .recent_strikes,
            0
        );
    }

    #[test]
    fn cooldowns_expire_on_their_own_and_clear_takes_the_strikes_with_them() {
        let store = RotationStateStore::in_memory().unwrap();
        store.start_cooldown("cred_1", Capability::Stt, MODEL, Duration::from_millis(0));
        assert!(!store
            .pair_state("cred_1", Capability::Stt, MODEL, WINDOW)
            .is_cooling_down(now_ms()));

        store.record_strike("cred_1", Capability::Stt, MODEL, WINDOW);
        store.start_cooldown("cred_1", Capability::Stt, MODEL, Duration::from_secs(600));
        store.mark_used("cred_1", Capability::Stt, MODEL);

        let state = store.pair_state("cred_1", Capability::Stt, MODEL, WINDOW);
        assert!(state.is_cooling_down(now_ms()));
        assert!(state.last_used_ms.is_some(), "mark_used must not clear it");
        assert!(state.cooldown_remaining(now_ms()).unwrap() <= Duration::from_secs(600));

        store.clear_cooldown("cred_1", Capability::Stt, MODEL);
        let state = store.pair_state("cred_1", Capability::Stt, MODEL, WINDOW);
        assert!(!state.is_cooling_down(now_ms()));
        assert_eq!(state.recent_strikes, 0);
    }

    #[test]
    fn clearing_one_model_leaves_another_models_pause_in_place() {
        const OTHER: &str = "llama-3.1-8b";
        let store = RotationStateStore::in_memory().unwrap();
        store.start_cooldown("cred_1", Capability::Stt, MODEL, Duration::from_secs(600));
        store.start_cooldown("cred_1", Capability::Stt, OTHER, Duration::from_secs(600));

        store.clear_cooldown("cred_1", Capability::Stt, MODEL);

        assert!(!store
            .pair_state("cred_1", Capability::Stt, MODEL, WINDOW)
            .is_cooling_down(now_ms()));
        assert!(store
            .pair_state("cred_1", Capability::Stt, OTHER, WINDOW)
            .is_cooling_down(now_ms()));

        store.clear_all_cooldowns("cred_1");
        assert!(!store
            .pair_state("cred_1", Capability::Stt, OTHER, WINDOW)
            .is_cooling_down(now_ms()));
    }

    #[test]
    fn one_key_serving_two_models_has_two_independent_quotas() {
        // Providers meter per model, so pausing a key for a rate-limited model
        // must leave the other model on the same key untouched.
        let store = RotationStateStore::in_memory().unwrap();
        let now = now_ms();

        store.start_cooldown(
            "cred_1",
            Capability::PostProcess,
            "llama-3.3-70b",
            Duration::from_secs(600),
        );
        store.record_strike("cred_1", Capability::PostProcess, "llama-3.3-70b", WINDOW);

        let paused = store.pair_state("cred_1", Capability::PostProcess, "llama-3.3-70b", WINDOW);
        assert!(paused.is_cooling_down(now));
        assert_eq!(paused.recent_strikes, 1);

        let other = store.pair_state("cred_1", Capability::PostProcess, "llama-3.1-8b", WINDOW);
        assert!(!other.is_cooling_down(now));
        assert_eq!(other.recent_strikes, 0);
    }

    #[test]
    fn deleting_a_credential_forgets_both_capabilities() {
        let store = RotationStateStore::in_memory().unwrap();
        store.record_strike("cred_1", Capability::Stt, MODEL, WINDOW);
        store.start_cooldown(
            "cred_1",
            Capability::PostProcess,
            MODEL,
            Duration::from_secs(600),
        );

        store.forget_credential("cred_1");

        for capability in Capability::ALL {
            assert_eq!(
                store.pair_state("cred_1", capability, MODEL, WINDOW),
                PairState::default()
            );
        }
    }

    #[test]
    fn cooldown_and_cursor_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_DB_FILENAME);

        {
            let store = RotationStateStore::open_at(&path).unwrap();
            store.start_cooldown("cred_1", Capability::Stt, MODEL, Duration::from_secs(600));
            store.set_cursor(Capability::Stt, 3);
        }

        let reopened = RotationStateStore::open_at(&path).unwrap();
        assert!(reopened
            .pair_state("cred_1", Capability::Stt, MODEL, WINDOW)
            .is_cooling_down(now_ms()));
        assert_eq!(reopened.cursor(Capability::Stt), 3);
        assert_eq!(reopened.cursor(Capability::PostProcess), 0);
    }
}
