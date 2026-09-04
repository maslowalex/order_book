#![deny(unsafe_op_in_unsafe_fn)]

use std::fmt::{self, Debug};
use std::mem::MaybeUninit;

/// Stable identity for one arena occupant.
///
/// The index chooses a slot; the generation proves that the handle names the
/// slot's current occupant rather than a value that was removed earlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Handle {
    pub index: u32,
    pub generation: u32,
}

/// Storage that is either vacant or contains exactly one initialized `T`.
///
/// # Invariant
///
/// `value` is initialized if and only if `occupied` is true. Only `insert`,
/// `remove`, `clone`, and `drop` may change or observe that state.
struct Slot<T> {
    value: MaybeUninit<T>,
    index: u32,
    generation: u32,
    occupied: bool,
}

impl<T> Slot<T> {
    fn vacant(index: u32) -> Self {
        Self {
            value: MaybeUninit::uninit(),
            index,
            generation: 0,
            occupied: false,
        }
    }

    fn insert(&mut self, value: T) -> Result<Handle, T> {
        if self.occupied {
            return Err(value);
        }

        self.value.write(value);
        self.occupied = true;
        Ok(Handle {
            index: self.index,
            generation: self.generation,
        })
    }

    fn get(&self, handle: Handle) -> Option<&T> {
        if !self.matches(handle) {
            return None;
        }

        // SAFETY: `matches` requires `occupied == true`. By the slot invariant,
        // `value` was initialized by `insert` and has not been moved out or
        // dropped. The shared slot borrow prevents either transition while the
        // returned reference exists.
        Some(unsafe { self.value.assume_init_ref() })
    }

    fn get_mut(&mut self, handle: Handle) -> Option<&mut T> {
        if !self.matches(handle) {
            return None;
        }

        // SAFETY: `matches` proves the storage is initialized and names the
        // current generation. The exclusive slot borrow ensures no other
        // reference can be created through the safe arena API.
        Some(unsafe { self.value.assume_init_mut() })
    }

    fn remove(&mut self, handle: Handle) -> Option<T> {
        if !self.matches(handle) {
            return None;
        }

        // Do the fallible transition first. On exhaustion, the slot remains
        // unchanged rather than holding moved-out bytes marked occupied.
        let next_generation = self
            .generation
            .checked_add(1)
            .expect("arena slot generation exhausted");

        // SAFETY: `matches` proves `value` is initialized. This is the only
        // operation that moves it out; immediately afterwards the slot becomes
        // vacant and changes generation, preventing another read or drop.
        let value = unsafe { self.value.assume_init_read() };
        self.occupied = false;
        self.generation = next_generation;
        Some(value)
    }

    fn matches(&self, handle: Handle) -> bool {
        self.occupied && self.index == handle.index && self.generation == handle.generation
    }
}

impl<T: Clone> Clone for Slot<T> {
    fn clone(&self) -> Self {
        let mut cloned = Self {
            value: MaybeUninit::uninit(),
            index: self.index,
            generation: self.generation,
            occupied: false,
        };

        if self.occupied {
            // SAFETY: the slot invariant says occupied storage contains a live
            // `T`. A panic in `T::clone` leaves `cloned` safely vacant.
            let value = unsafe { self.value.assume_init_ref() }.clone();
            cloned.value.write(value);
            cloned.occupied = true;
        }

        cloned
    }
}

impl<T: Debug> Debug for Slot<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut slot = formatter.debug_struct("Slot");
        slot.field("index", &self.index)
            .field("generation", &self.generation)
            .field("occupied", &self.occupied);
        if self.occupied {
            // SAFETY: the slot invariant says occupied storage contains a live
            // `T`; formatting holds only a shared borrow.
            slot.field("value", unsafe { self.value.assume_init_ref() });
        }
        slot.finish()
    }
}

impl<T> Drop for Slot<T> {
    fn drop(&mut self) {
        if self.occupied {
            // SAFETY: occupied storage contains exactly one initialized `T`.
            // `drop` owns the slot exclusively and runs once.
            unsafe { self.value.assume_init_drop() };
        }
    }
}

/// A generational arena with constant-time insertion, lookup, and removal.
#[derive(Debug, Clone)]
pub(crate) struct Arena<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    len: usize,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Arena<T> {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }

    pub(crate) fn insert(&mut self, value: T) -> Handle {
        let handle = if let Some(index) = self.free.pop() {
            self.slots[index as usize]
                .insert(value)
                .unwrap_or_else(|_| panic!("free list contained an occupied arena slot"))
        } else {
            let index = u32::try_from(self.slots.len()).expect("arena exhausted u32 indices");
            self.slots
                .try_reserve(1)
                .expect("arena could not reserve another slot");
            let mut slot = Slot::vacant(index);
            let handle = slot
                .insert(value)
                .unwrap_or_else(|_| panic!("a new arena slot must be vacant"));
            self.slots.push(slot);
            handle
        };
        self.len += 1;
        handle
    }

    pub(crate) fn get(&self, handle: Handle) -> Option<&T> {
        self.slots.get(handle.index as usize)?.get(handle)
    }

    pub(crate) fn get_mut(&mut self, handle: Handle) -> Option<&mut T> {
        self.slots.get_mut(handle.index as usize)?.get_mut(handle)
    }

    pub(crate) fn remove(&mut self, handle: Handle) -> Option<T> {
        self.get(handle)?;

        // Reserve before changing the slot so allocation failure cannot leave
        // a newly-vacant slot missing from the free list.
        self.free
            .try_reserve(1)
            .expect("arena could not grow its free list");
        let value = self.slots[handle.index as usize]
            .remove(handle)
            .expect("validated handle must remain live during exclusive mutation");
        self.free.push(handle.index);
        self.len -= 1;
        Some(value)
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use proptest::prelude::*;
    use slotmap::{SlotMap, new_key_type};

    use super::*;

    new_key_type! {
        struct OracleKey;
    }

    #[derive(Debug)]
    struct DropSpy(Rc<Cell<usize>>);

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn vacant_slot_returns_none() {
        let slot = Slot::<String>::vacant(7);
        assert_eq!(
            slot.get(Handle {
                index: 7,
                generation: 0
            }),
            None
        );
    }

    #[test]
    fn insert_makes_value_accessible() {
        let mut slot = Slot::vacant(7);
        let handle = slot.insert(String::from("maker")).unwrap();
        assert_eq!(slot.get(handle).map(String::as_str), Some("maker"));
    }

    #[test]
    fn get_mut_updates_the_value() {
        let mut slot = Slot::vacant(7);
        let handle = slot.insert(String::from("maker")).unwrap();
        slot.get_mut(handle).unwrap().push_str(" order");
        assert_eq!(slot.get(handle).map(String::as_str), Some("maker order"));
    }

    #[test]
    fn occupied_slot_rejects_second_insert() {
        let mut slot = Slot::vacant(7);
        let first = slot.insert(String::from("first")).unwrap();
        let second = slot.insert(String::from("second")).unwrap_err();
        assert_eq!(second, "second");
        assert_eq!(slot.get(first).map(String::as_str), Some("first"));
    }

    #[test]
    fn remove_returns_the_owned_value() {
        let mut slot = Slot::vacant(7);
        let handle = slot.insert(String::from("maker")).unwrap();
        assert_eq!(slot.remove(handle), Some(String::from("maker")));
    }

    #[test]
    fn removed_handle_is_invalid() {
        let mut slot = Slot::vacant(7);
        let handle = slot.insert(String::from("maker")).unwrap();
        slot.remove(handle).unwrap();
        assert_eq!(slot.get(handle), None);
        assert_eq!(slot.remove(handle), None);
    }

    #[test]
    fn stale_handle_cannot_access_reused_slot() {
        let mut slot = Slot::vacant(7);
        let stale = slot.insert(String::from("old")).unwrap();
        slot.remove(stale).unwrap();
        let current = slot.insert(String::from("new")).unwrap();
        assert_eq!(stale.index, current.index);
        assert_ne!(stale.generation, current.generation);
        assert_eq!(slot.get(stale), None);
        assert_eq!(slot.get(current).map(String::as_str), Some("new"));
    }

    #[test]
    fn dropping_an_occupied_slot_drops_value_once() {
        let drops = Rc::new(Cell::new(0));
        {
            let mut slot = Slot::vacant(0);
            slot.insert(DropSpy(Rc::clone(&drops))).unwrap();
        }
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn removing_then_dropping_slot_does_not_double_drop() {
        let drops = Rc::new(Cell::new(0));
        let removed = {
            let mut slot = Slot::vacant(0);
            let handle = slot.insert(DropSpy(Rc::clone(&drops))).unwrap();
            let removed = slot.remove(handle).unwrap();
            drop(slot);
            assert_eq!(drops.get(), 0);
            removed
        };
        drop(removed);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn arena_reuses_storage_but_not_identity() {
        let mut arena = Arena::new();
        let old = arena.insert(String::from("old"));
        assert_eq!(arena.remove(old).as_deref(), Some("old"));
        let new = arena.insert(String::from("new"));

        assert_eq!(old.index, new.index);
        assert_ne!(old.generation, new.generation);
        assert!(arena.get(old).is_none());
        assert_eq!(arena.get(new).map(String::as_str), Some("new"));
        assert_eq!(arena.len(), 1);
    }

    #[test]
    fn cloned_arena_has_independent_values_and_matching_handles() {
        let mut arena = Arena::new();
        let key = arena.insert(String::from("maker"));
        let mut cloned = arena.clone();
        cloned.get_mut(key).unwrap().push_str(" clone");

        assert_eq!(arena.get(key).map(String::as_str), Some("maker"));
        assert_eq!(cloned.get(key).map(String::as_str), Some("maker clone"));
    }

    proptest! {
        /// `slotmap` is the known-safe oracle for reuse and stale-key behavior.
        /// Logical handles are kept in parallel because their physical key
        /// encodings intentionally differ.
        #[test]
        fn random_operations_match_slotmap(
            operations in prop::collection::vec((any::<bool>(), any::<u16>(), any::<u64>()), 1..500)
        ) {
            let mut subject = Arena::new();
            let mut oracle = SlotMap::<OracleKey, u64>::with_key();
            let mut handles: Vec<Option<(Handle, OracleKey)>> = Vec::new();

            for (insert, selector, value) in operations {
                if insert || handles.is_empty() {
                    handles.push(Some((subject.insert(value), oracle.insert(value))));
                    continue;
                }

                let position = usize::from(selector) % handles.len();
                if let Some((subject_key, oracle_key)) = handles[position].take() {
                    prop_assert_eq!(subject.get(subject_key), oracle.get(oracle_key));
                    prop_assert_eq!(subject.remove(subject_key), oracle.remove(oracle_key));
                    prop_assert!(subject.get(subject_key).is_none());
                    prop_assert!(oracle.get(oracle_key).is_none());
                }
            }

            prop_assert_eq!(subject.len(), oracle.len());
            prop_assert_eq!(subject.is_empty(), oracle.is_empty());
            for (subject_key, oracle_key) in handles.into_iter().flatten() {
                prop_assert_eq!(subject.get(subject_key), oracle.get(oracle_key));
            }
        }
    }
}
