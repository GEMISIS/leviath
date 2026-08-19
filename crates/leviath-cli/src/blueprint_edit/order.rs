//! Where tables land in the file.
//!
//! `toml_edit` writes headed tables in the order of their `position`, a
//! number each table got when the document was parsed; a table added since
//! has none and is written right after whichever positioned table the writer
//! visited last, ties broken by the order the tables sit in memory. The stage
//! views must list stages in the order the file will show them, and adding a
//! stage "after plan" or moving one up must land it there, so this module
//! reproduces the writer's order and renumbers on demand.

use toml_edit::{DocumentMut, Item, Table};

/// One step down into the document: a key, or an element of an array of
/// tables (`[[stages.plan.interaction_points]]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Seg {
    Key(String),
    Index(usize),
}

/// The path of every headed table, root excluded, in the order the writer
/// emits them.
pub(super) fn written_order(doc: &DocumentMut) -> Vec<Vec<Seg>> {
    let mut found: Vec<(isize, Vec<Seg>)> = Vec::new();
    let mut last = 0;
    let mut path = Vec::new();
    visit(doc.as_table(), &mut path, &mut last, &mut found);
    // The writer sorts by position and keeps arrival order for ties.
    found.sort_by_key(|(pos, _)| *pos);
    found.into_iter().map(|(_, p)| p).collect()
}

fn visit(table: &Table, path: &mut Vec<Seg>, last: &mut isize, out: &mut Vec<(isize, Vec<Seg>)>) {
    if !path.is_empty() && !table.is_dotted() {
        if let Some(pos) = table.position() {
            *last = pos;
        }
        out.push((*last, path.clone()));
    }
    for (key, item) in table.iter() {
        match item {
            Item::Table(t) => {
                path.push(Seg::Key(key.to_string()));
                visit(t, path, last, out);
                path.pop();
            }
            Item::ArrayOfTables(a) => {
                for (i, t) in a.iter().enumerate() {
                    path.push(Seg::Key(key.to_string()));
                    path.push(Seg::Index(i));
                    visit(t, path, last, out);
                    path.pop();
                    path.pop();
                }
            }
            _ => {}
        }
    }
}

/// The table at `path`, mutably.
fn table_at_mut<'a>(doc: &'a mut DocumentMut, path: &[Seg]) -> Option<&'a mut Table> {
    walk_item(doc.as_item_mut(), path)
}

fn walk_item<'a>(item: &'a mut Item, path: &[Seg]) -> Option<&'a mut Table> {
    match path.split_first() {
        None => item.as_table_mut(),
        // `Item::get_mut` would create the key; the trait's `get_mut` only
        // finds it.
        Some((Seg::Key(k), rest)) => walk_item(item.as_table_like_mut()?.get_mut(k)?, rest),
        Some((Seg::Index(n), rest)) => {
            let table = item.as_array_of_tables_mut()?.get_mut(*n)?;
            walk_table(table, rest)
        }
    }
}

fn walk_table<'a>(table: &'a mut Table, path: &[Seg]) -> Option<&'a mut Table> {
    match path.split_first() {
        None => Some(table),
        Some((Seg::Key(k), rest)) => walk_item(table.get_mut(k)?, rest),
        // An index only ever follows the key of an array of tables.
        Some((Seg::Index(_), _)) => None,
    }
}

/// Give every headed table a position matching `order` (0, 1, 2, ...), so
/// the file is written in exactly that order.
pub(super) fn renumber(doc: &mut DocumentMut, order: &[Vec<Seg>]) {
    for (i, path) in order.iter().enumerate() {
        if let Some(table) = table_at_mut(doc, path) {
            table.set_position(Some(i as isize));
        }
    }
}

/// The names of the `[stages.*]` tables in the order the file shows them.
/// A stage written inline (`stages = { plan = {...} }`, or `plan = {...}`
/// under `[stages]`) has no position and comes first, as the writer puts a
/// table's values before its subtables.
pub(super) fn stage_order(doc: &DocumentMut) -> Vec<String> {
    let Some(stages) = doc.get("stages") else {
        return Vec::new();
    };
    let Some(table) = stages.as_table_like() else {
        return Vec::new();
    };
    let mut inline: Vec<String> = Vec::new();
    let mut headed: Vec<String> = Vec::new();
    for (name, item) in table.iter() {
        if item.is_inline_table() {
            inline.push(name.to_string());
        } else if item.is_table() {
            headed.push(name.to_string());
        }
    }
    let written = written_order(doc);
    let rank = |name: &str| {
        written
            .iter()
            .position(|p| {
                p.first() == Some(&Seg::Key("stages".into()))
                    && p.get(1) == Some(&Seg::Key(name.into()))
            })
            .unwrap_or(usize::MAX)
    };
    headed.sort_by_key(|n| rank(n));
    inline.extend(headed);
    inline
}

/// The paths of the tables that make up stage `name`: its own and every
/// table under it, in written order.
pub(super) fn stage_block(order: &[Vec<Seg>], name: &str) -> Vec<Vec<Seg>> {
    order
        .iter()
        .filter(|p| {
            p.first() == Some(&Seg::Key("stages".into()))
                && p.get(1) == Some(&Seg::Key(name.into()))
        })
        .cloned()
        .collect()
}

/// Move stage `name`'s block of tables to just before (`before = true`) or
/// just after the block of `other`, and renumber the file to match.
pub(super) fn move_stage_block(doc: &mut DocumentMut, name: &str, other: &str, before: bool) {
    let order = written_order(doc);
    let block = stage_block(&order, name);
    let anchor = stage_block(&order, other);
    if name == other || block.is_empty() || anchor.is_empty() {
        return;
    }
    let mut rest: Vec<Vec<Seg>> = order
        .iter()
        .filter(|p| !block.contains(p))
        .cloned()
        .collect();
    // The anchor is in the order and not in the block, so it is in `rest`.
    let at = if before {
        rest.iter()
            .position(|p| p == &anchor[0])
            .expect("the anchor's first table is in the rest")
    } else {
        rest.iter()
            .position(|p| p == anchor.last().expect("non-empty"))
            .expect("the anchor's last table is in the rest")
            + 1
    };
    rest.splice(at..at, block);
    renumber(doc, &rest);
}
