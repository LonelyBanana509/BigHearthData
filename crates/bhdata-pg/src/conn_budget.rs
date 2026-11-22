//! Checks that every pool fits inside the instance's connection limit (spec section 5).
//!
//! `budget = recipe_writers + readers + sql_sessions + admin + reserved_backup +
//! superuser_reserved_connections`, and it must be at most `max_connections`.

use std::fmt;

use bhdata_types::config::PoolConfig;

/// PostgreSQL's `superuser_reserved_connections`, as the engine writes it into the settings.
/// One constant, used by the engine's settings and by this check, so the two cannot differ.
pub const SUPERUSER_RESERVED_CONNECTIONS: u32 = 3;

/// The numbers behind a budget check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// The sum of the pools' caps, reserved backup connections included.
    pub pools: u64,
    /// Connections PostgreSQL keeps for superusers.
    pub superuser_reserved: u64,
    /// `max_connections` of the instance.
    pub max_connections: u64,
}

impl Budget {
    /// Everything that counts against the limit.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.pools + self.superuser_reserved
    }

    /// Connections left over.
    #[must_use]
    pub fn spare(&self) -> u64 {
        self.max_connections.saturating_sub(self.used())
    }
}

impl fmt::Display for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pools use {} + {} reserved = {} of {}",
            self.pools,
            self.superuser_reserved,
            self.used(),
            self.max_connections
        )
    }
}

/// The pools do not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}: lower the pool sizes in [pools] or raise engine.max_connections")]
pub struct OverBudget(pub Budget);

/// Returns the budget when it fits, or the error that names every term.
///
/// # Errors
/// [`OverBudget`] when the pools plus the reserved connections exceed `max_connections`.
pub fn check(
    pools: &PoolConfig,
    max_connections: u32,
    superuser_reserved: u32,
) -> Result<Budget, OverBudget> {
    let budget = Budget {
        pools: pools.total(),
        superuser_reserved: u64::from(superuser_reserved),
        max_connections: u64::from(max_connections),
    };
    if budget.used() <= budget.max_connections {
        Ok(budget)
    } else {
        Err(OverBudget(budget))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn the_defaults_fit_in_a_hundred() {
        let b = check(&PoolConfig::default(), 100, SUPERUSER_RESERVED_CONNECTIONS).unwrap();
        assert_eq!((b.pools, b.used(), b.spare()), (38, 41, 59));
        assert_eq!(b.to_string(), "pools use 38 + 3 reserved = 41 of 100");
    }

    #[test]
    fn exactly_the_limit_is_allowed_and_one_more_is_not() {
        let pools = PoolConfig::default(); // 38
        assert!(check(&pools, 41, 3).is_ok());
        assert!(check(&pools, 40, 3).is_err());
    }

    #[test]
    fn the_error_names_every_term_and_what_to_do() {
        let pools = PoolConfig {
            recipe_writers: 100,
            ..PoolConfig::default()
        };
        let text = check(&pools, 100, 3).unwrap_err().to_string();
        assert!(text.contains("122 + 3 reserved = 125 of 100"), "{text}");
        assert!(
            text.contains("[pools]") && text.contains("engine.max_connections"),
            "{text}"
        );
    }

    #[test]
    fn huge_values_cannot_overflow() {
        let pools = PoolConfig {
            recipe_writers: u32::MAX,
            readers: u32::MAX,
            sql_sessions: u32::MAX,
            admin: u32::MAX,
            reserved_backup: u32::MAX,
        };
        assert!(check(&pools, u32::MAX, u32::MAX).is_err());
    }

    proptest! {
        #[test]
        fn ok_exactly_when_the_sum_fits(
            a in 0u32..500, b in 0u32..500, c in 0u32..500, d in 0u32..500, e in 0u32..500,
            max in 0u32..2_000, reserved in 0u32..50,
        ) {
            let pools = PoolConfig { recipe_writers: a, readers: b, sql_sessions: c, admin: d, reserved_backup: e };
            let sum = u64::from(a) + u64::from(b) + u64::from(c) + u64::from(d) + u64::from(e) + u64::from(reserved);
            prop_assert_eq!(check(&pools, max, reserved).is_ok(), sum <= u64::from(max));
        }
    }
}
