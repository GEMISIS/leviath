//! The response's fields come back in the order the query asked for them.
//!
//! The spec says so (section 6.3: the fields of a selection set are answered
//! in the order they were selected), and a client that reads a response as a
//! stream, or diffs two of them, is right to expect it. The executor resolves
//! an object's fields at the same time and writes each answer down as it
//! lands, so a slow field comes after a fast one whatever order they were
//! asked in. Resolving them one after another would keep the order and lose
//! the concurrency, which is the wrong trade for a listing whose fields each
//! read a file.
//!
//! So the order is put back afterwards. The parsed document is kept from the
//! moment the parser hands it over, and once the whole response is in hand
//! every object in it is rewritten to follow the selection set that produced
//! it, fragments included. Nothing is resolved twice and nothing waits on
//! anything else; the walk touches keys, not values.

use std::sync::{Arc, Mutex};

use async_graphql::extensions::{
    Extension, ExtensionContext, ExtensionFactory, NextExecute, NextParseQuery,
};
use async_graphql::indexmap::IndexMap;
use async_graphql::parser::types::{
    DocumentOperations, ExecutableDocument, FragmentDefinition, Selection, SelectionSet,
};
use async_graphql::{Name, Positioned, Response, ServerResult, Value, Variables};

/// Registers [`SelectionOrder`] on the schema.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnswerInSelectionOrder;

impl ExtensionFactory for AnswerInSelectionOrder {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(SelectionOrder::default())
    }
}

/// Puts a response's fields back in the order the query selected them.
///
/// One of these is made per request, so the document it holds is the one
/// the response answers.
#[derive(Debug, Default)]
struct SelectionOrder {
    /// The document the parser produced, kept until the response is built.
    document: Mutex<Option<ExecutableDocument>>,
}

#[async_trait::async_trait]
impl Extension for SelectionOrder {
    async fn parse_query(
        &self,
        ctx: &ExtensionContext<'_>,
        query: &str,
        variables: &Variables,
        next: NextParseQuery<'_>,
    ) -> ServerResult<ExecutableDocument> {
        let document = next.run(ctx, query, variables).await?;
        *leviath_core::sync::lock(&self.document) = Some(document.clone());
        Ok(document)
    }

    async fn execute(
        &self,
        ctx: &ExtensionContext<'_>,
        operation_name: Option<&str>,
        next: NextExecute<'_>,
    ) -> Response {
        let mut response = next.run(ctx, operation_name).await;
        let document = leviath_core::sync::lock(&self.document).take();
        if let Some(document) = document
            && let Some(operation) = operation_of(&document, operation_name)
        {
            let fragments = &document.fragments;
            reorder(
                &mut response.data,
                &operation.node.selection_set.node,
                fragments,
            );
        }
        response
    }
}

/// The operation this request ran, by name where the document holds several.
fn operation_of<'a>(
    document: &'a ExecutableDocument,
    name: Option<&str>,
) -> Option<&'a Positioned<async_graphql::parser::types::OperationDefinition>> {
    match &document.operations {
        DocumentOperations::Single(operation) => Some(operation),
        // With several operations and no name the request was refused before
        // it ran, and `and_then` over the absent name answers that with
        // nothing to reorder.
        DocumentOperations::Multiple(operations) => name.and_then(|name| operations.get(name)),
    }
}

/// The fragments a document defines, by name.
type Fragments = std::collections::HashMap<Name, Positioned<FragmentDefinition>>;

/// Rewrite `value` so each object's keys follow `selection`, recursively.
///
/// A key the selection does not name (which cannot happen for a validated
/// query, but costs nothing to allow) keeps its place after the named ones.
/// A list is walked item by item against the same selection, because every
/// item of a list answers the same selection set.
fn reorder(value: &mut Value, selection: &SelectionSet, fragments: &Fragments) {
    match value {
        Value::Object(fields) => {
            let mut ordered: IndexMap<Name, Value> = IndexMap::with_capacity(fields.len());
            let mut keys: Vec<(Name, &SelectionSet)> = Vec::with_capacity(fields.len());
            response_keys(selection, fragments, &mut keys, 0);
            for (key, nested) in keys {
                if let Some(mut inner) = fields.swap_remove(&key) {
                    reorder(&mut inner, nested, fragments);
                    ordered.insert(key, inner);
                }
            }
            ordered.extend(std::mem::take(fields));
            *fields = ordered;
        }
        Value::List(items) => {
            for item in items {
                reorder(item, selection, fragments);
            }
        }
        _ => {}
    }
}

/// How deep a fragment may spread into another before the walk stops.
///
/// A validated document has no cycle, so this is never reached; it is what
/// keeps a walk finite if one ever were.
const MAX_SPREAD_DEPTH: usize = 32;

/// Every response key `selection` produces, in order, each with the selection
/// set that answers it. Fragments are flattened in place, the way the
/// executor flattens them.
fn response_keys<'a>(
    selection: &'a SelectionSet,
    fragments: &'a Fragments,
    out: &mut Vec<(Name, &'a SelectionSet)>,
    depth: usize,
) {
    if depth > MAX_SPREAD_DEPTH {
        return;
    }
    for item in &selection.items {
        match &item.node {
            Selection::Field(field) => {
                let key = field.node.response_key().node.clone();
                // Two selections of one key merge into one answer; the first
                // mention decides where it sits.
                if !out.iter().any(|(seen, _)| *seen == key) {
                    out.push((key, &field.node.selection_set.node));
                }
            }
            Selection::FragmentSpread(spread) => {
                // A spread naming no fragment is refused by the validator, so
                // the lookup is written as one that has nothing to say for it.
                for fragment in fragments.get(&spread.node.fragment_name.node).into_iter() {
                    response_keys(&fragment.node.selection_set.node, fragments, out, depth + 1);
                }
            }
            Selection::InlineFragment(inline) => {
                response_keys(&inline.node.selection_set.node, fragments, out, depth + 1);
            }
        }
    }
}

#[cfg(test)]
#[path = "ordering_tests.rs"]
mod tests;
