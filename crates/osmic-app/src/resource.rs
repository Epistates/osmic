//! Typed resource storage.

use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Type-safe resource container keyed by `TypeId`.
///
/// Each resource type can have at most one instance. Resources are
/// inserted during plugin build and accessed at runtime.
pub struct Resources {
    map: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Resources {
    /// An empty container.
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Insert a resource, replacing any existing value of the same type.
    pub fn insert<R: Send + Sync + 'static>(&mut self, resource: R) {
        self.map.insert(TypeId::of::<R>(), Box::new(resource));
    }

    /// Get an immutable reference to a resource.
    pub fn get<R: Send + Sync + 'static>(&self) -> Option<&R> {
        self.map
            .get(&TypeId::of::<R>())
            .and_then(|boxed| boxed.downcast_ref::<R>())
    }

    /// Get a mutable reference to a resource.
    pub fn get_mut<R: Send + Sync + 'static>(&mut self) -> Option<&mut R> {
        self.map
            .get_mut(&TypeId::of::<R>())
            .and_then(|boxed| boxed.downcast_mut::<R>())
    }

    /// Remove a resource, returning it if it existed.
    pub fn remove<R: Send + Sync + 'static>(&mut self) -> Option<R> {
        self.map
            .remove(&TypeId::of::<R>())
            .and_then(|boxed| boxed.downcast::<R>().ok())
            .map(|b| *b)
    }

    /// Check if a resource type exists.
    pub fn contains<R: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<R>())
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Counter(u32);
    impl Counter {
        fn new(n: u32) -> Self {
            Self(n)
        }
    }

    #[derive(Debug, PartialEq)]
    struct Label(String);

    #[test]
    fn insert_then_get_returns_value() {
        let mut res = Resources::new();
        res.insert(Counter::new(42));
        assert_eq!(res.get::<Counter>(), Some(&Counter(42)));
    }

    #[test]
    fn get_for_missing_type_returns_none() {
        let res = Resources::new();
        assert!(res.get::<Counter>().is_none());
    }

    #[test]
    fn insert_twice_replaces_value() {
        let mut res = Resources::new();
        res.insert(Counter::new(1));
        res.insert(Counter::new(99));
        assert_eq!(res.get::<Counter>(), Some(&Counter(99)));
    }

    #[test]
    fn get_mut_allows_mutation() {
        let mut res = Resources::new();
        res.insert(Counter::new(0));
        res.get_mut::<Counter>().unwrap().0 += 7;
        assert_eq!(res.get::<Counter>(), Some(&Counter(7)));
    }

    #[test]
    fn remove_returns_value_and_subsequent_get_returns_none() {
        let mut res = Resources::new();
        res.insert(Counter::new(5));
        let removed = res.remove::<Counter>();
        assert_eq!(removed, Some(Counter(5)));
        assert!(res.get::<Counter>().is_none());
    }

    #[test]
    fn contains_correctness() {
        let mut res = Resources::new();
        assert!(!res.contains::<Counter>());
        res.insert(Counter::new(1));
        assert!(res.contains::<Counter>());
        res.remove::<Counter>();
        assert!(!res.contains::<Counter>());
    }

    #[test]
    fn two_distinct_types_do_not_collide() {
        let mut res = Resources::new();
        res.insert(Counter::new(10));
        res.insert(Label("hello".to_string()));

        assert_eq!(res.get::<Counter>(), Some(&Counter(10)));
        assert_eq!(res.get::<Label>(), Some(&Label("hello".to_string())));

        res.remove::<Counter>();
        // Label must still be present after Counter is removed.
        assert!(res.get::<Counter>().is_none());
        assert_eq!(res.get::<Label>(), Some(&Label("hello".to_string())));
    }
}
