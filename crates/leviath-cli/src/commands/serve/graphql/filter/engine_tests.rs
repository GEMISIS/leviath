//! Every decision the filter system makes, against hand-written impls.
//!
//! The mirrors the macro writes are tested in `mirror_e2e_tests.rs`. Here the
//! impls are written out by hand, so what is being measured is the engine and
//! not the macro: the same rules hold for anything that implements the
//! contract, however it came to.

use std::sync::atomic::{AtomicUsize, Ordering as Counting};

use async_graphql::{Enum, InputObject};

use super::super::scalars::StringFilter;
use super::super::{Choices, ListItem, Nullable, Parts, Quantifiers};
use super::{
    Acc, BoxFuture, Confirm, EnumParts, Filterable, MatchCx, Mirror, Quantified, Tri, enum_test,
    object_confirm, object_test, quantified, quantified_confirm, settled,
};

/// How many times a second-phase field has been read.
static READS: AtomicUsize = AtomicUsize::new(0);

/// A value with one cheap field and one that costs a read.
#[derive(Debug, Clone)]
struct Thing {
    /// The cheap field.
    name: String,
    /// The field that costs a read.
    note: Option<String>,
}

impl Thing {
    /// Read the note, counting the read.
    async fn read_note(&self) -> Option<String> {
        READS.fetch_add(1, Counting::SeqCst);
        std::future::ready(()).await;
        self.note.clone()
    }
}

/// A hand-written mirror of [`Thing`], shaped as the macro would write it.
#[derive(Debug, Default, InputObject)]
#[graphql(name = "EngineThingInput")]
struct ThingFilter {
    /// Filter on the cheap field.
    name: Option<Box<StringFilter>>,
    /// Filter on the field that costs a read.
    note: Option<Box<StringFilter>>,
    /// Every filter in this list has to hold.
    and: Option<Vec<ThingFilter>>,
    /// At least one filter in this list has to hold.
    or: Option<Vec<ThingFilter>>,
    /// This filter must not hold.
    not: Option<Box<ThingFilter>>,
    /// Match where the value itself is absent.
    is_null: Option<bool>,
}

impl Nullable for ThingFilter {
    fn is_null(&self) -> Option<bool> {
        self.is_null
    }
}

impl Mirror for ThingFilter {
    type Target = Thing;

    fn parts(&self) -> Parts<'_, Self> {
        Parts::new(
            self.and.as_deref(),
            self.or.as_deref(),
            self.not.as_deref(),
            self.is_null,
        )
    }

    fn cheap(&self, target: &Self::Target, cx: &MatchCx<'_>, acc: &mut Acc) {
        acc.field(self.name.as_deref(), &target.name, cx);
        acc.pending(&self.note);
    }

    fn io<'a>(&'a self, target: &'a Self::Target, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut confirm = Confirm::new(cx);
            confirm.field(self.name.as_deref(), &target.name).await;
            confirm.io(self.note.as_deref(), target.read_note()).await;
            confirm.finish()
        })
    }
}

impl Filterable for Thing {
    type Filter = ThingFilter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        object_test(self, filter, cx)
    }

    fn confirm<'a>(&'a self, filter: &'a Self::Filter, cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        object_confirm(self, filter, cx)
    }
}

/// A hand-written quantifier input over [`Thing`].
#[derive(Debug, Default, InputObject)]
#[graphql(name = "EngineThingListInput")]
struct ThingListFilter {
    /// At least one item matches.
    some: Option<Box<ThingFilter>>,
    /// Every item matches.
    every: Option<Box<ThingFilter>>,
    /// No item matches.
    none: Option<Box<ThingFilter>>,
    /// Match where there is no list at all.
    is_null: Option<bool>,
}

impl Nullable for ThingListFilter {
    fn is_null(&self) -> Option<bool> {
        self.is_null
    }
}

impl Quantified for ThingListFilter {
    type Item = Thing;

    fn quantifiers(&self) -> Quantifiers<'_, ThingFilter> {
        Quantifiers::new(
            self.some.as_deref(),
            self.every.as_deref(),
            self.none.as_deref(),
        )
    }
}

impl ListItem for Thing {
    type ListFilter = ThingListFilter;

    fn list_test(items: &[Self], filter: &Self::ListFilter, cx: &MatchCx<'_>) -> Tri {
        quantified(items, filter, cx)
    }

    fn list_confirm<'a>(
        items: &'a [Self],
        filter: &'a Self::ListFilter,
        cx: &'a MatchCx<'a>,
    ) -> BoxFuture<'a, bool> {
        quantified_confirm(items, filter, cx)
    }
}

/// A vocabulary to compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
enum Shade {
    /// Light.
    Light,
    /// Dark.
    Dark,
}

/// A hand-written comparator for [`Shade`].
#[derive(Debug, Default, InputObject)]
#[graphql(name = "EngineShadeFilter")]
struct ShadeFilter {
    /// Exactly this value.
    eq: Option<Shade>,
    /// Anything but this value.
    ne: Option<Shade>,
    /// One of these values.
    #[graphql(name = "in")]
    within: Option<Vec<Shade>>,
    /// None of these values.
    not_in: Option<Vec<Shade>>,
    /// Match where there is no value at all.
    is_null: Option<bool>,
}

impl Nullable for ShadeFilter {
    fn is_null(&self) -> Option<bool> {
        self.is_null
    }
}

impl EnumParts for ShadeFilter {
    type Value = Shade;

    fn choices(&self) -> Choices<'_, Shade> {
        Choices::new(
            self.eq,
            self.ne,
            self.within.as_deref(),
            self.not_in.as_deref(),
            self.is_null,
        )
    }
}

/// A filter on nothing but the cheap field.
fn named(name: &str) -> ThingFilter {
    ThingFilter {
        name: Some(Box::new(StringFilter {
            eq: Some(name.to_owned()),
            ..StringFilter::default()
        })),
        ..ThingFilter::default()
    }
}

/// A filter on nothing but the field that costs a read.
fn noted(note: &str) -> ThingFilter {
    ThingFilter {
        note: Some(Box::new(StringFilter {
            eq: Some(note.to_owned()),
            ..StringFilter::default()
        })),
        ..ThingFilter::default()
    }
}

/// One value to test against.
fn thing() -> Thing {
    Thing {
        name: "plan".to_owned(),
        note: Some("written".to_owned()),
    }
}

/// A settled cheap answer is the true answer, whichever way it went.
#[tokio::test]
async fn a_settled_cheap_answer_is_the_true_answer() {
    let cx = MatchCx::at(0);
    let thing = thing();
    assert_eq!(thing.test(&named("plan"), &cx), Tri::Yes);
    assert!(thing.confirm(&named("plan"), &cx).await);
    assert_eq!(thing.test(&named("other"), &cx), Tri::No);
    assert!(!thing.confirm(&named("other"), &cx).await);
}

/// That holds underneath `not` and `or` as much as at the top.
#[tokio::test]
async fn the_two_phases_agree_under_the_combinators() {
    let cx = MatchCx::at(0);
    let thing = thing();
    let cases = [
        ThingFilter {
            not: Some(Box::new(named("other"))),
            ..ThingFilter::default()
        },
        ThingFilter {
            not: Some(Box::new(named("plan"))),
            ..ThingFilter::default()
        },
        ThingFilter {
            or: Some(vec![named("other"), named("plan")]),
            ..ThingFilter::default()
        },
        ThingFilter {
            or: Some(vec![named("other")]),
            ..ThingFilter::default()
        },
        ThingFilter {
            and: Some(vec![named("plan")]),
            ..ThingFilter::default()
        },
        ThingFilter {
            and: Some(vec![named("plan"), named("other")]),
            ..ThingFilter::default()
        },
    ];
    for filter in &cases {
        let cheap = thing.test(filter, &cx);
        let full = thing.confirm(filter, &cx).await;
        assert_ne!(cheap, Tri::Io, "nothing here costs a read");
        assert_eq!(
            cheap == Tri::Yes,
            full,
            "the two phases agree on {filter:?}"
        );
    }
}

/// An undecided answer under `not` stays undecided, and then flips.
#[tokio::test]
async fn an_undecided_answer_under_not_stays_undecided() {
    let cx = MatchCx::at(0);
    let thing = thing();
    let filter = ThingFilter {
        not: Some(Box::new(noted("written"))),
        ..ThingFilter::default()
    };
    assert_eq!(thing.test(&filter, &cx), Tri::Io);
    assert!(!thing.confirm(&filter, &cx).await);

    let filter = ThingFilter {
        or: Some(vec![noted("something else")]),
        ..ThingFilter::default()
    };
    assert_eq!(thing.test(&filter, &cx), Tri::Io);
    assert!(!thing.confirm(&filter, &cx).await);
}

/// `isNull: true` on a value that is there refuses it in both phases.
#[tokio::test]
async fn a_value_that_is_there_is_never_null() {
    let cx = MatchCx::at(0);
    let filter = ThingFilter {
        is_null: Some(true),
        ..ThingFilter::default()
    };
    assert_eq!(thing().test(&filter, &cx), Tri::No);
    assert!(!thing().confirm(&filter, &cx).await);
}

/// A field already refused is never read.
#[tokio::test]
async fn a_refused_field_is_never_read() {
    let cx = MatchCx::at(0);
    let filter = ThingFilter {
        note: noted("written").note,
        ..named("other")
    };
    let before = READS.load(Counting::SeqCst);
    assert_eq!(thing().test(&filter, &cx), Tri::No);
    assert!(!thing().confirm(&filter, &cx).await);
    assert_eq!(READS.load(Counting::SeqCst), before);

    let before = READS.load(Counting::SeqCst);
    assert!(thing().confirm(&noted("written"), &cx).await);
    assert_eq!(READS.load(Counting::SeqCst), before + 1);
}

/// A quantifier says what it says, in both phases.
#[tokio::test]
async fn the_quantifiers_say_what_they_say() {
    let cx = MatchCx::at(0);
    let items = [
        thing(),
        Thing {
            name: "write".to_owned(),
            note: None,
        },
    ];
    let over = |filter: ThingListFilter| Thing::list_test(&items, &filter, &cx);

    assert_eq!(over(ThingListFilter::default()), Tri::Yes);
    assert_eq!(
        over(ThingListFilter {
            some: Some(Box::new(named("plan"))),
            ..ThingListFilter::default()
        }),
        Tri::Yes
    );
    assert_eq!(
        over(ThingListFilter {
            some: Some(Box::new(named("nothing"))),
            ..ThingListFilter::default()
        }),
        Tri::No
    );
    assert_eq!(
        over(ThingListFilter {
            every: Some(Box::new(named("plan"))),
            ..ThingListFilter::default()
        }),
        Tri::No
    );
    assert_eq!(
        over(ThingListFilter {
            none: Some(Box::new(named("nothing"))),
            ..ThingListFilter::default()
        }),
        Tri::Yes
    );
    assert_eq!(
        over(ThingListFilter {
            is_null: Some(true),
            ..ThingListFilter::default()
        }),
        Tri::No
    );

    assert!(Thing::list_confirm(&items, &ThingListFilter::default(), &cx).await);
    assert!(
        Thing::list_confirm(
            &items,
            &ThingListFilter {
                some: Some(Box::new(named("plan"))),
                none: Some(Box::new(named("nothing"))),
                ..ThingListFilter::default()
            },
            &cx
        )
        .await
    );
    for refusing in [
        ThingListFilter {
            is_null: Some(true),
            ..ThingListFilter::default()
        },
        ThingListFilter {
            some: Some(Box::new(named("nothing"))),
            ..ThingListFilter::default()
        },
        ThingListFilter {
            every: Some(Box::new(named("plan"))),
            ..ThingListFilter::default()
        },
        ThingListFilter {
            none: Some(Box::new(named("plan"))),
            ..ThingListFilter::default()
        },
    ] {
        assert!(!Thing::list_confirm(&items, &refusing, &cx).await);
    }
    assert!(
        Thing::list_confirm(
            &items,
            &ThingListFilter {
                every: Some(Box::new(ThingFilter::default())),
                ..ThingListFilter::default()
            },
            &cx
        )
        .await
    );
}

/// An enum comparator compares by value, every way it can be asked.
#[test]
fn an_enum_compares_by_value() {
    let cx = MatchCx::at(0);
    let asked = |filter: ShadeFilter| enum_test(&Shade::Light, &filter, &cx);
    assert_eq!(asked(ShadeFilter::default()), Tri::Yes);
    assert_eq!(
        asked(ShadeFilter {
            eq: Some(Shade::Light),
            ..ShadeFilter::default()
        }),
        Tri::Yes
    );
    assert_eq!(
        asked(ShadeFilter {
            eq: Some(Shade::Dark),
            ..ShadeFilter::default()
        }),
        Tri::No
    );
    assert_eq!(
        asked(ShadeFilter {
            ne: Some(Shade::Dark),
            ..ShadeFilter::default()
        }),
        Tri::Yes
    );
    assert_eq!(
        asked(ShadeFilter {
            ne: Some(Shade::Light),
            ..ShadeFilter::default()
        }),
        Tri::No
    );
    assert_eq!(
        asked(ShadeFilter {
            within: Some(vec![Shade::Light]),
            ..ShadeFilter::default()
        }),
        Tri::Yes
    );
    assert_eq!(
        asked(ShadeFilter {
            within: Some(vec![Shade::Dark]),
            ..ShadeFilter::default()
        }),
        Tri::No
    );
    assert_eq!(
        asked(ShadeFilter {
            not_in: Some(vec![Shade::Dark]),
            ..ShadeFilter::default()
        }),
        Tri::Yes
    );
    assert_eq!(
        asked(ShadeFilter {
            not_in: Some(vec![Shade::Light]),
            ..ShadeFilter::default()
        }),
        Tri::No
    );
    assert_eq!(
        asked(ShadeFilter {
            is_null: Some(true),
            ..ShadeFilter::default()
        }),
        Tri::No
    );
}

/// The second phase's accumulator stops at the first refusal.
#[tokio::test]
async fn the_second_phase_stops_at_the_first_refusal() {
    let cx = MatchCx::at(0);
    let thing = thing();
    let mut confirm = Confirm::new(&cx);
    confirm.field(None::<&StringFilter>, &thing.name).await;
    assert!(Confirm::new(&cx).finish());

    let before = READS.load(Counting::SeqCst);
    confirm
        .field(
            Some(&StringFilter {
                eq: Some("other".to_owned()),
                ..StringFilter::default()
            }),
            &thing.name,
        )
        .await;
    confirm
        .io(
            Some(&StringFilter {
                eq: Some("written".to_owned()),
                ..StringFilter::default()
            }),
            thing.read_note(),
        )
        .await;
    confirm
        .field(
            Some(&StringFilter {
                eq: Some("plan".to_owned()),
                ..StringFilter::default()
            }),
            &thing.name,
        )
        .await;
    assert!(!confirm.finish());
    assert_eq!(
        READS.load(Counting::SeqCst),
        before,
        "nothing is read after a refusal"
    );

    let mut confirm = Confirm::new(&cx);
    confirm.io(None::<&StringFilter>, thing.read_note()).await;
    assert!(confirm.finish());
    assert_eq!(
        READS.load(Counting::SeqCst),
        before,
        "a field nobody asked about is not read"
    );
}

/// The second phase refuses a variant the value is not, without asking.
#[tokio::test]
async fn the_second_phase_refuses_another_variant() {
    let cx = MatchCx::at(0);
    let mut confirm = Confirm::new(&cx);
    confirm
        .variant(None::<&StringFilter>, "one", &[false, true], 0)
        .await;
    assert!(!confirm.finish());

    let mut confirm = Confirm::new(&cx);
    confirm
        .variant(
            Some(&StringFilter {
                eq: Some("one".to_owned()),
                ..StringFilter::default()
            }),
            "one",
            &[true, false],
            0,
        )
        .await;
    assert!(confirm.finish());
}

/// An answer that is already settled needs no reads to confirm.
#[tokio::test]
async fn a_settled_answer_confirms_itself() {
    assert!(settled(Tri::Yes).await);
    assert!(!settled(Tri::No).await);
    assert!(!settled(Tri::Io).await);
}
