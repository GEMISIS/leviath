//! Where headed tables land in the file.
//!
//! `toml_edit` writes headed tables in the order of their `position`, a
//! number each table got when the document was parsed; a table added since
//! has none and is written right after whichever positioned table the writer
//! visited last, ties broken by the order the tables sit in memory. A file
//! written by hand keeps each stage's `[[graph.edges]]` next to the stage,
//! so a stage or an edge the editor adds has to be placed there, not left
//! wherever the writer happens to be. This module reproduces the writer's
//! order and renumbers on demand.

use toml_edit::{DocumentMut, Item, Table};

/// One step down into the document: a key, or an element of an array of
/// tables (`[[graph.stages]]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Seg {
    Key(String),
    Index(usize),
}

/// The path of the `index`th element of the array of tables at
/// `graph.<list>`.
pub(super) fn element(list: &str, index: usize) -> Vec<Seg> {
    vec![
        Seg::Key("graph".to_string()),
        Seg::Key(list.to_string()),
        Seg::Index(index),
    ]
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

/// Where a moved block goes.
pub(super) enum Spot<'a> {
    /// Right after the last table under this path.
    After(&'a [Seg]),
    /// Right before the first table under this path, or at the end of the
    /// file when nothing is under it.
    Before(&'a [Seg]),
}

/// Move the tables under `block` (the table at that path and every table
/// under it) to `spot`, and renumber the file to match. Nothing moves when
/// `block` has no headed table, or `spot` names nothing to stand after.
pub(super) fn move_block(doc: &mut DocumentMut, block: &[Seg], spot: Spot<'_>) {
    let order = written_order(doc);
    let moving: Vec<Vec<Seg>> = order
        .iter()
        .filter(|p| p.starts_with(block))
        .cloned()
        .collect();
    if moving.is_empty() {
        return;
    }
    let mut rest: Vec<Vec<Seg>> = order
        .into_iter()
        .filter(|p| !p.starts_with(block))
        .collect();
    let at = match spot {
        Spot::After(anchor) => match rest.iter().rposition(|p| p.starts_with(anchor)) {
            Some(i) => i + 1,
            None => return,
        },
        Spot::Before(anchor) => rest
            .iter()
            .position(|p| p.starts_with(anchor))
            .unwrap_or(rest.len()),
    };
    rest.splice(at..at, moving);
    renumber(doc, &rest);
}
