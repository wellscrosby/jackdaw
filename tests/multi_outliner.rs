//! Multi-instance Outliner: two `HierarchyTreeContainer`s should both
//! reflect every scene-graph change in lockstep.
//!
//! Pins the per-`(container, source)` `TreeIndex` contracts:
//!  - adding a new root scene entity spawns one row in every container,
//!    not zero (single-instance fallthrough) and not two in any one
//!    panel (the duplicate-row regression);
//!  - reparenting a scene entity moves its row under the new parent's
//!    `TreeRowChildren` in every container;
//!  - despawning the source removes the row in every container;
//!  - Live rows follow the projection map, including rerooting a live
//!    child when its parent leaves the live set.

use bevy::prelude::*;
use jackdaw::hierarchy::HierarchyTreeContainer;
use jackdaw_ui::{UiButton, UiCanvas, UiGeneratedPart};
use jackdaw_widgets::tree_view::{TreeIndex, TreeNode};

mod util;

/// Spawn a host entity carrying `HierarchyTreeContainer` (which
/// requires `TreeRoot` + `EditorEntity`). Matches the runtime
/// layout's "Outliner panel content" entity.
fn spawn_outliner_container(world: &mut World) -> Entity {
    world
        .spawn((
            HierarchyTreeContainer,
            Node::default(),
            Visibility::Inherited,
        ))
        .id()
}

fn spawn_named_document_root(world: &mut World, name: &str) -> Entity {
    let entity = world
        .spawn((Name::new(name.to_string()), Transform::default()))
        .id();
    jackdaw::scene_io::register_entity_in_ast(world, entity);
    entity
}

fn map_live(world: &mut World, bits: u64, preview: Entity) {
    world
        .resource_mut::<jackdaw::pie_projection::PieProjection>()
        .by_bits
        .insert(bits, preview);
}

#[test]
fn add_root_entity_spawns_one_row_per_container() {
    let mut app = util::editor_test_app();
    let world = app.world_mut();

    let outliner_a = spawn_outliner_container(world);
    let outliner_b = spawn_outliner_container(world);

    let entity = spawn_named_document_root(world, "Brush");

    // Flush the queued `commands` from the `On<Add, ...>` observers.
    app.update();
    let world = app.world_mut();

    let index = world.resource::<TreeIndex>();
    assert!(
        index.contains(outliner_a, entity),
        "outliner A should have a row for the new root",
    );
    assert!(
        index.contains(outliner_b, entity),
        "outliner B should have a row for the new root",
    );

    // Exactly one row per container, never two.
    let mut q = world.query::<(Entity, &TreeNode)>();
    let rows: Vec<(Entity, Entity)> = q
        .iter(world)
        .filter(|(_, tree_node)| tree_node.0 == entity)
        .map(|(e, t)| (e, t.0))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "expected exactly one row per outliner container (2 total), got {}",
        rows.len(),
    );
}

#[test]
fn reparent_scene_entity_moves_row_in_every_outliner() {
    let mut app = util::editor_test_app();
    let world = app.world_mut();

    let outliner_a = spawn_outliner_container(world);
    let outliner_b = spawn_outliner_container(world);

    let parent = spawn_named_document_root(world, "Parent");
    let child = spawn_named_document_root(world, "Child");
    app.update();

    // Sanity: both containers initially see both as roots.
    let world = app.world_mut();
    {
        let index = world.resource::<TreeIndex>();
        for c in [outliner_a, outliner_b] {
            assert!(index.contains(c, parent), "{c} should host parent row");
            assert!(index.contains(c, child), "{c} should host child row");
        }
    }

    // Mark the parent as populated so reconcile reseats the child
    // under it instead of treating it as a collapsed subtree.
    // (`spawn_single_tree_row` defaults `TreeChildrenPopulated(false)`.)
    {
        let mut q = world.query::<(
            &TreeNode,
            &mut jackdaw_widgets::tree_view::TreeChildrenPopulated,
        )>();
        for (tree_node, mut populated) in q.iter_mut(world) {
            if tree_node.0 == parent {
                populated.0 = true;
            }
        }
    }

    {
        let world = app.world_mut();
        jackdaw::commands::set_hierarchy_location(
            world,
            child,
            jackdaw::commands::HierarchyLocation {
                parent: Some(parent),
                index: usize::MAX,
                slot: None,
            },
        );
    }
    app.update();

    let world = app.world_mut();
    let index = world.resource::<TreeIndex>();

    // Parent's row in each container has a `TreeRowChildren` descendant
    // that should be the new ancestor of the child's row.
    for container in [outliner_a, outliner_b] {
        let parent_row = index
            .get(container, parent)
            .expect("parent row in container");
        let child_row = index.get(container, child).expect("child row in container");

        // Walk up from child_row's ChildOf chain; we must hit parent_row.
        let mut current = child_row;
        let mut found_parent = false;
        for _ in 0..6 {
            let Some(co) = world.get::<ChildOf>(current) else {
                break;
            };
            if co.parent() == parent_row {
                found_parent = true;
                break;
            }
            current = co.parent();
        }
        assert!(
            found_parent,
            "child row in {container} should reparent under {parent_row} after the source was reparented",
        );
    }
}

#[test]
fn despawn_scene_entity_drops_row_in_every_outliner() {
    let mut app = util::editor_test_app();
    let world = app.world_mut();

    let outliner_a = spawn_outliner_container(world);
    let outliner_b = spawn_outliner_container(world);

    let entity = spawn_named_document_root(world, "Brush");
    app.update();

    let world = app.world_mut();
    {
        let index = world.resource::<TreeIndex>();
        assert!(index.contains(outliner_a, entity));
        assert!(index.contains(outliner_b, entity));
    }

    world.entity_mut(entity).despawn();
    app.update();

    let world = app.world_mut();
    let index = world.resource::<TreeIndex>();
    assert!(
        !index.contains(outliner_a, entity),
        "row should be cleaned out of outliner A",
    );
    assert!(
        !index.contains(outliner_b, entity),
        "row should be cleaned out of outliner B",
    );
}

#[test]
fn ui_canvas_and_authored_children_appear_but_generated_parts_do_not() {
    let mut app = util::editor_test_app();
    let outliner_a = spawn_outliner_container(app.world_mut());
    let outliner_b = spawn_outliner_container(app.world_mut());

    let canvas = app
        .world_mut()
        .spawn((Name::new("UI Canvas"), UiCanvas::default()))
        .id();
    jackdaw::scene_io::register_entity_in_ast(app.world_mut(), canvas);
    app.update();

    {
        let world = app.world_mut();
        let mut rows = world.query::<(
            &TreeNode,
            &mut jackdaw_widgets::tree_view::TreeChildrenPopulated,
        )>();
        for (row, mut populated) in rows.iter_mut(world) {
            if row.0 == canvas {
                populated.0 = true;
            }
        }
    }

    let button = app
        .world_mut()
        .spawn((Name::new("Button"), UiButton::default(), ChildOf(canvas)))
        .id();
    jackdaw::scene_io::register_entity_in_ast(app.world_mut(), button);
    app.update();
    app.update();

    let world = app.world_mut();
    let index = world.resource::<TreeIndex>();
    for container in [outliner_a, outliner_b] {
        assert!(index.contains(container, canvas));
        assert!(index.contains(container, button));
    }
    assert!(
        world
            .get::<Children>(button)
            .is_none_or(bevy::prelude::RelationshipTarget::is_empty),
        "the editor authors UI as inert data: nothing materializes onto the authored button"
    );

    // A view-local copy opts into materialization. Its generated label is
    // implementation-owned and must stay out of every Outliner.
    let materialized = world
        .spawn((
            Name::new("Projected Button"),
            UiButton::default(),
            jackdaw_ui::UiMaterialize,
        ))
        .id();
    app.update();
    app.update();

    let world = app.world_mut();
    let generated = world
        .get::<Children>(materialized)
        .expect("a marked button materializes its label")
        .iter()
        .find(|child| world.get::<UiGeneratedPart>(*child).is_some())
        .expect("button label should be materialized");
    assert!(
        !world.resource::<TreeIndex>().contains_anywhere(generated),
        "implementation-owned widget parts must not leak into the authored outliner"
    );
}

#[test]
fn stripping_live_preview_from_parent_reroots_live_children() {
    let mut app = util::editor_test_app();
    app.world_mut()
        .insert_resource(jackdaw::pie_mirror::PieViewMode::Live);
    let outliner = spawn_outliner_container(app.world_mut());

    let parent = app.world_mut().spawn_empty().id();
    map_live(app.world_mut(), 1, parent);
    app.update();

    {
        let world = app.world_mut();
        let mut rows = world.query::<(
            &TreeNode,
            &mut jackdaw_widgets::tree_view::TreeChildrenPopulated,
        )>();
        for (row, mut populated) in rows.iter_mut(world) {
            if row.0 == parent {
                populated.0 = true;
            }
        }
    }

    let child = app.world_mut().spawn(ChildOf(parent)).id();
    map_live(app.world_mut(), 2, child);
    app.update();

    {
        let index = app.world().resource::<TreeIndex>();
        assert!(index.contains(outliner, parent));
        assert!(
            index.contains(outliner, child),
            "an expanded live parent lists its live child"
        );
    }

    app.world_mut()
        .resource_mut::<jackdaw::pie_projection::PieProjection>()
        .by_bits
        .remove(&1);
    app.update();

    let index = app.world().resource::<TreeIndex>();
    assert!(
        !index.contains_anywhere(parent),
        "parent leaves the Live tree with its marker"
    );
    assert!(
        index.contains(outliner, child),
        "live child of a now-non-live parent becomes a Live root"
    );
}
