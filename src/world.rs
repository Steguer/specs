use std::sync::atomic::{AtomicUsize, Ordering};

use hibitset::{AtomicBitSet, BitSet, BitSetOr};
use mopa::Any;
use shred::{Fetch, FetchMut, Resource, Resources};

use join::Join;
use storage::{AnyStorage, MaskedStorage, ReadStorage, Storage, UnprotectedStorage, WriteStorage};
use Index;

/// Internally used structure for `Entity` allocation.
#[derive(Default, Debug)]
pub struct Allocator {
    generations: Vec<Generation>,

    alive: BitSet,
    raised: AtomicBitSet,
    killed: AtomicBitSet,
    start_from: AtomicUsize,
}

impl Allocator {
    fn kill(&mut self, delete: &[Entity]) {
        for entity in delete {
            self.alive.remove(entity.id());
            self.raised.remove(entity.id());
            let id = entity.id() as usize;
            self.generations[id].die();
            if id < self.start_from.load(Ordering::Relaxed) {
                self.start_from.store(id, Ordering::Relaxed);
            }
        }
    }

    fn kill_atomic(&self, e: Entity) {
        self.killed.add_atomic(e.id());
    }

    /// Return `true` if the entity is alive.
    fn is_alive(&self, e: Entity) -> bool {
        e.gen() ==
        match self.generations.get(e.id() as usize) {
            Some(g) if !g.is_alive() && self.raised.contains(e.id()) => g.raised(),
            Some(g) => *g,
            None => Generation(1),
        }
    }

    /// Attempt to move the `start_from` value
    fn update_start_from(&self, start_from: usize) {
        loop {
            let current = self.start_from.load(Ordering::Relaxed);

            // if the current value is bigger then ours, we bail
            if current >= start_from {
                return;
            }

            if start_from ==
               self.start_from
                   .compare_and_swap(current, start_from, Ordering::Relaxed) {
                return;
            }
        }
    }

    /// Allocate a new entity
    fn allocate_atomic(&self) -> Entity {
        let idx = self.start_from.load(Ordering::Relaxed);
        for i in idx.. {
            if !self.alive.contains(i as Index) && !self.raised.add_atomic(i as Index) {
                self.update_start_from(i + 1);

                let gen = self.generations
                    .get(i as usize)
                    .map(|&gen| if gen.is_alive() { gen } else { gen.raised() })
                    .unwrap_or(Generation(1));

                return Entity(i as Index, gen);
            }
        }
        panic!("No entities left to allocate")
    }

    /// Allocate a new entity
    fn allocate(&mut self) -> Entity {
        let idx = self.start_from.load(Ordering::Relaxed);
        for i in idx.. {
            if !self.raised.contains(i as Index) && !self.alive.add(i as Index) {
                // this is safe since we have mutable access to everything!
                self.start_from.store(i + 1, Ordering::Relaxed);

                while self.generations.len() <= i as usize {
                    self.generations.push(Generation(0));
                }
                self.generations[i as usize] = self.generations[i as usize].raised();

                return Entity(i as Index, self.generations[i as usize]);
            }
        }
        panic!("No entities left to allocate")
    }

    fn merge(&mut self) -> Vec<Entity> {
        use hibitset::BitSetLike;

        let mut deleted = vec![];

        for i in (&self.raised).iter() {
            while self.generations.len() <= i as usize {
                self.generations.push(Generation(0));
            }
            self.generations[i as usize] = self.generations[i as usize].raised();
            self.alive.add(i);
        }
        self.raised.clear();

        if let Some(lowest) = (&self.killed).iter().next() {
            if lowest < self.start_from.load(Ordering::Relaxed) as Index {
                self.start_from.store(lowest as usize, Ordering::Relaxed);
            }
        }

        for i in (&self.killed).iter() {
            self.alive.remove(i);
            self.generations[i as usize].die();
            deleted.push(Entity(i, self.generations[i as usize]))
        }
        self.killed.clear();

        deleted
    }
}

/// Abstract component type. Doesn't have to be Copy or even Clone.
pub trait Component: Any + Sized {
    /// Associated storage type for this component.
    type Storage: UnprotectedStorage<Self> + Any + Send + Sync;
}

/// An iterator for entity creation.
/// Please note that you have to consume
/// it because iterators are lazy.
///
/// Returned from `World::create_iter`.
pub struct CreateIter<'a>(FetchMut<'a, Entities>);

impl<'a> Iterator for CreateIter<'a> {
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        Some(self.0.alloc.allocate())
    }
}

/// An iterator for entity creation.
/// Please note that you have to consume
/// it because iterators are lazy.
///
/// Returned from `Entities::create_iter`.
pub struct CreateIterAtomic<'a>(&'a Allocator);

impl<'a> Iterator for CreateIterAtomic<'a> {
    type Item = Entity;

    fn next(&mut self) -> Option<Entity> {
        Some(self.0.allocate_atomic())
    }
}

/// `Entity` type, as seen by the user.
#[derive(Clone, Copy, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Entity(Index, Generation);

impl Entity {
    /// Creates a new entity (externally from ECS).
    #[cfg(test)]
    pub fn new(index: Index, gen: Generation) -> Entity {
        Entity(index, gen)
    }

    /// Returns the index of the `Entity`.
    #[inline]
    pub fn id(&self) -> Index {
        self.0
    }

    /// Returns the `Generation` of the `Entity`.
    #[inline]
    pub fn gen(&self) -> Generation {
        self.1
    }
}

/// The entity builder, allowing to
/// build an entity together with its components.
pub struct EntityBuilder<'a> {
    entity: Entity,
    world: &'a mut World,
}

impl<'a> EntityBuilder<'a> {
    /// Appends a component with the default component id.
    ///
    /// # Panics
    ///
    /// Panics if the component hasn't been `register()`ed in the
    /// `World`.
    pub fn with<T: Component>(self, c: T) -> Self {
        self.with_id(c, 0)
    }

    /// Appends a component with a component id.
    ///
    /// # Panics
    ///
    /// Panics if the component hasn't been `register_with_id()`ed in the
    /// `World`.
    pub fn with_id<T: Component>(self, c: T, id: usize) -> Self {
        {
            let mut storage = self.world.write_with_id(id);
            storage.insert(self.entity, c);
        }

        self
    }

    /// Finishes the building and returns
    /// the entity.
    pub fn build(self) -> Entity {
        self.entity
    }
}

/// The entities of this ECS.
///
/// **Please note that you should never fetch
/// this mutably in a system, because it would
/// block all the other systems.**
///
/// You need to call `World::maintain` after creating / deleting
/// entities with this struct.
#[derive(Debug, Default)]
pub struct Entities {
    alloc: Allocator,
}

impl Entities {
    /// Creates a new entity atomically.
    /// This will be persistent as soon
    /// as you call `World::maintain`.
    pub fn create(&self) -> Entity {
        self.alloc.allocate_atomic()
    }

    /// Returns an iterator which creates
    /// new entities atomically.
    /// They will be persistent as soon
    /// as you call `World::maintain`.
    pub fn create_iter(&self) -> CreateIterAtomic {
        CreateIterAtomic(&self.alloc)
    }

    /// Deletes an entity atomically.
    /// The associated components will be
    /// deleted as soon as you call `World::maintain`.
    pub fn delete(&self, e: Entity) {
        self.alloc.kill_atomic(e);
    }

    /// Returns `true` if the specified entity is
    /// alive.
    #[inline]
    pub fn is_alive(&self, e: Entity) -> bool {
        self.alloc.is_alive(e)
    }
}

impl<'a> Join for &'a Entities {
    type Type = Entity;
    type Value = Self;
    type Mask = BitSetOr<&'a BitSet, &'a AtomicBitSet>;

    fn open(self) -> (Self::Mask, Self) {
        (BitSetOr(&self.alloc.alive, &self.alloc.raised), self)
    }

    unsafe fn get(v: &mut &'a Entities, idx: Index) -> Entity {
        let gen = v.alloc
            .generations
            .get(idx as usize)
            .map(|&gen| if gen.is_alive() { gen } else { gen.raised() })
            .unwrap_or(Generation(1));
        Entity(idx, gen)
    }
}

/// Index generation. When a new entity is placed at an old index,
/// it bumps the `Generation` by 1. This allows to avoid using components
/// from the entities that were deleted.
#[derive(Clone, Copy, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Generation(i32);

impl Generation {
    #[cfg(test)]
    pub fn new(v: i32) -> Self {
        Generation(v)
    }

    /// Returns the id of the generation.
    #[inline]
    pub fn id(&self) -> i32 {
        self.0
    }

    /// Returns `true` if entities of this `Generation` are alive.
    pub fn is_alive(&self) -> bool {
        self.0 > 0
    }

    /// Kills this `Generation`.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if it's not alive.
    fn die(&mut self) {
        debug_assert!(self.is_alive());
        self.0 = -self.0;
    }

    /// Revives and increments a dead `Generation`.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if it is alive.
    fn raised(self) -> Generation {
        debug_assert!(!self.is_alive());
        Generation(1 - self.0)
    }
}

/// The `World` struct contains the component storages and
/// other resources.
///
/// Many methods take `&self` which works because everything
/// is stored with **interior mutability**. In case you violate
/// the borrowing rules of Rust (multiple reads xor one write),
/// you will get a panic.
///
/// # Component / Resource ids
///
/// Components and resources may, in addition to their type, be identified
/// by an id of type `usize`. The convenience methods dealing
/// with components assume that it's `0`.
pub struct World {
    /// The resources used for this world.
    pub res: Resources,
    storages: Vec<*mut AnyStorage>,
}

impl World {
    /// Creates a new empty `World`.
    pub fn new() -> World {
        Default::default()
    }

    /// Registers a new component.
    ///
    /// Calls `register_with_id` with id `0`, which
    /// is the default for component ids.
    ///
    /// Does nothing if the component was already
    /// registered.
    pub fn register<T: Component>(&mut self) {
        self.register_with_id::<T>(0);
    }

    /// Registers a new component with a given id.
    ///
    /// Does nothing if the component was already
    /// registered.
    pub fn register_with_id<T: Component>(&mut self, id: usize) {
        use shred::ResourceId;

        if self.res
               .has_value(ResourceId::new_with_id::<MaskedStorage<T>>(id)) {
            return;
        }

        self.res.add_with_id(MaskedStorage::<T>::new(), id);

        let mut storage = self.res.fetch_mut::<MaskedStorage<T>>(id);
        self.storages.push(&mut *storage as *mut AnyStorage);
    }

    /// Adds a resource with the default ID (`0`).
    ///
    /// If the resource already exists it will be overwritten.
    pub fn add_resource<T: Resource>(&mut self, res: T) {
        self.add_resource_with_id(res, 0);
    }

    /// Adds a resource with a given ID.
    ///
    /// If the resource already exists it will be overwritten.
    pub fn add_resource_with_id<T: Resource>(&mut self, res: T, id: usize) {
        use shred::ResourceId;

        if self.res
               .has_value(ResourceId::new_with_id::<T>(id)) {
            *self.write_resource_with_id(id) = res;
        } else {
            self.res.add_with_id(res, id);
        }
    }

    /// Fetches a component's storage with the default id for reading.
    ///
    /// Convenience method for `read_with_id`, using the default component
    /// id (`0`).
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed mutably.
    pub fn read<T: Component>(&self) -> ReadStorage<T> {
        self.read_with_id(0)
    }

    /// Fetches a component's storage with the default id for writing.
    ///
    /// Convenience method for `write_with_id`, using the default component
    /// id (`0`).
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed.
    pub fn write<T: Component>(&self) -> WriteStorage<T> {
        self.write_with_id(0)
    }

    /// Fetches a component's storage with a specified id for reading.
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed mutably.
    pub fn read_with_id<T: Component>(&self, id: usize) -> ReadStorage<T> {
        let entities = self.entities();

        Storage::new(entities, self.res.fetch(id))
    }

    /// Fetches a component's storage with a specified id for writing.
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed.
    pub fn write_with_id<T: Component>(&self, id: usize) -> WriteStorage<T> {
        let entities = self.entities();

        Storage::new(entities, self.res.fetch_mut(id))
    }

    /// Fetches a resource with a specified id for reading.
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed mutably.
    pub fn read_resource_with_id<T: Resource>(&self, id: usize) -> Fetch<T> {
        self.res.fetch(id)
    }

    /// Fetches a resource with a specified id for writing.
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed.
    pub fn write_resource_with_id<T: Resource>(&self, id: usize) -> FetchMut<T> {
        self.res.fetch_mut(id)
    }

    /// Fetches a resource with the default id for reading.
    ///
    /// Convenience method for `read_resource_with_id`, using the default component
    /// id (`0`).
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed mutably.
    pub fn read_resource<T: Resource>(&self) -> Fetch<T> {
        self.read_resource_with_id(0)
    }

    /// Fetches a resource with the default id for writing.
    ///
    /// Convenience method for `write_resource_with_id`, using the default component
    /// id (`0`).
    ///
    /// # Panics
    ///
    /// Panics if it is already borrowed.
    pub fn write_resource<T: Resource>(&self) -> FetchMut<T> {
        self.write_resource_with_id(0)
    }

    /// Convenience method for fetching entities.
    ///
    /// Creation and deletion of entities with the `Entities` struct
    /// are atomically, so the actual changes will be applied
    /// with the next call to `maintain()`.
    pub fn entities(&self) -> Fetch<Entities> {
        self.read_resource()
    }

    /// Convenience method for fetching entities.
    fn entities_mut(&self) -> FetchMut<Entities> {
        self.write_resource()
    }

    /// Allows building an entity with its
    /// components.
    pub fn create_entity(&mut self) -> EntityBuilder {
        let entity = self.entities_mut().alloc.allocate();

        EntityBuilder {
            entity: entity,
            world: self,
        }
    }

    /// Returns an iterator for entity creation.
    /// This makes it easy to create a whole collection
    /// of them.
    ///
    /// # Examples
    ///
    /// ```
    /// use specs::World;
    ///
    /// let mut world = World::new();
    /// let five_entities: Vec<_> = world.create_iter().take(5).collect();
    /// #
    /// # assert_eq!(five_entities.len(), 5);
    /// ```
    pub fn create_iter(&mut self) -> CreateIter {
        CreateIter(self.entities_mut())
    }

    /// Deletes an entity and its components.
    pub fn delete_entity(&mut self, entity: Entity) {
        self.delete_entities(&[entity]);
    }

    /// Deletes the specified entities and their components.
    pub fn delete_entities(&mut self, delete: &[Entity]) {
        self.delete_components(delete);
        self.entities_mut().alloc.kill(delete);
    }

    /// Checks if an entity is alive.
    /// Please note that atomically created or deleted entities
    /// (the ones created / deleted with the `Entities` struct)
    /// are not handled by this method. Therefore, you
    /// should have called `maintain()` before using this
    /// method.
    ///
    /// # Panics
    ///
    /// Panics if generation is dead.
    pub fn is_alive(&self, e: Entity) -> bool {
        assert!(e.gen().is_alive(), "Generation is dead");

        let alloc: &Allocator = &self.entities().alloc;
        alloc
            .generations
            .get(e.id() as usize)
            .map(|&x| x == e.gen())
            .unwrap_or(false)
    }

    /// Merges in the appendix, recording all the dynamically created
    /// and deleted entities into the persistent generations vector.
    /// Also removes all the abandoned components.
    pub fn maintain(&mut self) {
        let deleted = self.entities_mut().alloc.merge();
        self.delete_components(&deleted);
    }

    fn delete_components(&mut self, delete: &[Entity]) {
        for storage in &mut self.storages {
            let storage: &mut AnyStorage = unsafe { &mut **storage };

            for entity in delete {
                storage.remove(entity.id());
            }
        }
    }
}

impl Default for World {
    fn default() -> Self {
        let mut res = Resources::new();
        res.add(Entities::default());

        World {
            res: res,
            storages: Default::default(),
        }
    }
}
