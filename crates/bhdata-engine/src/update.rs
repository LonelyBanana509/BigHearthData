//! Minor-release updates (design section 3, "Engine manager").
//!
//! Once a day, inside the one-hour maintenance window, the engine manager asks the helper to apply
//! the package update, then restarts each instance whose server is older than the installed
//! programs, one instance at a time. A major version change is never applied here: it is an
//! explicit admin action, and a different major version in the installed programs is ignored.
//!
//! Each day gets at most one attempt, even a failed one, so a broken update does not hammer the
//! package system every tick of the window. The next attempt is the next day's window.
//!
//! [`Updater::tick`] is a plain function of the time and an [`UpdateOps`], so tests need no clock,
//! no packages and no PostgreSQL. [`SystemUpdateOps`] is the real one.

use std::path::PathBuf;

use bhdata_types::Timestamp;

use crate::error::EngineError;
use crate::install::{self, HelperAction};
use crate::instance::{self, InstanceSpec, PgVersion};
use crate::supervise;

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 24 * HOUR_MS;

/// Whether `now` is inside the hour that starts at `hour_utc`.
pub fn in_window(now: Timestamp, hour_utc: u8) -> bool {
    (now.as_unix_millis() / HOUR_MS) % 24 == u64::from(hour_utc)
}

/// The UTC day number of `now` (days since the epoch).
pub fn day_of(now: Timestamp) -> u64 {
    now.as_unix_millis() / DAY_MS
}

/// Whether a server started as `started` should be restarted to pick up `installed`: same major,
/// newer minor. Pre-releases (no minor number) are never updated automatically.
pub fn needs_restart(started: PgVersion, installed: PgVersion) -> bool {
    started.major == installed.major
        && matches!((started.minor, installed.minor), (Some(a), Some(b)) if b > a)
}

/// What the updater needs from the system.
pub trait UpdateOps {
    /// Applies the package update (the helper's `update` action).
    fn run_update(&self) -> Result<(), EngineError>;
    /// The version of the installed server programs.
    fn installed_version(&self) -> Result<PgVersion, EngineError>;
    /// Each instance's id and the version it was last started with, if known.
    fn instances(&self) -> Vec<(String, Option<PgVersion>)>;
    fn restart(&self, id: &str) -> Result<(), EngineError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    OutsideWindow,
    AlreadyAttemptedToday,
    /// The update ran and no instance needed a restart.
    UpToDate,
    /// The update ran and these instances were restarted, in order.
    Restarted(Vec<String>),
}

#[derive(Debug)]
pub struct Updater {
    hour_utc: u8,
    last_attempt_day: Option<u64>,
}

impl Updater {
    pub fn new(hour_utc: u8) -> Updater {
        Updater {
            hour_utc,
            last_attempt_day: None,
        }
    }

    /// Call this every minute or so. Does nothing outside the window or after today's attempt.
    pub fn tick(&mut self, now: Timestamp, ops: &dyn UpdateOps) -> Result<Report, EngineError> {
        if !in_window(now, self.hour_utc) {
            return Ok(Report::OutsideWindow);
        }
        let today = day_of(now);
        if self.last_attempt_day == Some(today) {
            return Ok(Report::AlreadyAttemptedToday);
        }
        self.last_attempt_day = Some(today);

        ops.run_update()?;
        let installed = ops.installed_version()?;
        let mut restarted = Vec::new();
        for (id, started) in ops.instances() {
            if started.is_some_and(|s| needs_restart(s, installed)) {
                ops.restart(&id)?;
                restarted.push(id);
            }
        }
        Ok(if restarted.is_empty() {
            Report::UpToDate
        } else {
            Report::Restarted(restarted)
        })
    }
}

/// The real thing: the helper for packages, `pg_ctl` for the restarts.
pub struct SystemUpdateOps {
    pub helper: PathBuf,
    pub specs: Vec<InstanceSpec>,
}

impl UpdateOps for SystemUpdateOps {
    fn run_update(&self) -> Result<(), EngineError> {
        install::run_helper(&self.helper, HelperAction::Update)
    }

    fn installed_version(&self) -> Result<PgVersion, EngineError> {
        let spec = self
            .specs
            .first()
            .ok_or_else(|| EngineError::invalid("instances", "there are none to update"))?;
        instance::server_version(spec)
    }

    fn instances(&self) -> Vec<(String, Option<PgVersion>)> {
        self.specs
            .iter()
            .map(|s| (s.id.clone(), supervise::recorded_version(s)))
            .collect()
    }

    fn restart(&self, id: &str) -> Result<(), EngineError> {
        let spec =
            self.specs.iter().find(|s| s.id == id).ok_or_else(|| {
                EngineError::invalid("instance id", format!("unknown instance {id}"))
            })?;
        supervise::restart(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn at(day: u64, hour: u64, minute: u64) -> Timestamp {
        Timestamp::from_unix_millis(day * DAY_MS + hour * HOUR_MS + minute * 60_000)
    }

    fn v(major: u32, minor: Option<u32>) -> PgVersion {
        PgVersion { major, minor }
    }

    struct Fake {
        installed: PgVersion,
        instances: Vec<(String, Option<PgVersion>)>,
        fail_update: bool,
        fail_restart_of: Option<&'static str>,
        log: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(installed: PgVersion, instances: &[(&str, Option<PgVersion>)]) -> Fake {
            Fake {
                installed,
                instances: instances
                    .iter()
                    .map(|(id, v)| (id.to_string(), *v))
                    .collect(),
                fail_update: false,
                fail_restart_of: None,
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl UpdateOps for Fake {
        fn run_update(&self) -> Result<(), EngineError> {
            self.log.borrow_mut().push("update".into());
            if self.fail_update {
                Err(EngineError::Tool {
                    program: "apt",
                    detail: "no network".into(),
                })
            } else {
                Ok(())
            }
        }
        fn installed_version(&self) -> Result<PgVersion, EngineError> {
            Ok(self.installed)
        }
        fn instances(&self) -> Vec<(String, Option<PgVersion>)> {
            self.instances.clone()
        }
        fn restart(&self, id: &str) -> Result<(), EngineError> {
            self.log.borrow_mut().push(format!("restart {id}"));
            if self.fail_restart_of == Some(id) {
                Err(EngineError::Tool {
                    program: "pg_ctl",
                    detail: "stuck".into(),
                })
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn the_window_is_one_utc_hour() {
        assert!(in_window(at(100, 3, 0), 3));
        assert!(in_window(at(100, 3, 59), 3));
        assert!(!in_window(at(100, 4, 0), 3));
        assert!(!in_window(at(100, 2, 59), 3));
        assert!(in_window(at(7, 0, 30), 0));
        assert!(in_window(at(7, 23, 30), 23));
    }

    #[test]
    fn days_count_from_the_epoch() {
        assert_eq!(day_of(Timestamp::EPOCH), 0);
        assert_eq!(day_of(at(5, 23, 59)), 5);
        assert_eq!(day_of(at(6, 0, 0)), 6);
    }

    #[test]
    fn only_a_newer_minor_of_the_same_major_needs_a_restart() {
        assert!(needs_restart(v(17, Some(2)), v(17, Some(3))));
        assert!(!needs_restart(v(17, Some(3)), v(17, Some(3))));
        assert!(
            !needs_restart(v(17, Some(3)), v(17, Some(2))),
            "never downgrade"
        );
        assert!(
            !needs_restart(v(17, Some(9)), v(18, Some(1))),
            "never across majors"
        );
        assert!(!needs_restart(v(17, None), v(17, Some(1))));
        assert!(!needs_restart(v(17, Some(1)), v(17, None)));
    }

    #[test]
    fn nothing_happens_outside_the_window() {
        let ops = Fake::new(v(17, Some(3)), &[("shard0", Some(v(17, Some(2))))]);
        let mut u = Updater::new(3);
        assert_eq!(u.tick(at(10, 12, 0), &ops).unwrap(), Report::OutsideWindow);
        assert!(ops.log.borrow().is_empty());
    }

    #[test]
    fn inside_the_window_it_updates_then_restarts_the_old_instances_in_order() {
        let ops = Fake::new(
            v(17, Some(3)),
            &[
                ("shard0", Some(v(17, Some(2)))),
                ("shard1", Some(v(17, Some(3)))),
                ("shard2", Some(v(17, Some(1)))),
                ("shard3", None),
            ],
        );
        let mut u = Updater::new(3);
        assert_eq!(
            u.tick(at(10, 3, 5), &ops).unwrap(),
            Report::Restarted(vec!["shard0".into(), "shard2".into()])
        );
        assert_eq!(
            *ops.log.borrow(),
            ["update", "restart shard0", "restart shard2"]
        );
    }

    #[test]
    fn an_update_with_nothing_to_restart_says_so() {
        let ops = Fake::new(v(17, Some(3)), &[("shard0", Some(v(17, Some(3))))]);
        let mut u = Updater::new(3);
        assert_eq!(u.tick(at(10, 3, 0), &ops).unwrap(), Report::UpToDate);
        assert_eq!(*ops.log.borrow(), ["update"]);
    }

    #[test]
    fn a_new_major_in_the_packages_restarts_nothing() {
        let ops = Fake::new(v(18, Some(0)), &[("shard0", Some(v(17, Some(5))))]);
        let mut u = Updater::new(3);
        assert_eq!(u.tick(at(10, 3, 0), &ops).unwrap(), Report::UpToDate);
        assert_eq!(*ops.log.borrow(), ["update"]);
    }

    #[test]
    fn one_attempt_per_day_then_again_the_next_day() {
        let ops = Fake::new(v(17, Some(3)), &[]);
        let mut u = Updater::new(3);
        assert_eq!(u.tick(at(10, 3, 0), &ops).unwrap(), Report::UpToDate);
        assert_eq!(
            u.tick(at(10, 3, 30), &ops).unwrap(),
            Report::AlreadyAttemptedToday
        );
        assert_eq!(u.tick(at(11, 3, 0), &ops).unwrap(), Report::UpToDate);
        assert_eq!(*ops.log.borrow(), ["update", "update"]);
    }

    #[test]
    fn a_failed_update_is_an_error_and_is_not_retried_the_same_day() {
        let mut ops = Fake::new(v(17, Some(3)), &[("shard0", Some(v(17, Some(2))))]);
        ops.fail_update = true;
        let mut u = Updater::new(3);
        assert!(u.tick(at(10, 3, 0), &ops).is_err());
        assert_eq!(
            u.tick(at(10, 3, 1), &ops).unwrap(),
            Report::AlreadyAttemptedToday
        );
        assert_eq!(
            *ops.log.borrow(),
            ["update"],
            "no restarts after a failed update"
        );
    }

    #[test]
    fn a_failed_restart_stops_the_rest_for_the_day() {
        let mut ops = Fake::new(
            v(17, Some(3)),
            &[
                ("shard0", Some(v(17, Some(2)))),
                ("shard1", Some(v(17, Some(2)))),
            ],
        );
        ops.fail_restart_of = Some("shard0");
        let mut u = Updater::new(3);
        assert!(u.tick(at(10, 3, 0), &ops).is_err());
        assert_eq!(*ops.log.borrow(), ["update", "restart shard0"]);
    }

    #[test]
    fn system_ops_with_no_instances_cannot_read_a_version() {
        let ops = SystemUpdateOps {
            helper: PathBuf::from("/usr/libexec/bhdata/bhdata-helper"),
            specs: Vec::new(),
        };
        assert!(matches!(
            ops.installed_version(),
            Err(EngineError::Invalid { .. })
        ));
        assert!(ops.instances().is_empty());
        assert!(matches!(
            ops.restart("nope"),
            Err(EngineError::Invalid { .. })
        ));
    }
}
