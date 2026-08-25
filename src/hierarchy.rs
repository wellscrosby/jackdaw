use std::collections::HashSet;

use bevy::{
    input_focus::{FocusCause, InputFocus},
    prelude::*,
    ui::ui_transform::UiGlobalTransform,
};
use bevy_enhanced_input::prelude::{Press, *};
use bevy_monitors::prelude::{Mutation, NotifyChanged};
use jackdaw_api::prelude::*;
use jackdaw_api_internal::entity_icons::{EntityIconRegistry, registered_icon};
use jackdaw_api_internal::keymap::PresetInput;
use jackdaw_feathers::{
    context_menu::spawn_context_menu,
    icons::IconFont,
    text_edit::{self, EditorTextEdit, TextEditCommitEvent, TextEditProps, TextEditValue},
    tokens,
    tree_view::{ROW_BG, TreeRowStyle, tree_row},
};
use jackdaw_widgets::context_menu::{ContextMenuAction, ContextMenuState};
use jackdaw_widgets::tree_view::{
    EntityCategory, TreeChildrenPopulated, TreeFocused, TreeIndex, TreeNode, TreeNodeExpanded,
    TreeNodeHasChildren, TreeRowChildren, TreeRowClicked, TreeRowContent, TreeRowDot,
    TreeRowDropped, TreeRowDroppedOnRoot, TreeRowInlineRename, TreeRowLabel, TreeRowRenamed,
    TreeRowSelected, TreeRowStartRename, TreeRowVisibilityToggle, TreeRowVisibilityToggled,
};

use crate::{
    EditorEntity, EditorHidden, OP_PREFIX,
    commands::{CommandHistory, EditorCommand, ReparentEntity, SetBsnField},
    entity_ops,
    layout::HierarchyFilter,
    pie_projection::LivePreview,
    selection::{Selected, Selection},
};
use bevy::world_serialization::WorldAssetRoot;
use jackdaw_feathers::dialog::{DialogActionEvent, DialogChildrenSlot};
use jackdaw_scene_types::Brush;

/// Stores the default name for the prefab save dialog.
#[derive(Resource, Default)]
struct PendingPrefabDefaultName(String);

/// Distinguishes between "save subtree as new prefab file" and
/// "save instance + its overrides as a variant of the current prefab".
#[derive(Default, Clone, Copy)]
pub enum PrefabSaveMode {
    /// Save the selected entities as a new prefab file; the source
    /// becomes an `IsA` instance in the current scene. Source tab is
    /// unchanged.
    #[default]
    Prefab,
    /// Save the entire active scene tab as a prefab file. The tab
    /// itself converts to a Prefab tab (`TabKind::Prefab`,
    /// `TabContent::Prefab(path)`, Package icon, Ctrl+S goes through
    /// the prefab save branch).
    Scene,
    /// Save the current instance + its overrides as a variant of the
    /// underlying prefab.
    Variant,
}

/// Tracks which entities to package when the prefab save dialog is confirmed.
#[derive(Resource, Default)]
pub struct PendingPrefabSave {
    pub roots: Vec<Entity>,
    pub mode: PrefabSaveMode,
}

/// Marker for the prefab name text input inside the dialog.
#[derive(Component)]
struct PrefabNameInput;

/// Marker for the hierarchy panel
#[derive(Component)]
#[require(EditorEntity)]
pub struct HierarchyPanel;

/// Marker for the container that holds tree rows. Carries the
/// widget-side [`jackdaw_widgets::tree_view::TreeRoot`] so the
/// per-container `TreeIndex` knows where to file the rows that
/// descend from it. Multi-instance Outliner tabs each spawn their
/// own container; the index keys rows by `(container, source)` so
/// they don't collide.
#[derive(Component)]
#[require(EditorEntity, jackdaw_widgets::tree_view::TreeRoot)]
pub struct HierarchyTreeContainer;

pub struct HierarchyPlugin;

impl Plugin for HierarchyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ContextMenuState>()
            .init_resource::<PendingPrefabDefaultName>()
            .init_resource::<PendingPrefabSave>()
            .init_resource::<RevealTarget>()
            .init_resource::<EntityIconRegistry>()
            .add_systems(Startup, setup_tree_node_expanded_watcher)
            .add_systems(OnEnter(crate::AppState::Editor), setup_name_watcher)
            .add_systems(
                Update,
                (
                    apply_hierarchy_filter,
                    auto_focus_inline_rename,
                    populate_prefab_dialog,
                    sync_pie_live_outliner,
                    watch_selection_for_reveal,
                    drive_reveal_target,
                    jackdaw_feathers::tree_view::tree_keyboard_navigation,
                )
                    .run_if(in_state(crate::AppState::Editor)),
            )
            .add_systems(
                PostUpdate,
                (
                    crate::pie_projection::sync_live_previews,
                    reconcile_outliner,
                )
                    .chain(),
            )
            .add_observer(handle_inline_rename_commit)
            .add_observer(on_tree_node_expanded)
            .add_observer(on_tree_row_clicked)
            .add_observer(on_name_changed)
            .add_observer(on_brush_icon_ready)
            .add_observer(on_entity_selected)
            .add_observer(on_entity_deselected)
            .add_observer(on_tree_row_dropped)
            .add_observer(on_tree_row_dropped_on_root)
            .add_observer(on_tree_row_start_rename)
            .add_observer(on_tree_row_renamed)
            .add_observer(on_context_menu_action)
            .add_observer(on_visibility_toggled)
            .add_observer(on_prefab_dialog_action);
    }
}

/// True when `entity` was spawned by the glTF loader under a `GltfSource`
/// root rather than authored by the user. Authored entities parented under a
/// model keep a document node, so the absence of one is what separates the
/// loader's nodes from anything the user put there.
fn is_asset_part(world: &World, entity: Entity) -> bool {
    if world.get::<jackdaw_bsn::AstNodeRef>(entity).is_some() {
        return false;
    }
    let mut current = entity;
    while let Some(ChildOf(parent)) = world.get::<ChildOf>(current) {
        if world
            .get::<jackdaw_scene_types::GltfSource>(*parent)
            .is_some()
        {
            return true;
        }
        current = *parent;
    }
    false
}

/// Classify a scene entity by its primary component for tree display.
/// Returns the underlying category (Brush mesh, Camera, Light, etc.)
/// regardless of whether the entity is inherited from a prefab. Inherited
/// status is conveyed separately via [`is_inherited_descendant`] so the
/// outliner can pair the right icon with a muted color.
fn classify_entity(world: &World, entity: Entity) -> EntityCategory {
    // Checked before the component-based arms below: a glTF leaf carries
    // `Mesh3d` and would otherwise read as an ordinary authored mesh.
    if is_asset_part(world, entity) {
        return EntityCategory::AssetPart;
    }
    if world.get::<crate::prefab::IsA>(entity).is_some() {
        return EntityCategory::Prefab;
    }
    if world.get::<Camera>(entity).is_some() {
        return EntityCategory::Camera;
    }
    if world.get::<PointLight>(entity).is_some()
        || world.get::<DirectionalLight>(entity).is_some()
        || world.get::<SpotLight>(entity).is_some()
    {
        return EntityCategory::Light;
    }
    if world.get::<Mesh3d>(entity).is_some() {
        return EntityCategory::Mesh;
    }
    if world
        .get::<jackdaw_scene_types::SceneRootTag>(entity)
        .is_some()
    {
        return EntityCategory::Scene;
    }
    // `GltfSource` is the authored component and `WorldAssetRoot` the handle
    // derived from it, so match the former first: otherwise the row's icon
    // depends on whether the asset has finished loading yet.
    if world
        .get::<jackdaw_scene_types::GltfSource>(entity)
        .is_some()
        || world.get::<WorldAssetRoot>(entity).is_some()
    {
        return EntityCategory::Scene;
    }
    // An entity with no type of its own but with children reads as a grouping
    // container (a "Trees" or "Player" parent), so it gets the group icon.
    if !outliner_children(world, entity).is_empty() {
        return EntityCategory::Group;
    }
    EntityCategory::Entity
}

/// True when this entity is an inherited descendant of a prefab instance
/// (`PrefabEntityId` present, `IsA` absent). The outliner mutes such
/// rows so they're visually distinguishable from authored entities.
fn is_inherited_descendant(world: &World, entity: Entity) -> bool {
    world.get::<crate::prefab::IsA>(entity).is_none()
        && world.get::<crate::prefab::PrefabEntityId>(entity).is_some()
}

/// True when the outliner is currently showing the Live (running game) tree.
fn outliner_in_live_mode(world: &World) -> bool {
    world
        .get_resource::<crate::pie_mirror::PieViewMode>()
        .copied()
        .unwrap_or_default()
        == crate::pie_mirror::PieViewMode::Live
}

/// Children that belong under `entity` in the active outliner.
fn outliner_children(world: &World, entity: Entity) -> Vec<Entity> {
    if outliner_in_live_mode(world) {
        live_children(world, entity)
    } else {
        scene_children(world, entity)
    }
}

/// Scene-mode roots: document root nodes that currently have an ECS entity.
fn scene_roots(world: &World) -> Vec<Entity> {
    let Some(doc) = world.get_resource::<jackdaw_bsn::SceneBsnAst>() else {
        return Vec::new();
    };
    doc.roots
        .clone()
        .into_iter()
        .filter_map(|ast| {
            let ecs = doc.ecs_for_ast(ast)?;
            is_scene_member(world, ecs).then_some(ecs)
        })
        .collect()
}

/// Scene-mode children of `entity`: document children first, then loader
/// instance nodes when this row is a `GltfSource` or an existing asset part.
fn scene_children(world: &World, entity: Entity) -> Vec<Entity> {
    let mut kids = Vec::new();
    if let Some(doc) = world.get_resource::<jackdaw_bsn::SceneBsnAst>()
        && let Some(ast) = doc.ast_for(entity)
    {
        for child_ast in doc.get_children_ast(ast) {
            if let Some(ecs) = doc.ecs_for_ast(child_ast)
                && is_scene_member(world, ecs)
                && !kids.contains(&ecs)
            {
                kids.push(ecs);
            }
        }
    }
    if (world
        .get::<jackdaw_scene_types::GltfSource>(entity)
        .is_some()
        || is_asset_part(world, entity))
        && let Some(children) = world.get::<Children>(entity)
    {
        for child in children.iter() {
            if is_scene_member(world, child)
                && is_asset_part(world, child)
                && !kids.contains(&child)
            {
                kids.push(child);
            }
        }
    }
    kids
}

fn is_scene_member(world: &World, entity: Entity) -> bool {
    world.get_entity(entity).is_ok()
        && world.get::<EditorHidden>(entity).is_none()
        && world.get::<EditorEntity>(entity).is_none()
        && (world.get::<jackdaw_bsn::AstNodeRef>(entity).is_some() || is_asset_part(world, entity))
}

fn live_children(world: &World, entity: Entity) -> Vec<Entity> {
    let Some(children) = world.get::<Children>(entity) else {
        return Vec::new();
    };
    children
        .iter()
        .filter(|&child| world.get::<LivePreview>(child).is_some())
        .collect()
}

/// Returns true if `entity` has `PrefabEntityId` but NOT `IsA` -- meaning
/// it's an entity materialized from a prefab, not an instance root.
fn is_inherited_entity(world: &World, entity: Entity) -> bool {
    world.get::<crate::prefab::PrefabEntityId>(entity).is_some()
        && world.get::<crate::prefab::IsA>(entity).is_none()
}

/// Walks up from `entity` through `ChildOf` until it finds an ancestor
/// with `IsA`. Returns the instance root, or `None` if not inside an
/// instance.
fn find_instance_root(world: &World, mut entity: Entity) -> Option<Entity> {
    loop {
        if world.get::<crate::prefab::IsA>(entity).is_some() {
            return Some(entity);
        }
        entity = world.get::<ChildOf>(entity)?.0;
    }
}

/// Snapshot of every Outliner panel container. Cached so reconcile
/// can reuse the `QueryState` each frame.
fn collect_hierarchy_containers(
    containers: Query<Entity, With<HierarchyTreeContainer>>,
) -> Vec<Entity> {
    containers.iter().collect()
}

/// Spawn a single (non-recursive) tree row for `source` under `parent`.
/// `container` is the Outliner panel used as the `TreeIndex` key.
///
/// Insert into `TreeIndex` here: reconcile looks the row up in the
/// same exclusive-world pass, so the mapping has to exist before
/// spawn returns.
fn spawn_single_tree_row(
    world: &mut World,
    source: Entity,
    parent: Entity,
    container: Entity,
) -> Entity {
    let label = world
        .get::<Name>(source)
        .map(|n| n.as_str().to_string())
        .unwrap_or_else(|| format!("Entity {source}"));
    let has_children = !outliner_children(world, source).is_empty();
    let category = classify_entity(world, source);
    let inherited = is_inherited_descendant(world, source);
    let icon_font = world.resource::<IconFont>().0.clone();
    let style = TreeRowStyle { icon_font };
    let icon_override = registered_icon(world, source);

    let tree_row_entity = world
        .spawn((
            tree_row(
                &label,
                has_children,
                false,
                source,
                category,
                inherited,
                icon_override,
                &style,
            ),
            ChildOf(parent),
        ))
        .id();

    world
        .resource_mut::<TreeIndex>()
        .insert(container, source, tree_row_entity);
    tree_row_entity
}

fn current_outliner_roots(world: &mut World) -> Vec<Entity> {
    if outliner_in_live_mode(world) {
        live_tree_roots(world)
    } else {
        scene_roots(world)
    }
}

fn tree_row_children_slot(world: &World, row: Entity) -> Option<Entity> {
    world
        .get::<Children>(row)?
        .iter()
        .find(|&child| world.get::<TreeRowChildren>(child).is_some())
}

fn push_populated_descendants(
    world: &World,
    container: Entity,
    source: Entity,
    desired: &mut Vec<(Entity, Entity)>,
) {
    let Some(row) = world.resource::<TreeIndex>().get(container, source) else {
        return;
    };
    if !world
        .get::<TreeChildrenPopulated>(row)
        .is_some_and(|populated| populated.0)
    {
        return;
    }
    let Some(slot) = tree_row_children_slot(world, row) else {
        return;
    };
    for child in outliner_children(world, source) {
        if desired.iter().any(|(existing, _)| *existing == child) {
            continue;
        }
        desired.push((child, slot));
        push_populated_descendants(world, container, child, desired);
    }
}

fn visible_placements(world: &World, container: Entity, roots: &[Entity]) -> Vec<(Entity, Entity)> {
    let mut desired = Vec::new();
    for &root in roots {
        desired.push((root, container));
        push_populated_descendants(world, container, root, &mut desired);
    }
    desired
}

fn reconcile_container(world: &mut World, container: Entity, roots: &[Entity]) {
    let desired = visible_placements(world, container, roots);
    let desired_sources: HashSet<Entity> = desired.iter().map(|(source, _)| *source).collect();

    for (source, slot) in &desired {
        let Some(row) = world.resource::<TreeIndex>().get(container, *source) else {
            continue;
        };
        let parent = world.get::<ChildOf>(row).map(|child_of| child_of.0);
        if parent != Some(*slot)
            && let Ok(mut entity_mut) = world.get_entity_mut(row)
        {
            entity_mut.insert(ChildOf(*slot));
        }
    }

    let existing: Vec<(Entity, Entity)> =
        world.resource::<TreeIndex>().rows_in(container).collect();
    for (source, row) in existing {
        if desired_sources.contains(&source) {
            continue;
        }
        world.resource_mut::<TreeIndex>().remove(container, source);
        if let Ok(entity_mut) = world.get_entity_mut(row) {
            entity_mut.despawn();
        }
    }

    for (source, slot) in &desired {
        if world.resource::<TreeIndex>().contains(container, *source) {
            continue;
        }
        spawn_single_tree_row(world, *source, *slot, container);
    }

    let rows: Vec<(Entity, Entity)> = world.resource::<TreeIndex>().rows_in(container).collect();
    for (source, row) in rows {
        let want = !outliner_children(world, source).is_empty();
        if let Some(mut flag) = world.get_mut::<TreeNodeHasChildren>(row)
            && flag.0 != want
        {
            flag.0 = want;
        }
    }
}

pub(crate) fn reconcile_outliner(world: &mut World) {
    let containers: Vec<Entity> = world
        .run_system_cached(collect_hierarchy_containers)
        .unwrap_or_default();
    if containers.is_empty() {
        return;
    }
    let roots = current_outliner_roots(world);
    for container in containers {
        reconcile_container(world, container, &roots);
    }
}

/// Roots of the Live tree: live entities whose parent is missing or not
/// itself live (the game hierarchy can hang under authored containers the
/// game never spawned).
fn live_tree_roots(world: &mut World) -> Vec<Entity> {
    let live: Vec<Entity> = {
        let mut query = world.query_filtered::<Entity, With<LivePreview>>();
        query.iter(world).collect()
    };
    let mut roots: Vec<Entity> = live
        .into_iter()
        .filter(|&entity| match world.get::<ChildOf>(entity) {
            Some(child_of) => world.get::<LivePreview>(child_of.0).is_none(),
            None => true,
        })
        .collect();
    roots.sort_by_key(|entity| entity.index());
    roots
}

/// Despawn every Outliner tree row and reset `TreeIndex`.
pub fn despawn_tree_rows(world: &mut World) {
    let containers: Vec<Entity> = world
        .run_system_cached(collect_hierarchy_containers)
        .unwrap_or_default();
    for container in &containers {
        let children: Vec<Entity> = world
            .get::<Children>(*container)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for child in children {
            if world.get::<TreeNode>(child).is_some()
                && let Ok(entity_mut) = world.get_entity_mut(child)
            {
                entity_mut.despawn();
            }
        }
    }
    if let Some(mut index) = world.get_resource_mut::<TreeIndex>() {
        index.clear();
    }
}

/// Drop every Outliner row and reconcile when Scene/Live view mode changes.
fn sync_pie_live_outliner(mode: Res<crate::pie_mirror::PieViewMode>, mut commands: Commands) {
    if !mode.is_changed() {
        return;
    }
    commands.queue(|world: &mut World| {
        despawn_tree_rows(world);
        reconcile_outliner(world);
    });
}

/// Ancestor entities whose rows must expand, top down, so that `target`'s
/// row can be spawned in an Outliner container. Walks `ChildOf` from `target`
/// up to a root, collecting ancestors; returns them ordered from the highest
/// ancestor down to `target`'s direct parent. `target` itself is excluded.
/// Expanding each in order marks it populated so reconcile can spawn the next
/// level until `target`'s row exists.
fn reveal_path(world: &World, target: Entity) -> Vec<Entity> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cursor = target;
    seen.insert(cursor);
    while let Some(child_of) = world.get::<ChildOf>(cursor) {
        let parent = child_of.0;
        // A streamed projection can momentarily form a parent cycle while
        // entities respawn and reparent; stop rather than loop forever.
        if !seen.insert(parent) {
            break;
        }
        chain.push(parent);
        cursor = parent;
    }
    chain.reverse();
    chain
}

/// The entity the Live tree should reveal (expand ancestors to), with a
/// countdown so a target that never resolves does not spin forever.
#[derive(Resource, Default)]
pub(crate) struct RevealTarget {
    pub(crate) entity: Option<Entity>,
    pub(crate) frames_left: u8,
}

/// When the primary selection lands on an entity whose Live-tree row has not
/// been spawned yet (rows spawn lazily on expansion), arm [`RevealTarget`] so
/// the driver expands its ancestors until the row appears. Only armed in
/// Live mode.
fn watch_selection_for_reveal(
    selection: Res<Selection>,
    mode: Res<crate::pie_mirror::PieViewMode>,
    tree_index: Res<TreeIndex>,
    mut reveal: ResMut<RevealTarget>,
) {
    if !selection.is_changed() {
        return;
    }
    if *mode != crate::pie_mirror::PieViewMode::Live {
        return;
    }
    let Some(primary) = selection.primary() else {
        return;
    };
    if tree_index.contains_anywhere(primary) {
        return;
    }
    reveal.entity = Some(primary);
    reveal.frames_left = 16;
}

/// While [`RevealTarget`] is armed, expand the nearest already-rowed ancestor
/// of the target each frame. Expanding a row triggers `on_tree_node_expanded`,
/// which spawns the next level on the following flush; the driver then advances
/// to that newly rowed ancestor on the next frame. Clears the target once its
/// own row exists or the countdown runs out.
fn drive_reveal_target(world: &mut World) {
    let target = world.resource::<RevealTarget>().entity;
    let Some(target) = target else {
        return;
    };

    if world.resource::<TreeIndex>().contains_anywhere(target) {
        let mut reveal = world.resource_mut::<RevealTarget>();
        reveal.entity = None;
        reveal.frames_left = 0;
        return;
    }

    let frames_left = world.resource::<RevealTarget>().frames_left;
    if frames_left == 0 {
        world.resource_mut::<RevealTarget>().entity = None;
        return;
    }
    world.resource_mut::<RevealTarget>().frames_left = frames_left - 1;

    // Highest-to-lowest ancestor chain. Expand the first ancestor that has a
    // row somewhere but is not yet expanded. That marks the row populated so
    // the next reconcile spawns the next level.
    let path = reveal_path(world, target);
    let mut row_to_expand = None;
    'outer: for ancestor in path {
        let rows: Vec<Entity> = world
            .resource::<TreeIndex>()
            .rows_for_source(ancestor)
            .map(|(_container, row)| row)
            .collect();
        for row in rows {
            if world.get::<TreeNodeExpanded>(row).map(|e| e.0) == Some(false) {
                row_to_expand = Some(row);
                break 'outer;
            }
        }
    }

    if let Some(row) = row_to_expand {
        if let Some(mut expanded) = world.get_mut::<TreeNodeExpanded>(row) {
            expanded.0 = true;
        }
    } else if world.resource::<RevealTarget>().frames_left == 0 {
        // No rowed ancestor to expand and the budget is spent: give up so the
        // target does not linger after it became unreachable.
        world.resource_mut::<RevealTarget>().entity = None;
    }
}

/// When an entity's Name is added, update its row label in every Outliner
/// panel. Name is a label, not membership.
fn on_name_changed(
    trigger: On<Add, Name>,
    mut commands: Commands,
    name_query: Query<&Name>,
    tree_index: Res<TreeIndex>,
    tree_nodes: Query<&Children, With<TreeNode>>,
    content_query: Query<&Children, With<TreeRowContent>>,
    mut label_query: Query<&mut Text, With<TreeRowLabel>>,
) {
    let entity = trigger.event_target();

    commands.queue(move |world: &mut World| {
        refresh_row_icon(world, entity);
    });

    let Ok(name) = name_query.get(entity) else {
        return;
    };

    if !tree_index.contains_anywhere(entity) {
        return;
    }
    for (_container, tree_entity) in tree_index.rows_for_source(entity) {
        let Ok(children) = tree_nodes.get(tree_entity) else {
            continue;
        };
        for child in children.iter() {
            if let Ok(content_children) = content_query.get(child) {
                for grandchild in content_children.iter() {
                    if let Ok(mut text) = label_query.get_mut(grandchild) {
                        text.0 = name.as_str().to_string();
                        break;
                    }
                }
            }
        }
    }
}

/// Spawn a watcher entity that notifies us when Name is mutated in-place.
fn setup_name_watcher(mut commands: Commands) {
    commands
        .spawn((EditorEntity, NotifyChanged::<Name>::default()))
        .observe(on_name_mutated);
}

/// Pre-register `NotifyChanged` hooks for tree-row components during
/// Startup. `bevy_monitors`'s add-hook queues a command that calls
/// `world.schedule_scope(Update, ...)` the first time any entity with
/// `NotifyChanged<C>` spawns. If that first spawn happens while `Update`
/// is already executing (e.g. `reconcile_outliner` spawning scene tree rows
/// on workspace switch), the queued command panics with "Schedule
/// Update not found". Registering watcher entities here in Startup
/// flushes the hooks before any `Update` tick runs, so subsequent spawns
/// take the `DetectingChanges<C>` early-return branch.
fn setup_tree_node_expanded_watcher(mut commands: Commands) {
    commands.spawn(NotifyChanged::<TreeNodeExpanded>::default());
    commands.spawn(NotifyChanged::<TreeNodeHasChildren>::default());
}

/// When an entity's Name is mutated in-place (e.g. via inspector),
/// update the row label in every Outliner panel that has a row for it.
fn on_name_mutated(
    trigger: On<Mutation<Name>>,
    name_query: Query<&Name>,
    tree_index: Res<TreeIndex>,
    tree_nodes: Query<&Children, With<TreeNode>>,
    content_query: Query<&Children, With<TreeRowContent>>,
    mut label_query: Query<&mut Text, With<TreeRowLabel>>,
) {
    let entity = trigger.mutated;
    let Ok(name) = name_query.get(entity) else {
        return;
    };
    for (_container, tree_entity) in tree_index.rows_for_source(entity) {
        let Ok(children) = tree_nodes.get(tree_entity) else {
            continue;
        };
        for child in children.iter() {
            let Ok(content_children) = content_query.get(child) else {
                continue;
            };
            for grandchild in content_children.iter() {
                if let Ok(mut text) = label_query.get_mut(grandchild) {
                    text.0 = name.as_str().to_string();
                    break;
                }
            }
        }
    }
}

/// First child of `parent` that carries component `C`.
fn first_child_with<C: Component>(world: &World, parent: Entity) -> Option<Entity> {
    let children: Vec<Entity> = world.get::<Children>(parent)?.iter().collect();
    children
        .into_iter()
        .find(|&child| world.get::<C>(child).is_some())
}

/// Re-derive the icon glyph for every Outliner row of `entity`. A brush's icon
/// is registered against its `Brush` component (`registered_icon`); the
/// duplicate path streams a brush's components into the world one at a time
/// through the scene, so reconcile can spawn the row before `Brush` arrives,
/// leaving the fallback dot. Refreshing the glyph here mirrors how the label
/// refreshes on a `Name` change. Only the glyph changes: a brush root's
/// category (and so its icon color) does not depend on `Brush`.
fn refresh_row_icon(world: &mut World, entity: Entity) {
    let Some(icon) = registered_icon(world, entity) else {
        return;
    };
    let glyph = String::from(icon.unicode());
    let rows: Vec<Entity> = world
        .resource::<TreeIndex>()
        .rows_for_source(entity)
        .map(|(_container, row)| row)
        .collect();
    for row in rows {
        // TreeNode -> TreeRowContent -> TreeRowDot -> glyph Text.
        let Some(content) = first_child_with::<TreeRowContent>(world, row) else {
            continue;
        };
        let Some(dot) = first_child_with::<TreeRowDot>(world, content) else {
            continue;
        };
        let Some(glyph_text) = world.get::<Children>(dot).and_then(|c| c.iter().next()) else {
            continue;
        };
        if let Some(mut text) = world.get_mut::<Text>(glyph_text) {
            text.0 = glyph.clone();
        }
    }
}

/// A brush's outliner row icon is registered against its `Brush` component, but
/// the duplicate path writes a brush's components into the world incrementally
/// through the scene, so the row can be created before `Brush` lands and shows
/// the generic dot. Re-derive the glyph once `Brush` is present.
fn on_brush_icon_ready(trigger: On<Add, Brush>, mut commands: Commands) {
    let entity = trigger.event_target();
    commands.queue(move |world: &mut World| {
        refresh_row_icon(world, entity);
    });
}

/// First expand of a row: mark it populated so the next reconcile
/// spawns its children.
fn on_tree_node_expanded(
    trigger: On<Mutation<TreeNodeExpanded>>,
    expanded: Query<&TreeNodeExpanded>,
    mut populated: Query<&mut TreeChildrenPopulated>,
    tree_nodes: Query<&TreeNode>,
    remote_check: Query<(), With<crate::remote::entity_browser::RemoteEntityProxy>>,
) {
    let row = trigger.event_target();
    let Ok(expanded) = expanded.get(row) else {
        return;
    };
    if !expanded.0 {
        return;
    }
    let Ok(tree_node) = tree_nodes.get(row) else {
        return;
    };
    if remote_check.contains(tree_node.0) {
        return;
    }
    let Ok(mut populated) = populated.get_mut(row) else {
        return;
    };
    populated.0 = true;
}

/// Handle tree row click -> select the source entity.
/// Plain click on selected entity -> deselect. Ctrl+Click -> toggle.
fn on_tree_row_clicked(
    event: On<TreeRowClicked>,
    mut commands: Commands,
    mut selection: ResMut<Selection>,
    mut focused: ResMut<TreeFocused>,
    keyboard: Res<ButtonInput<KeyCode>>,
    parent_query: Query<&ChildOf>,
    tree_nodes: Query<Entity, With<TreeNode>>,
    remote_check: Query<(), With<crate::remote::entity_browser::RemoteEntityProxy>>,
) {
    // Skip remote entity proxies, handled by entity_browser observer
    if remote_check.contains(event.source_entity) {
        return;
    }

    let ctrl = keyboard.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]);

    if ctrl {
        selection.toggle(&mut commands, event.source_entity);
    } else if selection.is_selected(event.source_entity) {
        selection.clear(&mut commands);
    } else {
        selection.select_single(&mut commands, event.source_entity);
    }

    let content_entity = event.entity;
    if let Ok(&ChildOf(tree_row)) = parent_query.get(content_entity)
        && tree_nodes.contains(tree_row)
    {
        focused.0 = Some(tree_row);
    }
}

/// When Selected is added, highlight the corresponding row in every
/// Outliner panel.
fn on_entity_selected(
    trigger: On<Add, Selected>,
    mut commands: Commands,
    tree_index: Res<TreeIndex>,
    tree_nodes: Query<&Children, With<TreeNode>>,
    tree_row_contents: Query<Entity, With<TreeRowContent>>,
    mut bg_query: Query<&mut BackgroundColor>,
    mut border_query: Query<&mut BorderColor>,
) {
    let entity = trigger.event_target();

    for (_container, tree_entity) in tree_index.rows_for_source(entity) {
        let Ok(children) = tree_nodes.get(tree_entity) else {
            continue;
        };
        for child in children.iter() {
            if tree_row_contents.contains(child) {
                if let Ok(mut ec) = commands.get_entity(child) {
                    ec.insert(TreeRowSelected);
                }
                if let Ok(mut bg) = bg_query.get_mut(child) {
                    bg.0 = tokens::SELECTED_BG;
                }
                if let Ok(mut border) = border_query.get_mut(child) {
                    *border = BorderColor::all(tokens::SELECTED_BORDER);
                }
                break;
            }
        }
    }
}

/// When Selected is removed, unhighlight the corresponding row in
/// every Outliner panel.
fn on_entity_deselected(
    trigger: On<Remove, Selected>,
    mut commands: Commands,
    tree_index: Res<TreeIndex>,
    tree_nodes: Query<&Children, With<TreeNode>>,
    tree_row_contents: Query<Entity, With<TreeRowContent>>,
    mut bg_query: Query<&mut BackgroundColor>,
    mut border_query: Query<&mut BorderColor>,
) {
    let entity = trigger.event_target();

    for (_container, tree_entity) in tree_index.rows_for_source(entity) {
        let Ok(children) = tree_nodes.get(tree_entity) else {
            continue;
        };
        for child in children.iter() {
            if tree_row_contents.contains(child) {
                if let Ok(mut ec) = commands.get_entity(child) {
                    ec.remove::<TreeRowSelected>();
                }
                if let Ok(mut bg) = bg_query.get_mut(child) {
                    bg.0 = ROW_BG;
                }
                if let Ok(mut border) = border_query.get_mut(child) {
                    *border = BorderColor::all(Color::NONE);
                }
                break;
            }
        }
    }
}

/// Handle tree row dropped -> reparent the scene entity with undo support.
fn on_tree_row_dropped(
    event: On<TreeRowDropped>,
    mut commands: Commands,
    parent_query: Query<&ChildOf>,
) {
    let dragged = event.dragged_source;
    let target = event.target_source;

    if dragged == target {
        return;
    }

    // Cycle check: walk up from target, ensure dragged is not an ancestor
    let mut current = target;
    while let Ok(&ChildOf(parent)) = parent_query.get(current) {
        if parent == dragged {
            return;
        }
        current = parent;
    }

    commands.queue(move |world: &mut World| {
        // Inherited entities dropped outside their instance subtree get
        // unpacked: the AST adds a standalone copy under the drop target
        // and the source instance's `IsA.deleted` list grows by the
        // child's `PrefabEntityId`. The live ECS entity still needs to
        // be reparented for the visual to match.
        if is_inherited_entity(world, dragged) {
            let dragged_instance = find_instance_root(world, dragged);
            let target_instance = find_instance_root(world, target);
            if dragged_instance.is_some() && dragged_instance != target_instance {
                // The operator resolves AST keys from these entities
                // inside its queued closure (after the framework's
                // before-snapshot install reshuffles indices).
                let both_in_ast = {
                    let ast = world.resource::<jackdaw_bsn::SceneBsnAst>();
                    ast.ast_for(dragged).is_some() && ast.ast_for(target).is_some()
                };
                if both_in_ast {
                    let _ = world
                        .operator("prefab.unpack_child")
                        .settings(CallOperatorSettings {
                            creates_history_entry: true,
                            ..default()
                        })
                        .param("child_entity", dragged)
                        .param("drop_target_entity", target)
                        .call();
                    let old_parent = world.get::<ChildOf>(dragged).map(|c| c.0);
                    let mut cmd = ReparentEntity {
                        entity: dragged,
                        old_parent,
                        new_parent: Some(target),
                    };
                    cmd.execute(world);
                    world
                        .resource_mut::<CommandHistory>()
                        .undo_stack
                        .push(Box::new(cmd));
                    world.resource_mut::<CommandHistory>().redo_stack.clear();
                    return;
                }
            }
        }

        let old_parent = world.get::<ChildOf>(dragged).map(|c| c.0);
        let mut cmd = ReparentEntity {
            entity: dragged,
            old_parent,
            new_parent: Some(target),
        };
        cmd.execute(world);
        world
            .resource_mut::<CommandHistory>()
            .undo_stack
            .push(Box::new(cmd));
        world.resource_mut::<CommandHistory>().redo_stack.clear();
    });
}

/// Handle tree row dropped on root container -> deparent the scene entity.
fn on_tree_row_dropped_on_root(
    event: On<TreeRowDroppedOnRoot>,
    mut commands: Commands,
    parent_query: Query<&ChildOf, Without<EditorEntity>>,
    tree_index: Res<TreeIndex>,
) {
    let dragged = event.dragged_source;

    let old_parent = match parent_query.get(dragged) {
        Ok(child_of) => Some(child_of.0),
        Err(_) => return,
    };

    let mut cmd = ReparentEntity {
        entity: dragged,
        old_parent,
        new_parent: None,
    };

    commands.queue(move |world: &mut World| {
        // `unpack_child` requires a drop-target key, so a true unpack to
        // the project root has no operator yet. For now, dragging an
        // inherited entity to the empty root just deparents it in the
        // ECS; the AST instance keeps owning it, so the next scene
        // re-resolve will reanchor it under its instance root.
        cmd.execute(world);
        world
            .resource_mut::<CommandHistory>()
            .undo_stack
            .push(Box::new(cmd));
        world.resource_mut::<CommandHistory>().redo_stack.clear();
    });

    // Move every Outliner panel's row for this source back under its
    // own root container.
    for (container, tree_entity) in tree_index.rows_for_source(dragged) {
        commands.entity(tree_entity).try_insert(ChildOf(container));
    }
}

/// Open the hierarchy row context menu under the cursor (RMB).
#[operator(
    id = "hierarchy.open_context_menu",
    label = "Open Context Menu",
    description = "Show the context menu for the entity under the cursor.",
    allows_undo = false
)]
pub(crate) fn hierarchy_open_context_menu(
    _: In<OperatorParameters>,
    mut commands: Commands,
    mut state: ResMut<ContextMenuState>,
    cursor: crate::viewport::UiCursorPos,
    selection: Res<Selection>,
    tree_row_contents: Query<(Entity, &ChildOf), With<TreeRowContent>>,
    tree_nodes: Query<&TreeNode>,
    computed_nodes: Query<(&ComputedNode, &UiGlobalTransform), With<TreeRowContent>>,
    extension_add_entries: Query<&jackdaw_api_internal::lifecycle::RegisteredMenuEntry>,
    q_isa: Query<(), With<crate::prefab::IsA>>,
) -> OperatorResult {
    let cursor_pos = cursor.get()?;

    // Close any existing context menu
    if let Some(menu) = state.menu_entity.take()
        && let Ok(mut ec) = commands.get_entity(menu)
    {
        ec.despawn();
    }

    // Find which tree row content the cursor is over by hit testing
    let mut target_source = None;
    for (content_entity, child_of) in &tree_row_contents {
        let Ok((computed, global_transform)) = computed_nodes.get(content_entity) else {
            continue;
        };
        let inv_scale = computed.inverse_scale_factor();
        let size = computed.size() * inv_scale;
        let (_, _, translation) = global_transform.to_scale_angle_translation();
        let pos = translation * inv_scale;
        let half = size / 2.0;
        let rect = Rect::from_center_half_size(pos, half);
        if rect.contains(cursor_pos)
            && let Ok(tree_node) = tree_nodes.get(child_of.0)
        {
            target_source = Some(tree_node.0);
            break;
        }
    }

    let target = target_source?;

    // If the right-clicked entity isn't selected, select it
    if !selection.is_selected(target) {
        commands.queue(move |world: &mut World| {
            let old_entities: Vec<Entity> = world.resource::<Selection>().entities.clone();
            let mut selection = world.resource_mut::<Selection>();
            selection.entities.clear();
            selection.entities.push(target);

            for &e in &old_entities {
                if e != target
                    && let Ok(mut ec) = world.get_entity_mut(e)
                {
                    ec.remove::<Selected>();
                }
            }
            if let Ok(mut ec) = world.get_entity_mut(target) {
                ec.insert(Selected);
            }
        });
    }

    // Built-in context menu items. The "Add Child ..." entries are the
    // parent-aware variant: they spawn the entity and reparent it under
    // the right-clicked target.
    let mut owned_items: Vec<(String, String)> = vec![
        (
            "hierarchy.focus".into(),
            "Focus                    F".into(),
        ),
        ("hierarchy.rename".into(), "Rename              F2".into()),
        (
            "hierarchy.duplicate".into(),
            "Duplicate        Ctrl+D".into(),
        ),
        ("hierarchy.delete".into(), "Delete             Del".into()),
        (
            "hierarchy.save_prefab".into(),
            "Save Selection as Prefab...".into(),
        ),
        (
            "hierarchy.save_scene_as_prefab".into(),
            "Save Scene as Prefab...".into(),
        ),
        ("hierarchy.add_cube".into(), "Add Child Cube".into()),
        ("hierarchy.add_sphere".into(), "Add Child Sphere".into()),
        ("hierarchy.add_light".into(), "Add Child Light".into()),
        ("hierarchy.add_empty".into(), "Add Child Empty".into()),
    ];

    // If the right-clicked target is a prefab instance root (has IsA),
    // expose prefab-instance specific actions above the generic ones.
    if q_isa.get(target).is_ok() {
        owned_items.insert(
            0,
            (
                "hierarchy.prefab.revert_all".into(),
                "Revert All Overrides".into(),
            ),
        );
        owned_items.insert(
            1,
            (
                "hierarchy.prefab.save_as_variant".into(),
                "Save as Variant...".into(),
            ),
        );
        owned_items.insert(
            2,
            (
                "hierarchy.prefab.apply_all_to_source".into(),
                "Apply All Changes to Prefab Source".into(),
            ),
        );
        owned_items.insert(
            3,
            (
                "hierarchy.prefab.unbundle_instance".into(),
                "Unbundle Prefab Instance".into(),
            ),
        );
    }

    // Append extension-contributed Add entries from the same source the
    // toolbar Add menu and the Add Entity picker use. One
    // `register_menu_entry` call therefore surfaces in all three places.
    let mut ext_rows: Vec<(String, String)> = extension_add_entries
        .iter()
        .filter(|entry| entry.menu == TopLevelMenu::Add)
        .map(|entry| {
            (
                format!("{OP_PREFIX}{}", entry.operator_id),
                format!("Add {}", entry.label),
            )
        })
        .collect();
    ext_rows.sort_by(|a, b| a.1.cmp(&b.1));
    owned_items.extend(ext_rows);

    let items: Vec<(&str, &str)> = owned_items
        .iter()
        .map(|(a, l)| (a.as_str(), l.as_str()))
        .collect();

    let menu = spawn_context_menu(&mut commands, cursor_pos, Some(target), &items);
    state.menu_entity = Some(menu);
    state.target_entity = Some(target);
    OperatorResult::Finished
}

/// Handle context menu actions for hierarchy operations.
fn on_context_menu_action(
    event: On<ContextMenuAction>,
    mut commands: Commands,
    global_transforms: Query<&GlobalTransform>,
    mut camera_query: Query<&mut Transform, With<jackdaw_camera::JackdawCameraSettings>>,
) {
    let target_entity = event.target_entity;

    match event.action.as_str() {
        "hierarchy.focus" => {
            if let Some(target) = target_entity
                && let Ok(global_tf) = global_transforms.get(target)
            {
                let target_pos = global_tf.translation();
                let scale = global_tf.compute_transform().scale;
                let dist = (scale.length() * 3.0).max(5.0);

                for mut transform in &mut camera_query {
                    let forward = transform.forward().as_vec3();
                    transform.translation = target_pos - forward * dist;
                    *transform = transform.looking_at(target_pos, Vec3::Y);
                }
            }
        }
        "hierarchy.rename" => {
            if let Some(target) = target_entity {
                commands
                    .operator(RenameBeginOp::ID)
                    .param("entity", target)
                    .call();
            }
        }
        "hierarchy.duplicate" => {
            commands.queue(|world: &mut World| {
                entity_ops::duplicate_selected(world);
            });
        }
        "hierarchy.delete" => {
            commands.queue(|world: &mut World| {
                entity_ops::delete_selected(world);
            });
        }
        "hierarchy.add_cube" => add_child_entity(
            &mut commands,
            target_entity,
            entity_ops::EntityTemplate::Cube,
        ),
        "hierarchy.add_sphere" => add_child_entity(
            &mut commands,
            target_entity,
            entity_ops::EntityTemplate::Sphere,
        ),
        "hierarchy.add_light" => add_child_entity(
            &mut commands,
            target_entity,
            entity_ops::EntityTemplate::PointLight,
        ),
        "hierarchy.add_empty" => add_child_entity(
            &mut commands,
            target_entity,
            entity_ops::EntityTemplate::Empty,
        ),
        "hierarchy.save_prefab" => {
            commands.queue(move |world: &mut World| {
                // Prefer the right-clicked entity. The current Selection
                // is only used when the user right-clicked an entity that
                // IS part of the selection (multi-select save). When the
                // right-click lands on something outside the selection,
                // the user expects that row to be the target -- otherwise
                // they'd silently save the wrong entity tree.
                let selection: Vec<Entity> = world
                    .resource::<crate::selection::Selection>()
                    .entities
                    .clone();
                let roots = match target_entity {
                    Some(target) if selection.contains(&target) => selection,
                    Some(target) => vec![target],
                    None => selection,
                };
                if roots.is_empty() {
                    return;
                }
                info!(
                    "hierarchy.save_prefab: target_entity={:?}, selection_len={}, roots_len={}",
                    target_entity,
                    world
                        .resource::<crate::selection::Selection>()
                        .entities
                        .len(),
                    roots.len(),
                );
                let default_name = roots
                    .first()
                    .and_then(|e| world.get::<Name>(*e).map(|n| n.as_str().to_string()))
                    .unwrap_or_else(|| "prefab".to_string());
                world.resource_mut::<PendingPrefabSave>().roots = roots;
                world.resource_mut::<PendingPrefabSave>().mode = PrefabSaveMode::Prefab;
                world.resource_mut::<PendingPrefabDefaultName>().0 = default_name;
            });
            commands.trigger(jackdaw_feathers::dialog::OpenDialogEvent::new(
                "Save as Prefab",
                "Save",
            ));
        }
        "hierarchy.save_scene_as_prefab" => {
            commands.queue(move |world: &mut World| {
                let scenes = world.resource::<crate::scenes::Scenes>();
                let active = scenes.active;
                let default_name = scenes
                    .tabs
                    .get(active)
                    .map(|t| t.display_name.trim().to_string())
                    .filter(|s| !s.is_empty() && !s.starts_with("untitled"))
                    .unwrap_or_else(|| "prefab".to_string());

                world.resource_mut::<PendingPrefabSave>().roots = Vec::new();
                world.resource_mut::<PendingPrefabSave>().mode = PrefabSaveMode::Scene;
                world.resource_mut::<PendingPrefabDefaultName>().0 = default_name;
            });
            commands.trigger(jackdaw_feathers::dialog::OpenDialogEvent::new(
                "Save Scene as Prefab",
                "Save",
            ));
        }
        "hierarchy.prefab.revert_all" => {
            let Some(target) = target_entity else {
                return;
            };
            commands.queue(move |world: &mut World| {
                // The operator resolves the AST key from this entity
                // inside its queued closure (after the framework's
                // before-snapshot install reshuffles indices).
                if world
                    .resource::<jackdaw_bsn::SceneBsnAst>()
                    .ast_for(target)
                    .is_none()
                {
                    return;
                }
                let _ = world
                    .operator("prefab.revert_all")
                    .settings(CallOperatorSettings {
                        creates_history_entry: true,
                        ..default()
                    })
                    .param("instance_entity", target)
                    .call();
            });
        }
        "hierarchy.prefab.save_as_variant" => {
            let Some(target) = target_entity else {
                return;
            };
            commands.queue(move |world: &mut World| {
                let default_name = world
                    .get::<Name>(target)
                    .map(|n| format!("{}_variant", n.as_str()))
                    .unwrap_or_else(|| "variant".to_string());
                world.resource_mut::<PendingPrefabSave>().roots = vec![target];
                world.resource_mut::<PendingPrefabSave>().mode = PrefabSaveMode::Variant;
                world.resource_mut::<PendingPrefabDefaultName>().0 = default_name;
            });
            commands.trigger(jackdaw_feathers::dialog::OpenDialogEvent::new(
                "Save as Variant",
                "Save",
            ));
        }
        "hierarchy.prefab.apply_all_to_source" => {
            let Some(target) = target_entity else {
                return;
            };
            commands.queue(move |world: &mut World| {
                let node = {
                    let ast = world.resource::<jackdaw_bsn::SceneBsnAst>();
                    ast.ast_for(target)
                };
                let Some(node) = node else { return };
                crate::prefab::operators::apply_all_overrides_to_source(world, node);
            });
        }
        "hierarchy.prefab.unbundle_instance" => {
            let Some(target) = target_entity else {
                return;
            };
            commands.queue(move |world: &mut World| {
                let _ = world
                    .operator("prefab.unbundle_instance")
                    .settings(CallOperatorSettings {
                        creates_history_entry: true,
                        ..default()
                    })
                    .param("instance_entity", target)
                    .call();
            });
        }
        action if action.starts_with(OP_PREFIX) => {
            // Extension-contributed Add entry. Dispatch through the same
            // path as the toolbar Add menu and the Add Entity picker so
            // operators behave identically regardless of which surface
            // invoked them.
            let operator_id = action.strip_prefix(OP_PREFIX).unwrap().to_string();
            commands.queue(move |world: &mut World| {
                world
                    .operator(operator_id)
                    .settings(CallOperatorSettings {
                        execution_context: ExecutionContext::Invoke,
                        creates_history_entry: true,
                    })
                    .call()
            });
        }
        _ => {}
    }
}

/// Spawn an entity from `template` and reparent it under `parent` (if
/// provided). Goes through the AST-aware `set_parent` so the live
/// scene document stays in sync with the ECS hierarchy.
fn add_child_entity(
    commands: &mut Commands,
    parent: Option<Entity>,
    template: entity_ops::EntityTemplate,
) {
    let Some(parent) = parent else {
        return;
    };
    commands.queue(move |world: &mut World| {
        entity_ops::create_entity_in_world(world, template);
        let selection = world.resource::<Selection>();
        if let Some(new_entity) = selection.primary() {
            crate::commands::set_parent(world, new_entity, Some(parent));
        }
    });
}

/// Toggle entity visibility when the eye icon is clicked. This is an
/// editor-local view state: it sets ECS `Visibility` only and is never written
/// to the `.jsn` scene, so it does not re-apply on rebuild. The eye glyph is
/// synced to match so hidden state is always visible.
fn on_visibility_toggled(
    event: On<TreeRowVisibilityToggled>,
    mut commands: Commands,
    visibility_query: Query<&Visibility>,
) {
    let source = event.source_entity;

    let current = visibility_query
        .get(source)
        .copied()
        .unwrap_or(Visibility::Inherited);

    let new_visibility = match current {
        Visibility::Hidden => Visibility::Inherited,
        _ => Visibility::Hidden,
    };
    let hidden = matches!(new_visibility, Visibility::Hidden);

    commands.queue(move |world: &mut World| {
        if let Ok(mut ec) = world.get_entity_mut(source) {
            ec.insert(new_visibility);
        }
        refresh_row_visibility_glyph(world, source, hidden);
    });
}

/// Sync the eye toggle glyph for every Outliner row of `entity` to its
/// visibility state, dimming when hidden so the state always reads correctly.
fn refresh_row_visibility_glyph(world: &mut World, entity: Entity, hidden: bool) {
    use jackdaw_feathers::icons::Icon;
    let glyph = String::from(if hidden { Icon::EyeOff } else { Icon::Eye }.unicode());
    let alpha = if hidden { 0.7 } else { 0.4 };
    let rows: Vec<Entity> = world
        .resource::<TreeIndex>()
        .rows_for_source(entity)
        .map(|(_container, row)| row)
        .collect();
    for row in rows {
        // TreeNode -> TreeRowContent -> TreeRowVisibilityToggle -> glyph Text.
        let Some(content) = first_child_with::<TreeRowContent>(world, row) else {
            continue;
        };
        let Some(toggle) = first_child_with::<TreeRowVisibilityToggle>(world, content) else {
            continue;
        };
        let Some(glyph_text) = world.get::<Children>(toggle).and_then(|c| c.iter().next()) else {
            continue;
        };
        if let Some(mut text) = world.get_mut::<Text>(glyph_text) {
            text.0 = glyph.clone();
        }
        if let Some(mut color) = world.get_mut::<TextColor>(glyph_text) {
            color.0 = color.0.with_alpha(alpha);
        }
    }
}

pub(crate) fn add_to_extension(ctx: &mut ExtensionContext) {
    ctx.register_operator::<RenameBeginOp>()
        .register_operator::<HierarchyOpenContextMenuOp>()
        .register_operator::<PrefabSaveAsPrefabOp>()
        .register_operator::<PrefabSaveSceneAsPrefabOp>()
        .register_operator::<PrefabSaveAsVariantOp>()
        .register_operator::<crate::prefab::operators::PrefabSaveOp>()
        .register_operator::<crate::prefab::operators::PrefabSpawnInstanceOp>()
        .register_operator::<crate::prefab::operators::PrefabRevertFieldOp>()
        .register_operator::<crate::prefab::operators::PrefabRevertComponentOp>()
        .register_operator::<crate::prefab::operators::PrefabRevertAllOp>()
        .register_operator::<crate::prefab::operators::PrefabApplyToSourceOp>()
        .register_operator::<crate::prefab::operators::PrefabBulkApplyInSceneOp>()
        .register_operator::<crate::prefab::operators::PrefabSaveAsVariantEntityOp>()
        .register_operator::<crate::prefab::operators::PrefabUnpackChildOp>()
        .register_operator::<crate::prefab::operators::PrefabUnbundleInstanceOp>()
        .register_operator::<crate::prefab::operators::PrefabRepairSelfCyclesOp>();
    let ext = ctx.id();
    // Deferred: condition is not bare Press::default() (mouse button + Press).
    ctx.spawn((
        Action::<HierarchyOpenContextMenuOp>::new(),
        ActionOf::<crate::core_extension::CoreExtensionInputContext>::new(ext),
        bindings![(MouseButton::Right, Press::default())],
    ));
    ctx.bind_operator::<crate::core_extension::CoreExtensionInputContext, RenameBeginOp>([
        PresetInput::key("F2"),
    ]);
}

/// Marker for inline rename `text_edit` entity, linking back to the label entity and source entity.
#[derive(Component)]
struct InlineRenameInput {
    label_entity: Entity,
    source_entity: Entity,
}

fn on_tree_row_start_rename(event: On<TreeRowStartRename>, mut commands: Commands) {
    let target = event.source_entity;
    commands
        .operator(RenameBeginOp::ID)
        .param("entity", target)
        .call();
}

/// `is_available` for `hierarchy.rename_begin`: only fires when no
/// inline rename is already in progress.
fn no_rename_in_progress(rename_check: Query<(), With<InlineRenameInput>>) -> bool {
    rename_check.is_empty()
}

/// Pick the entity to rename: the explicit `entity` operator
/// parameter wins (used by the context-menu "Rename" action and the
/// `TreeRowStartRename` event), otherwise fall back to the primary
/// selection so a bare F2 press renames whatever the user has
/// highlighted in the outliner. Pulled out so the regression check
/// for the F2-without-selection path can run as a unit test.
pub(crate) fn resolve_rename_target(
    params: &OperatorParameters,
    selection: &Selection,
) -> Option<Entity> {
    params.as_entity("entity").or_else(|| selection.primary())
}

fn entity_name(names: &Query<&Name>, entity: Entity) -> String {
    names
        .get(entity)
        .map(|n| n.as_str().to_string())
        .unwrap_or_default()
}

/// Resolve the label entity and its containing row for a scene
/// entity's tree node. With multi-instance Outliner panels, returns
/// the first match across all containers; the inline-rename UX
/// targets one panel at a time and the others stay synchronised
/// once the rename commits via `on_name_changed` / `on_name_mutated`.
fn find_rename_targets(
    source: Entity,
    tree_index: &TreeIndex,
    tree_nodes: &Query<&Children, With<TreeNode>>,
    content_query: &Query<(Entity, &Children), With<TreeRowContent>>,
    label_query: &Query<Entity, With<TreeRowLabel>>,
) -> Option<(Entity, Entity)> {
    for (_container, tree_entity) in tree_index.rows_for_source(source) {
        let Ok(children) = tree_nodes.get(tree_entity) else {
            continue;
        };
        for child in children.iter() {
            if let Ok((content_e, content_children)) = content_query.get(child) {
                for grandchild in content_children.iter() {
                    if label_query.contains(grandchild) {
                        return Some((grandchild, content_e));
                    }
                }
            }
        }
    }
    None
}

/// Custom command: drop the inline-rename marker from a tree-row label
/// and restore its displayed text + visibility. Issued from rename
/// commit/cancel paths so the queue boundary is explicit.
struct RestoreLabel {
    label_entity: Entity,
    text: String,
}

impl Command for RestoreLabel {
    type Out = ();

    fn apply(self, world: &mut World) -> Self::Out {
        let Ok(mut ec) = world.get_entity_mut(self.label_entity) else {
            return;
        };
        ec.remove::<TreeRowInlineRename>();
        ec.insert(Text::new(self.text));
        if let Some(mut node) = ec.get_mut::<Node>() {
            node.display = Display::Flex;
        }
    }
}

/// Begin inline rename of an entity in the hierarchy tree.
#[operator(
    id = "hierarchy.rename_begin",
    label = "Rename Entity",
    description = "Rename the selected entity in the hierarchy.",
    modal = true,
    cancel = cancel_rename_begin,
    is_available = no_rename_in_progress,
    params(entity(Entity, doc = "Scene entity to rename.")),
)]
pub fn rename_begin(
    params: In<OperatorParameters>,
    mut commands: Commands,
    tree_index: Res<TreeIndex>,
    tree_nodes: Query<&Children, With<TreeNode>>,
    content_query: Query<(Entity, &Children), With<TreeRowContent>>,
    label_query: Query<Entity, With<TreeRowLabel>>,
    names: Query<&Name>,
    rename_inputs: Query<(), With<InlineRenameInput>>,
    active: ActiveModalQuery,
    selection: Res<Selection>,
) -> OperatorResult {
    if active.is_modal_running() {
        return if rename_inputs.is_empty() {
            OperatorResult::Finished
        } else {
            OperatorResult::Running
        };
    }

    let source = resolve_rename_target(&params, &selection)?;
    let (label_entity, content_entity) = find_rename_targets(
        source,
        &tree_index,
        &tree_nodes,
        &content_query,
        &label_query,
    )?;

    commands.entity(label_entity).insert(TreeRowInlineRename);
    commands
        .entity(label_entity)
        .entry::<Node>()
        .and_modify(|mut node| {
            node.display = Display::None;
        });

    commands.spawn((
        InlineRenameInput {
            label_entity,
            source_entity: source,
        },
        text_edit::text_edit(
            TextEditProps::default()
                .with_default_value(entity_name(&names, source))
                .allow_empty(),
        ),
        ChildOf(content_entity),
    ));
    OperatorResult::Running
}

fn cancel_rename_begin(
    mut commands: Commands,
    rename_query: Query<(Entity, &InlineRenameInput)>,
    names: Query<&Name>,
    mut input_focus: ResMut<InputFocus>,
) {
    for (rename_entity, inline_rename) in &rename_query {
        input_focus.clear();
        let original = entity_name(&names, inline_rename.source_entity);
        commands.queue(RestoreLabel {
            label_entity: inline_rename.label_entity,
            text: original,
        });
        commands.entity(rename_entity).despawn();
    }
}

/// Auto-focus inline rename `text_edit` inputs one frame after spawn.
fn auto_focus_inline_rename(
    rename_inputs: Query<(Entity, &InlineRenameInput, &Children)>,
    wrappers: Query<&jackdaw_feathers::text_edit::TextEditConfig>,
    wrapper_children: Query<&Children>,
    editor_text_edits: Query<Entity, With<EditorTextEdit>>,
    mut input_focus: ResMut<InputFocus>,
) {
    for (_rename_entity, _inline, children) in &rename_inputs {
        // The text_edit outer entity has children: [wrapper] which has children: [..., EditorTextEdit]
        for child in children.iter() {
            if wrappers.contains(child) {
                // this is the label/wrapper -- skip, we need the actual wrapper node
                continue;
            }
            // child might be the wrapper entity (has TextEditWrapper inside)
            if let Ok(wrapper_kids) = wrapper_children.get(child) {
                for wk in wrapper_kids.iter() {
                    if editor_text_edits.contains(wk) {
                        if input_focus.get() != Some(wk) {
                            input_focus.set(wk, FocusCause::Pressed);
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Handle `TextEditCommitEvent` for inline renames.
fn handle_inline_rename_commit(
    event: On<TextEditCommitEvent>,
    rename_inputs: Query<(Entity, &InlineRenameInput)>,
    child_of_query: Query<&ChildOf>,
    mut commands: Commands,
    mut input_focus: ResMut<InputFocus>,
) {
    // Walk up from the committed entity to find if it belongs to an InlineRenameInput
    // event.entity is the inner EditorTextEdit -> parent is wrapper -> parent is text_edit outer -> parent is content
    // The InlineRenameInput is on the text_edit outer entity
    let mut current = event.entity;
    let mut found = None;
    for _ in 0..4 {
        let Ok(child_of) = child_of_query.get(current) else {
            break;
        };
        if let Ok((rename_entity, inline_rename)) = rename_inputs.get(child_of.parent()) {
            found = Some((
                rename_entity,
                inline_rename.label_entity,
                inline_rename.source_entity,
            ));
            break;
        }
        current = child_of.parent();
    }

    let Some((rename_entity, label_entity, source_entity)) = found else {
        return;
    };

    input_focus.clear();
    commands.queue(RestoreLabel {
        label_entity,
        text: event.text.clone(),
    });
    commands.entity(rename_entity).despawn();

    // Trigger the rename
    commands.trigger(TreeRowRenamed {
        entity: label_entity,
        source_entity,
        new_name: event.text.clone(),
    });
}

/// Commit inline rename: update Name with undo.
fn on_tree_row_renamed(event: On<TreeRowRenamed>, mut commands: Commands, names: Query<&Name>) {
    let source = event.source_entity;
    let new_name = event.new_name.clone();

    // Apply name change with undo
    let old_name = names
        .get(source)
        .map(|n| n.as_str().to_string())
        .unwrap_or_default();

    if old_name == new_name {
        return;
    }

    commands.queue(move |world: &mut World| {
        let old_value = if old_name.is_empty() {
            None
        } else {
            Some(jackdaw_bsn::BsnValue::String(old_name))
        };
        let cmd = SetBsnField {
            entity: source,
            type_path: crate::commands::NAME_TYPE_PATH.to_string(),
            field_path: String::new(),
            old_value,
            new_value: jackdaw_bsn::BsnValue::String(new_name),
            was_derived: false,
        };
        let mut cmd = Box::new(cmd);
        cmd.execute(world);
        let mut history = world.resource_mut::<CommandHistory>();
        history.push_executed(cmd);
    });
}

/// When the prefab dialog opens, populate its children slot with a name input.
/// The slot is spawned without a `Children` component (Bevy only adds it on
/// first parenting), so this query MUST NOT require `&Children` -- it would
/// never match a fresh slot.
fn populate_prefab_dialog(
    mut commands: Commands,
    pending: Res<PendingPrefabSave>,
    default_name: Res<PendingPrefabDefaultName>,
    slots: Query<Entity, With<DialogChildrenSlot>>,
    existing_inputs: Query<(), With<PrefabNameInput>>,
) {
    // `Scene` mode targets the whole active tab, so an empty
    // `pending.roots` is meaningful there. Every other mode needs at
    // least one pending root to display the dialog for.
    if pending.roots.is_empty() && !matches!(pending.mode, PrefabSaveMode::Scene) {
        return;
    }
    // Idempotent: once the input exists, subsequent ticks bail here.
    if !existing_inputs.is_empty() {
        return;
    }
    for slot_entity in &slots {
        commands.spawn((
            PrefabNameInput,
            text_edit::text_edit(
                TextEditProps::default()
                    .with_placeholder("Prefab name...")
                    .with_default_value(default_name.0.clone())
                    .allow_empty(),
            ),
            ChildOf(slot_entity),
        ));
    }
}

/// When the dialog's action button is clicked, dispatch the matching
/// prefab save operator. Routing through the operator system gives the
/// save a log entry, an extension-API surface, and a consistent
/// invocation path with the rest of the editor.
fn on_prefab_dialog_action(
    _event: On<DialogActionEvent>,
    mut commands: Commands,
    pending: Res<PendingPrefabSave>,
    name_inputs: Query<&TextEditValue, With<PrefabNameInput>>,
) {
    // `Scene` mode operates on the active tab's whole AST, so an empty
    // `pending.roots` is expected. Every other mode needs at least one
    // pending root to package.
    if pending.roots.is_empty() && !matches!(pending.mode, PrefabSaveMode::Scene) {
        return;
    }
    let name = name_inputs
        .iter()
        .next()
        .map(|input| input.0.trim().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        warn!("save prefab cancelled: name is empty");
        return;
    }
    let op_id = match pending.mode {
        PrefabSaveMode::Prefab => PrefabSaveAsPrefabOp::ID,
        PrefabSaveMode::Scene => PrefabSaveSceneAsPrefabOp::ID,
        PrefabSaveMode::Variant => PrefabSaveAsVariantOp::ID,
    };
    commands
        .operator(op_id)
        .settings(CallOperatorSettings {
            creates_history_entry: true,
            ..default()
        })
        .param("name", name)
        .call();
}

/// Save the entities listed in `PendingPrefabSave` as a new prefab file
/// at `assets/prefabs/<name>.jsn` (project-relative). Clears the
/// pending state after running.
#[operator(
    id = "prefab.save_as_prefab",
    label = "Save as Prefab",
    description = "Write the pending entity roots out as a new prefab file.",
    allows_undo = true,
    params(name(String, doc = "File name (without extension)."))
)]
pub fn prefab_save_as_prefab(
    params: In<OperatorParameters>,
    mut commands: Commands,
) -> OperatorResult {
    let Some(name) = params.as_str("name").map(str::to_string) else {
        warn!("prefab.save_as_prefab: missing `name` param");
        return OperatorResult::Cancelled;
    };
    commands.queue(move |world: &mut World| {
        let roots = world.resource::<PendingPrefabSave>().roots.clone();
        if roots.is_empty() {
            warn!("prefab.save_as_prefab: no pending roots");
            return;
        }
        let target = match world.get_resource::<crate::project::ProjectRoot>() {
            Some(root) => root.root.join("assets/prefabs").join(format!("{name}.bsn")),
            None => std::path::PathBuf::from(format!("{name}.bsn")),
        };
        info!(
            "prefab.save_as_prefab: bundling {} root(s) into {}",
            roots.len(),
            target.display()
        );
        crate::prefab::operators::save_as_prefab_from_selection(world, &roots, &target);
        let mut pending = world.resource_mut::<PendingPrefabSave>();
        pending.roots.clear();
        pending.mode = PrefabSaveMode::Prefab;
    });
    OperatorResult::Finished
}

/// Save the entire active scene tab as a prefab file and convert
/// the tab itself into a prefab tab. The active tab's
/// `TabContent` switches to `Prefab(path)`, `TabKind` becomes
/// `Prefab`, and Ctrl+S routes through the prefab save branch from
/// that point on.
#[operator(
    id = "prefab.save_scene_as_prefab",
    label = "Save Scene as Prefab",
    description = "Write the active scene tab out as a new prefab file and convert the tab into a prefab tab.",
    allows_undo = true,
    params(name(String, doc = "File name (without extension)."))
)]
pub fn prefab_save_scene_as_prefab(
    params: In<OperatorParameters>,
    mut commands: Commands,
) -> OperatorResult {
    let name = params
        .as_str("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            warn!("prefab.save_scene_as_prefab: no name provided; defaulting to 'prefab'");
            "prefab".to_string()
        });

    commands.queue(move |world: &mut World| {
        let target = match world.get_resource::<crate::project::ProjectRoot>() {
            Some(root) => root.root.join("assets/prefabs").join(format!("{name}.bsn")),
            None => std::path::PathBuf::from(format!("{name}.bsn")),
        };
        crate::prefab::operators::save_scene_as_prefab(world, &target);
        let mut pending = world.resource_mut::<PendingPrefabSave>();
        pending.roots.clear();
        pending.mode = PrefabSaveMode::Prefab;
    });
    OperatorResult::Finished
}

/// Save the first entity in `PendingPrefabSave` as a variant prefab
/// file at `assets/prefabs/<name>.jsn`. The new prefab carries both
/// `Prefab` and `IsA` (pointing at the original prefab) plus any
/// instance overrides.
#[operator(
    id = "prefab.save_as_variant",
    label = "Save as Variant",
    description = "Write the pending instance out as a variant prefab.",
    allows_undo = true,
    params(name(String, doc = "File name (without extension)."))
)]
pub fn prefab_save_as_variant(
    params: In<OperatorParameters>,
    mut commands: Commands,
) -> OperatorResult {
    let Some(name) = params.as_str("name").map(str::to_string) else {
        warn!("prefab.save_as_variant: missing `name` param");
        return OperatorResult::Cancelled;
    };
    commands.queue(move |world: &mut World| {
        let root = world.resource::<PendingPrefabSave>().roots.first().copied();
        let Some(root) = root else {
            warn!("prefab.save_as_variant: no pending root");
            return;
        };
        let target = match world.get_resource::<crate::project::ProjectRoot>() {
            Some(p) => p.root.join("assets/prefabs").join(format!("{name}.bsn")),
            None => std::path::PathBuf::from(format!("{name}.bsn")),
        };
        crate::prefab::operators::save_as_variant(world, root, &target);
        let mut pending = world.resource_mut::<PendingPrefabSave>();
        pending.roots.clear();
        pending.mode = PrefabSaveMode::Prefab;
    });
    OperatorResult::Finished
}

/// Filter hierarchy tree rows based on the filter text input.
fn apply_hierarchy_filter(
    filter_input: Query<&TextEditValue, (With<HierarchyFilter>, Changed<TextEditValue>)>,
    tree_nodes: Query<(Entity, &TreeNode)>,
    names: Query<&Name>,
    parent_query: Query<&ChildOf>,
    tree_row_children_query: Query<(), With<TreeRowChildren>>,
    mut display_query: Query<&mut Node>,
) {
    let Ok(text_edit_value) = filter_input.single() else {
        return;
    };

    let filter = text_edit_value.0.trim().to_lowercase();

    if filter.is_empty() {
        for (tree_entity, _) in &tree_nodes {
            if let Ok(mut node) = display_query.get_mut(tree_entity) {
                node.display = Display::Flex;
            }
        }
        return;
    }

    // First pass: determine which source entities match the filter
    let mut visible_tree_entities: HashSet<Entity> = HashSet::new();

    for (tree_entity, tree_node) in &tree_nodes {
        let label = names
            .get(tree_node.0)
            .map(|n| n.as_str().to_lowercase())
            .unwrap_or_else(|_| format!("entity {}", tree_node.0).to_lowercase());
        let matches = label.contains(&filter);

        if matches {
            visible_tree_entities.insert(tree_entity);

            // Walk up ancestors: tree row -> ChildOf -> TreeRowChildren -> ChildOf -> parent tree row
            let mut current = tree_entity;
            while let Ok(&ChildOf(parent)) = parent_query.get(current) {
                if tree_row_children_query.contains(parent) {
                    if let Ok(&ChildOf(grandparent)) = parent_query.get(parent) {
                        visible_tree_entities.insert(grandparent);
                        current = grandparent;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }

    // Second pass: set display on all tree rows
    for (tree_entity, _) in &tree_nodes {
        if let Ok(mut node) = display_query.get_mut(tree_entity) {
            node.display = if visible_tree_entities.contains(&tree_entity) {
                Display::Flex
            } else {
                Display::None
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jackdaw_api_internal::operator::OperatorParameters;
    use jackdaw_scene_types::PropertyValue;
    use std::collections::BTreeMap;

    fn empty_params() -> OperatorParameters {
        OperatorParameters(BTreeMap::new())
    }

    fn params_with_entity(key: &str, entity: Entity) -> OperatorParameters {
        let mut map = BTreeMap::new();
        map.insert(key.to_string(), PropertyValue::Entity(entity));
        OperatorParameters(map)
    }

    #[test]
    fn scene_root_tag_classifies_as_scene() {
        let mut world = World::new();
        let root = world.spawn(jackdaw_scene_types::SceneRootTag).id();
        let plain = world.spawn_empty().id();
        assert_eq!(classify_entity(&world, root), EntityCategory::Scene);
        assert_ne!(classify_entity(&world, plain), EntityCategory::Scene);
    }

    #[test]
    fn gltf_descendants_classify_as_asset_parts() {
        // The glTF root is authored; everything the loader spawned under it is
        // not, including the mesh leaf, which would otherwise read as Mesh.
        let mut world = World::new();
        let root = world
            .spawn(jackdaw_scene_types::GltfSource {
                path: "models/dungeon.glb".into(),
                scene_index: 0,
            })
            .id();
        let scene = world.spawn(ChildOf(root)).id();
        let mesh_leaf = world.spawn((ChildOf(scene), Mesh3d::default())).id();

        assert_eq!(classify_entity(&world, root), EntityCategory::Scene);
        assert_eq!(classify_entity(&world, scene), EntityCategory::AssetPart);
        assert_eq!(
            classify_entity(&world, mesh_leaf),
            EntityCategory::AssetPart
        );

        // An authored mesh outside any glTF subtree is unaffected.
        let authored_mesh = world.spawn(Mesh3d::default()).id();
        assert_eq!(classify_entity(&world, authored_mesh), EntityCategory::Mesh);
    }

    #[test]
    fn authored_children_of_a_model_keep_their_own_category() {
        // Parenting your own entity under a model is normal (a light on a lamp
        // prop). It has a document node, so it stays editable and must not be
        // lumped in with the nodes the loader spawned.
        let mut world = World::new();
        world.insert_resource(jackdaw_bsn::SceneBsnAst::default());
        let root = world
            .spawn(jackdaw_scene_types::GltfSource {
                path: "models/dungeon.glb".into(),
                scene_index: 0,
            })
            .id();

        let loader_node = world.spawn(ChildOf(root)).id();
        assert_eq!(
            classify_entity(&world, loader_node),
            EntityCategory::AssetPart
        );

        let authored = world.spawn((ChildOf(root), PointLight::default())).id();
        jackdaw_bsn::create_entity_in_ast(&mut world, authored, None);
        assert_eq!(classify_entity(&world, authored), EntityCategory::Light);
    }

    fn document_entity(world: &mut World) -> Entity {
        world.init_resource::<jackdaw_bsn::SceneBsnAst>();
        let entity = world.spawn_empty().id();
        jackdaw_bsn::create_entity_in_ast(world, entity, None);
        entity
    }

    #[test]
    fn dead_child_refs_are_not_scene_children() {
        // The AST can still name a despawned ECS entity (duplicate + remap
        // leaves a stale `ecs_for_ast` id). Membership requires a live entity.
        let mut world = World::new();
        let host = document_entity(&mut world);
        let ghost = world.spawn(ChildOf(host)).id();
        jackdaw_bsn::create_entity_in_ast(&mut world, ghost, Some(host));
        assert!(scene_children(&world, host).contains(&ghost));
        world.despawn(ghost);
        assert!(!scene_children(&world, host).contains(&ghost));
    }

    #[test]
    fn non_document_children_are_not_scene_children() {
        // Bevy internals (shadow cascades, WorldAssetRoot instance nodes)
        // are not in the scene document. They must not appear as children
        // of an authored parent, named or not.
        let mut world = World::new();
        let host = document_entity(&mut world);
        let derived = world.spawn((Name::new("blaster-d"), ChildOf(host))).id();
        assert!(!scene_children(&world, host).contains(&derived));
    }

    #[test]
    fn gltf_source_lists_authored_children_and_loader_parts() {
        let mut world = World::new();
        let root = document_entity(&mut world);
        world
            .entity_mut(root)
            .insert(jackdaw_scene_types::GltfSource {
                path: "models/dungeon.glb".into(),
                scene_index: 0,
            });
        let part = world.spawn(ChildOf(root)).id();
        let authored = world.spawn(ChildOf(root)).id();
        jackdaw_bsn::create_entity_in_ast(&mut world, authored, Some(root));
        let kids = scene_children(&world, root);
        assert!(kids.contains(&authored));
        assert!(kids.contains(&part));
    }

    #[test]
    fn brush_icon_refreshes_when_brush_added_after_row() {
        // The duplicate path streams a brush's components into the world one at
        // a time through the scene, so its outliner row can be spawned (on
        // Transform) before `Brush` lands, leaving the fallback dot. Once
        // `Brush` is present, refresh_row_icon must swap the glyph to the
        // registered brush icon.
        use jackdaw_feathers::icons::Icon;

        let mut world = World::new();
        world.init_resource::<AppTypeRegistry>();
        {
            let registry = world.resource::<AppTypeRegistry>();
            registry.write().register::<Brush>();
        }

        let mut icons = EntityIconRegistry::default();
        icons.register(Brush::type_path(), Icon::Cuboid);
        world.insert_resource(icons);

        let source = world.spawn(Brush::default()).id();

        // Minimal row: TreeNode -> TreeRowContent -> TreeRowDot -> glyph Text.
        let glyph = world.spawn(Text::new("x")).id();
        let dot = world.spawn(TreeRowDot).id();
        world.entity_mut(glyph).insert(ChildOf(dot));
        let content = world.spawn(TreeRowContent).id();
        world.entity_mut(dot).insert(ChildOf(content));
        let row = world.spawn(TreeNode(source)).id();
        world.entity_mut(content).insert(ChildOf(row));

        let container = world.spawn_empty().id();
        let mut index = TreeIndex::default();
        index.insert(container, source, row);
        world.insert_resource(index);

        refresh_row_icon(&mut world, source);

        assert_eq!(
            world.get::<Text>(glyph).map(|t| t.0.clone()),
            Some(String::from(Icon::Cuboid.unicode()))
        );
    }

    #[test]
    fn reveal_path_walks_to_the_nearest_rowed_ancestor() {
        // root -> mid -> leaf via ChildOf. `reveal_path` returns the ancestor
        // chain from the highest ancestor down to leaf's direct parent, with
        // leaf itself excluded: [root, mid]. The driver decides which of these
        // already have rows and which still need expanding.
        let mut world = World::new();
        let root = world.spawn_empty().id();
        let mid = world.spawn(ChildOf(root)).id();
        let leaf = world.spawn(ChildOf(mid)).id();

        assert_eq!(reveal_path(&world, leaf), vec![root, mid]);
        // A root with no parent has an empty reveal path.
        assert!(reveal_path(&world, root).is_empty());
    }

    #[test]
    fn reveal_driver_expands_nearest_rowed_ancestor_and_counts_down() {
        // Only `root` has a row in TreeIndex; the driver should set root's row
        // to expanded and leave the countdown decremented.
        let mut world = World::new();
        world.init_resource::<TreeIndex>();

        let container = world.spawn_empty().id();
        let root = world.spawn_empty().id();
        let mid = world.spawn(ChildOf(root)).id();
        let leaf = world.spawn(ChildOf(mid)).id();

        let root_row = world.spawn(TreeNodeExpanded(false)).id();
        world
            .resource_mut::<TreeIndex>()
            .insert(container, root, root_row);

        world.insert_resource(RevealTarget {
            entity: Some(leaf),
            frames_left: 16,
        });

        run_reveal_driver_once(&mut world);

        assert!(
            world.get::<TreeNodeExpanded>(root_row).map(|e| e.0) == Some(true),
            "root's row should be expanded (nearest rowed ancestor)"
        );
        assert_eq!(
            world.resource::<RevealTarget>().frames_left,
            15,
            "countdown decrements each driven frame"
        );
        assert_eq!(
            world.resource::<RevealTarget>().entity,
            Some(leaf),
            "target stays set until its own row exists"
        );
    }

    #[test]
    fn reveal_driver_clears_when_target_has_a_row() {
        let mut world = World::new();
        world.init_resource::<TreeIndex>();
        let container = world.spawn_empty().id();
        let leaf = world.spawn_empty().id();
        let leaf_row = world.spawn(TreeNodeExpanded(false)).id();
        world
            .resource_mut::<TreeIndex>()
            .insert(container, leaf, leaf_row);
        world.insert_resource(RevealTarget {
            entity: Some(leaf),
            frames_left: 16,
        });

        run_reveal_driver_once(&mut world);

        assert!(
            world.resource::<RevealTarget>().entity.is_none(),
            "target clears once its own row exists"
        );
    }

    #[test]
    fn reveal_driver_clears_when_countdown_expires() {
        let mut world = World::new();
        world.init_resource::<TreeIndex>();
        let _container = world.spawn_empty().id();
        let leaf = world.spawn_empty().id();
        // No row anywhere for leaf and no rowed ancestor; the countdown drains.
        world.insert_resource(RevealTarget {
            entity: Some(leaf),
            frames_left: 1,
        });

        run_reveal_driver_once(&mut world);

        assert!(
            world.resource::<RevealTarget>().entity.is_none(),
            "target clears when the countdown hits zero with no progress"
        );
    }

    /// Run the reveal driver one tick against `world` via a cached system.
    fn run_reveal_driver_once(world: &mut World) {
        world
            .run_system_cached(drive_reveal_target)
            .expect("reveal driver runs");
    }

    /// `RenameBeginOp` dispatched with an explicit `entity` param
    /// (the path the context-menu "Rename" item and the
    /// `TreeRowStartRename` event use) returns that entity. The
    /// param wins over any selection state.
    #[test]
    fn resolve_rename_target_prefers_entity_param() {
        let target = Entity::from_raw_u32(7).unwrap();
        let other = Entity::from_raw_u32(42).unwrap();
        let params = params_with_entity("entity", target);
        let selection = Selection {
            entities: vec![other],
        };
        assert_eq!(resolve_rename_target(&params, &selection), Some(target));
    }

    /// F2 keybind regression cover: the bare keypress dispatches
    /// `RenameBeginOp` with no params, and the operator must read
    /// the primary selection. Before the fix, the op early-returned
    /// `Cancelled` whenever no `entity` param was supplied, so F2
    /// silently did nothing even with a selected outliner row.
    #[test]
    fn resolve_rename_target_falls_back_to_selection_primary() {
        let primary = Entity::from_raw_u32(11).unwrap();
        let params = empty_params();
        let selection = Selection {
            // The last entry is the primary selection.
            entities: vec![Entity::from_raw_u32(99).unwrap(), primary],
        };
        assert_eq!(resolve_rename_target(&params, &selection), Some(primary));
    }

    /// No param, no selection: the op cancels. Confirms the early
    /// bail still fires, so a stray F2 in an empty scene doesn't
    /// fall into find-rename-targets with a garbage entity.
    #[test]
    fn resolve_rename_target_returns_none_without_selection_or_param() {
        let params = empty_params();
        let selection = Selection::default();
        assert_eq!(resolve_rename_target(&params, &selection), None);
    }

    #[test]
    fn live_set_roots_are_live_entities_without_live_parents() {
        let mut world = World::new();
        let authored_parent = world.spawn_empty().id();
        let live_root = world.spawn((ChildOf(authored_parent), LivePreview)).id();
        let _live_child = world.spawn((ChildOf(live_root), LivePreview)).id();
        let _not_live = world.spawn_empty().id();
        let roots = live_tree_roots(&mut world);
        assert_eq!(
            roots,
            vec![live_root],
            "live child of a non-live parent is the root"
        );
    }
}
