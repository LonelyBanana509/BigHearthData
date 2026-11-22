//! Version counters. The stage 2 ones exist from day one so messages and records can carry them
//! before anything uses them (plan section 1, rule 1).

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! counter {
    ($(#[$meta:meta])* $name:ident($inner:ty), initial = $initial:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name($inner);

        impl $name {
            /// The value a fresh system starts with.
            pub const INITIAL: $name = $name($initial);

            /// Wraps a raw number.
            #[must_use]
            pub const fn new(value: $inner) -> Self {
                $name(value)
            }

            /// The raw number.
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }

            /// The next value, or `None` if the counter is exhausted (it never wraps).
            #[must_use]
            pub const fn next(self) -> Option<Self> {
                match self.0.checked_add(1) {
                    Some(value) => Some($name(value)),
                    None => None,
                }
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::INITIAL
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

counter! {
    /// The version of a registered recipe. A registered version never changes; a change registers
    /// the next one. The first version is 1.
    RecipeVersion(u32), initial = 1
}

counter! {
    /// The version of the shard map. Every request carries the map version it was routed with.
    /// Stage 0 has one map, version 1, with every slot on shard 0.
    MapVersion(u64), initial = 1
}

counter! {
    /// The ownership epoch of a slot. A shard refuses a write for a slot it does not own at the
    /// request's epoch.
    Epoch(u64), initial = 1
}

counter! {
    /// A leadership term for the Transaction DB (stage 2). A shard stores the highest term it has
    /// seen and never accepts a step from a lower one. `INITIAL` (0) means no term has been issued;
    /// the first leader gets `Term::INITIAL.next()`.
    Term(u64), initial = 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_values() {
        assert_eq!(RecipeVersion::default().get(), 1);
        assert_eq!(MapVersion::default().get(), 1);
        assert_eq!(Epoch::default().get(), 1);
        assert_eq!(Term::default().get(), 0);
    }

    #[test]
    fn next_counts_up_and_never_wraps() {
        assert_eq!(Term::INITIAL.next(), Some(Term::new(1)));
        assert_eq!(RecipeVersion::new(7).next(), Some(RecipeVersion::new(8)));
        assert_eq!(RecipeVersion::new(u32::MAX).next(), None);
        assert_eq!(Epoch::new(u64::MAX).next(), None);
    }

    #[test]
    fn ordering_follows_the_number() {
        assert!(Term::new(2) > Term::new(1));
        assert!(MapVersion::new(3) >= MapVersion::new(3));
    }

    #[test]
    fn display_is_the_bare_number() {
        assert_eq!(RecipeVersion::new(12).to_string(), "12");
    }
}
