use bevy::prelude::*;

/// Marker for the tree view container
#[derive(Component)]
pub struct TreeView;

/// Pointer from a tree row to the scene entity it represents.
#[derive(Component)]
pub struct TreeNode(pub Entity);

/// Marker for expand/collapse toggle button
#[derive(Component)]
pub struct TreeNodeExpandToggle;

/// Tracks whether a tree node is expanded
#[derive(Component, Default)]
pub struct TreeNodeExpanded(pub bool);

/// Whether this row has children and should show an expand chevron.
#[derive(Component, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeNodeHasChildren(pub bool);

/// The clickable content area of a tree row (contains toggle + label)
#[derive(Component)]
pub struct TreeRowContent;

/// Marker on `TreeRowContent` when its source entity is selected
#[derive(Component)]
pub struct TreeRowSelected;

/// Container for displaying the row label
#[derive(Component)]
#[require(Text)]
pub struct TreeRowLabel;

/// Container for child rows (indented)
#[derive(Component)]
pub struct TreeRowChildren;

/// Tracks whether a tree node's children have been lazily populated.
/// Set to `true` after first expansion spawns children; prevents re-population on re-expand.
#[derive(Component, Default)]
pub struct TreeChildrenPopulated(pub bool);

/// Classifies a scene entity by type for colored dot display.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntityCategory {
    Camera,
    Light,
    Mesh,
    Scene,
    Prefab,
    /// Entity inherited from a prefab (carries `PrefabEntityId` but no
    /// `IsA`). Drawn with a faint tinge to signal it's a materialized
    /// child of an instance rather than authored directly.
    Inherited,
    /// A node of a loaded asset (a glTF scene's own nodes and meshes) rather
    /// than something authored. Shown so the model's structure is inspectable,
    /// but drawn with its own icon and a muted tone because it has no document
    /// node and so cannot be duplicated, deleted or reparented.
    AssetPart,
    /// A container entity: it has children but no more specific type of its
    /// own, so it reads as a grouping node (e.g. a "Trees" parent).
    Group,
    #[default]
    Entity,
}

/// Marker for the colored category dot in a tree row.
#[derive(Component)]
pub struct TreeRowDot;

/// Marker for the visibility toggle icon in a tree row.
#[derive(Component)]
pub struct TreeRowVisibilityToggle;

/// Event fired when a visibility toggle is clicked
#[derive(EntityEvent)]
pub struct TreeRowVisibilityToggled {
    #[event_target]
    pub entity: Entity,
    /// The source (scene) entity to toggle visibility
    pub source_entity: Entity,
}

/// Marker on the text input during inline rename
#[derive(Component)]
pub struct TreeRowInlineRename;

/// Reverse lookup from `(container, source)` to the row that shows
/// that source in that tree.
///
/// Spawners insert here themselves when they need the mapping in the
/// same pass. Removing [`TreeRoot`] from a container (including on
/// despawn) drops every mapping keyed by that container.
#[derive(Resource, Default)]
pub struct TreeIndex {
    /// `(container, source)` -> tree row entity. The container is the
    /// host entity carrying [`TreeRoot`]; the source is the scene
    /// entity the row represents.
    map: HashMap<(Entity, Entity), Entity>,
}

impl TreeIndex {
    /// Tree row entity for `source` in `container`, if one exists.
    pub fn get(&self, container: Entity, source: Entity) -> Option<Entity> {
        self.map.get(&(container, source)).copied()
    }

    /// Insert / overwrite the mapping for the `(container, source)` pair.
    pub fn insert(&mut self, container: Entity, source: Entity, tree_row: Entity) {
        self.map.insert((container, source), tree_row);
    }

    /// Drop the mapping for the `(container, source)` pair.
    pub fn remove(&mut self, container: Entity, source: Entity) {
        self.map.remove(&(container, source));
    }

    /// Drop every mapping for `source` across every container. Used
    /// when a scene entity goes away and its rows in every panel
    /// should be forgotten.
    pub fn remove_source(&mut self, source: Entity) {
        self.map.retain(|(_, s), _| *s != source);
    }

    /// True if `source` has a row in `container`.
    pub fn contains(&self, container: Entity, source: Entity) -> bool {
        self.map.contains_key(&(container, source))
    }

    /// True if `source` has a row in any container.
    pub fn contains_anywhere(&self, source: Entity) -> bool {
        self.map.keys().any(|(_, s)| *s == source)
    }

    /// Iterate every row entity for `source` across all containers.
    pub fn rows_for_source(&self, source: Entity) -> impl Iterator<Item = (Entity, Entity)> + '_ {
        self.map
            .iter()
            .filter(move |((_, s), _)| *s == source)
            .map(|((c, _), row)| (*c, *row))
    }

    /// Iterate every row entity for `container`.
    pub fn rows_in(&self, container: Entity) -> impl Iterator<Item = (Entity, Entity)> + '_ {
        self.map
            .iter()
            .filter(move |((c, _), _)| *c == container)
            .map(|((_, s), row)| (*s, *row))
    }

    /// Drop every mapping for `container`. Used when a panel hosting
    /// a tree is torn down.
    pub fn clear_container(&mut self, container: Entity) {
        self.map.retain(|(c, _), _| *c != container);
    }

    /// Drop every mapping. Used when the host app fully resets state.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

/// Marker the consumer adds to the entity that hosts a tree (every
/// `Outliner` panel content entity, in jackdaw's case). Used as the
/// container key in [`TreeIndex`]. Removing this component (including
/// on despawn) clears that container's mappings.
#[derive(Component, Default)]
pub struct TreeRoot;

use std::collections::HashMap;

/// Tracks which tree row has keyboard focus (rendered with a focus ring).
#[derive(Resource, Default)]
pub struct TreeFocused(pub Option<Entity>);

/// Event fired when a tree row is clicked
#[derive(EntityEvent)]
pub struct TreeRowClicked {
    #[event_target]
    pub entity: Entity,
    /// The source entity this tree row represents
    pub source_entity: Entity,
}

/// Event fired when a tree row is dropped onto another tree row
#[derive(EntityEvent)]
pub struct TreeRowDropped {
    #[event_target]
    pub entity: Entity,
    /// The scene entity being moved
    pub dragged_source: Entity,
    /// The scene entity to become new parent
    pub target_source: Entity,
}

/// Event fired when a tree row is dropped onto the root container (deparent)
#[derive(EntityEvent)]
pub struct TreeRowDroppedOnRoot {
    #[event_target]
    pub entity: Entity,
    /// The scene entity being moved back to root
    pub dragged_source: Entity,
}

/// Event fired when an inline rename is committed
#[derive(EntityEvent)]
pub struct TreeRowRenamed {
    #[event_target]
    pub entity: Entity,
    /// The source (scene) entity
    pub source_entity: Entity,
    /// The new name entered by the user
    pub new_name: String,
}

/// Event fired to request starting an inline rename
#[derive(EntityEvent)]
pub struct TreeRowStartRename {
    #[event_target]
    pub entity: Entity,
    /// The source (scene) entity to rename
    pub source_entity: Entity,
}

pub struct TreeViewPlugin;

impl Plugin for TreeViewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TreeIndex>()
            .init_resource::<TreeFocused>()
            .add_observer(unindex_tree_root);
    }
}

/// Drop every [`TreeIndex`] mapping for a container when its
/// [`TreeRoot`] is removed, including on despawn.
fn unindex_tree_root(trigger: On<Remove, TreeRoot>, mut index: ResMut<TreeIndex>) {
    index.clear_container(trigger.event_target());
}
