//! The `#[Object]` impl shape: what is moved, what is left, what is refused.

use quote::quote;
use syn::{ItemImpl, parse_quote};

use super::expand;
use crate::attrs::{MirrorArgs, mirror_args};

/// What `#[mirror]` was given, with nothing said.
fn plain() -> MirrorArgs {
    mirror_args(quote!()).expect("no arguments")
}

/// The generated code for one impl block, as readable Rust.
fn written(item: ItemImpl) -> String {
    let tokens = expand(plain(), item).expect("a mirrored impl");
    let written = prettyplease::unparse(&syn::parse2(tokens).expect("the output is Rust"));
    println!("{written}");
    written
}

/// The reason one impl block was refused.
fn refused(item: ItemImpl) -> String {
    expand(plain(), item).expect_err("a refusal").to_string()
}

/// The resolver's body moves into an accessor, and the resolver calls it.
#[test]
fn a_resolver_body_is_moved_and_called() {
    let written = written(parse_quote! {
        #[Object]
        impl Region {
            /// The region's name.
            async fn name(&self) -> &str {
                &self.region().name
            }
        }
    });
    assert!(
        written.contains("async fn name(&self) -> &str {\n        self.mirror_name()\n    }"),
        "{written}"
    );
    assert!(
        written.contains(
            "pub(crate) fn mirror_name(&self) -> &str {\n        &self.region().name\n    }"
        ),
        "{written}"
    );
    assert!(
        written.contains("#[Object(name = \"RegionOutput\")]"),
        "{written}"
    );
    assert!(
        written.contains("<&'static str as"),
        "the mirror names the type with no borrow to make:\n{written}"
    );
}

/// `no_filter` names the type and leaves every body where it was.
#[test]
fn naming_only_writes_no_mirror() {
    let tokens = expand(
        mirror_args(quote!(no_filter)).expect("one argument"),
        parse_quote! {
            #[Object]
            impl Region {
                const LIMIT: usize = 4;

                /// The region's name.
                #[filter(orderable)]
                async fn name(&self) -> &str {
                    &self.region().name
                }
            }
        },
    )
    .expect("a named impl");
    let written = prettyplease::unparse(&syn::parse2(tokens).expect("the output is Rust"));
    assert!(
        written.contains("#[Object(name = \"RegionOutput\")]"),
        "{written}"
    );
    assert!(
        written.contains("&self.region().name"),
        "the body stays put:\n{written}"
    );
    assert!(!written.contains("RegionFilter"), "{written}");
    assert!(!written.contains("mirror_name"), "{written}");
    assert!(
        !written.contains("#[filter"),
        "the helper attribute comes off:\n{written}"
    );
}

/// A mark that cannot mean anything is read even where nothing is mirrored.
#[test]
fn naming_only_still_reads_the_marks() {
    let problem = expand(
        mirror_args(quote!(no_filter)).expect("one argument"),
        parse_quote! {
            #[Object]
            impl Region {
                /// The region's name.
                #[filter(io, orderable)]
                async fn name(&self) -> &str {
                    &self.name
                }
            }
        },
    )
    .expect_err("a contradiction");
    assert!(
        problem.to_string().contains("cannot sit on an `io` field"),
        "{problem}"
    );
}

/// A field that costs a read keeps an asynchronous accessor.
#[test]
fn a_read_costing_field_keeps_its_await() {
    let written = written(parse_quote! {
        #[Object]
        impl Run {
            /// The context.
            #[filter(io)]
            async fn context(&self) -> Option<Window> {
                read(self).await
            }
        }
    });
    assert!(
        written.contains("pub(crate) async fn mirror_context(&self)"),
        "{written}"
    );
    assert!(written.contains("self.mirror_context().await"), "{written}");
    assert!(written.contains("acc.pending(&self.context);"), "{written}");
    assert!(
        written.contains("confirm.io(self.context.as_deref(), target.mirror_context())"),
        "{written}"
    );
}

/// A resolver left out of the mirror keeps its own body.
#[test]
fn a_skipped_resolver_is_left_alone() {
    let written = written(parse_quote! {
        #[Object]
        impl Run {
            /// The logs.
            #[filter(skip)]
            async fn logs(&self, stage: i32) -> String {
                read(stage)
            }
        }
    });
    assert!(
        written.contains("async fn logs(&self, stage: i32)"),
        "{written}"
    );
    assert!(!written.contains("mirror_logs"), "{written}");
    assert!(!written.contains("logs: Option<"), "{written}");
}

/// A resolver read through an accessor of the caller's keeps its own body too.
#[test]
fn a_resolver_with_its_own_accessor_is_left_alone() {
    let written = written(parse_quote! {
        #[Object]
        impl Run {
            /// The blueprint.
            #[filter(with = "run_relations::blueprint_of")]
            async fn blueprint(&self, ctx: &Context<'_>) -> Option<Blueprint> {
                read(ctx).await
            }
        }
    });
    assert!(!written.contains("mirror_blueprint"), "{written}");
    assert!(
        written.contains("run_relations::blueprint_of(target, cx)"),
        "{written}"
    );
}

/// A resolver that pages its answer is mirrored on what it pages, read
/// through the accessor that produces it.
#[test]
fn a_reshaped_resolver_is_mirrored_on_what_it_answers_about() {
    let written = written(parse_quote! {
        #[Object]
        impl Run {
            /// The stages.
            #[filter(io, with = "run_relations::stages_of", ty = "Vec<StageRecord>")]
            async fn stages(&self, first: i32) -> Connection<StageRecord> {
                page(first).await
            }
        }
    });
    // Line breaks are the pretty-printer's business, so the shapes are read
    // off the code with its whitespace taken out.
    let flat: String = written
        .chars()
        .filter(|each| !each.is_whitespace())
        .collect();
    assert!(
        flat.contains("stages:Option<Box<<Vec<StageRecord,>as"),
        "the mirror reads the list rather than the page: {written}"
    );
    assert!(
        flat.contains("confirm.io(self.stages.as_deref(),run_relations::stages_of(target,cx))"),
        "the read happens in the second phase, through the accessor: {written}"
    );
    assert!(!written.contains("mirror_stages"), "{written}");
}

/// A resolver taking arguments has no honest mirror, and says so.
#[test]
fn a_resolver_with_arguments_is_refused() {
    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// The logs.
            async fn logs(&self, stage: i32) -> String {
                read(stage)
            }
        }
    });
    assert!(said.contains("takes `&self` and nothing else"), "{said}");
    assert!(said.contains("#[filter(skip)]"), "{said}");
}

/// A resolver that awaits without saying so is refused where it awaits.
#[test]
fn an_unmarked_await_is_refused() {
    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// The context.
            async fn context(&self) -> Option<Window> {
                read(self).await
            }
        }
    });
    assert!(said.contains("awaits something"), "{said}");
    assert!(said.contains("#[filter(io)]"), "{said}");
}

/// A resolver that reads nothing is not an `io` field.
#[test]
fn a_mark_that_says_nothing_is_refused() {
    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// The name.
            #[filter(io)]
            fn name(&self) -> &str {
                &self.name
            }
        }
    });
    assert!(said.contains("not even asynchronous"), "{said}");
}

/// A resolver answering with nothing has nothing to compare.
#[test]
fn a_resolver_with_no_answer_is_refused() {
    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// Nothing.
            async fn nothing(&self) {}
        }
    });
    assert!(said.contains("answers with a value"), "{said}");
}

/// The impl has to be the one `#[Object]` reads, and of a named type.
#[test]
fn the_impl_has_to_be_a_graphql_type() {
    let said = refused(parse_quote! {
        impl Run {
            /// The name.
            async fn name(&self) -> &str {
                &self.name
            }
        }
    });
    assert!(said.contains("directly above `#[Object]`"), "{said}");

    let said = refused(parse_quote! {
        #[Object]
        impl Wrapper<Run> {
            /// The name.
            async fn name(&self) -> &str {
                &self.name
            }
        }
    });
    assert!(said.contains("plain named type"), "{said}");
}

/// A name the source already gave is kept rather than replaced.
#[test]
fn an_explicit_name_is_kept() {
    let written = written(parse_quote! {
        #[Object(name = "RunOutput", cache_control(max_age = 60))]
        impl Run {
            /// The name.
            async fn name(&self) -> &str {
                &self.name
            }
        }
    });
    assert!(written.contains("cache_control(max_age = 60)"), "{written}");
    assert!(
        !written.contains("name = \"RunOutput\", name ="),
        "the name is set once:\n{written}"
    );
}

/// A renamed resolver is mirrored under the name the schema shows.
#[test]
fn a_renamed_resolver_is_mirrored_under_its_schema_name() {
    let written = written(parse_quote! {
        #[Object]
        impl Run {
            /// The name.
            #[graphql(name = "label")]
            async fn name(&self) -> &str {
                &self.name
            }
        }
    });
    assert!(
        written.contains("#[graphql(name = \"label\")]"),
        "{written}"
    );
    assert!(
        written.contains("Filter on `RunOutput.label`."),
        "{written}"
    );
}

/// Anything in the impl that is not a resolver is left where it is.
#[test]
fn a_non_resolver_item_is_left_where_it_is() {
    let written = written(parse_quote! {
        #[Object]
        impl Run {
            const LIMIT: usize = 4;

            /// Kept out of the schema.
            #[graphql(skip)]
            async fn hidden(&self) -> &str {
                "hidden"
            }
        }
    });
    assert!(written.contains("const LIMIT: usize = 4;"), "{written}");
    assert!(!written.contains("mirror_hidden"), "{written}");
}

/// Every attribute an impl block carries is read, and a broken one is refused.
#[test]
fn a_broken_attribute_anywhere_in_the_impl_is_refused() {
    let said = refused(parse_quote! {
        #[Object(name = 4)]
        impl Run {
            /// The name.
            async fn name(&self) -> &str { &self.name }
        }
    });
    assert!(said.contains("string in quotes"), "{said}");

    let said = refused(parse_quote! {
        #[Object]
        impl RunConnection {
            /// The name.
            async fn name(&self) -> &str { &self.name }
        }
    });
    assert!(said.contains("not a type a filter mirrors"), "{said}");

    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// The name.
            #[filter(sorted)]
            async fn name(&self) -> &str { &self.name }
        }
    });
    assert!(said.contains("unknown `#[filter]`"), "{said}");

    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// The name.
            #[graphql = "label"]
            async fn name(&self) -> &str { &self.name }
        }
    });
    assert!(said.contains("arguments in brackets"), "{said}");

    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// The name.
            #[graphql(name = 4)]
            async fn name(&self) -> &str { &self.name }
        }
    });
    assert!(said.contains("string in quotes"), "{said}");
}

/// A resolver with no receiver at all is refused with the same message.
#[test]
fn an_associated_function_is_refused() {
    let said = refused(parse_quote! {
        #[Object]
        impl Run {
            /// A name from nowhere.
            async fn name() -> &'static str { "run" }
        }
    });
    assert!(said.contains("takes `&self` and nothing else"), "{said}");
}
