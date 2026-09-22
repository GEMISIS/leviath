# leviath-graphql-derive

One attribute, `#[mirror]`, which reads a GraphQL output type and writes the
filter input that mirrors it.

```rust
#[mirror(list)]
#[Object]
impl Region {
    /// Region name, unique within the layout that declares it.
    async fn name(&self) -> &str { &self.region().name }
}
```

That produces the object type `RegionOutput`, the input object `RegionInput`
with a `name: StringFilter` field carrying the same doc comment, the quantifier
input `RegionListInput` with `some` / `every` / `none`, and the trait impls that
hand every decision to a runtime module the caller points the macro at.

A resolver that shapes its answer for the client — a page of a list rather than
the list — says what a filter on it is really about:

```rust
#[filter(io, with = "run_relations::stages_of", ty = "Vec<StageRecord>")]
async fn stages(&self, first: i32) -> Connection<StageRecord> { ... }
```

`with` names the accessor the mirror reads the value through, `ty` the type it
reads it as, and `io` puts that read in the phase where reads are allowed.

The macro emits data and delegation only. Nothing it writes decides anything:
the three-valued matching logic, the null rule and the list quantifiers all live
in hand-written code, so they are read, reviewed and measured once.

This crate is published because `leviath-cli` depends on it. It is written for
that one schema and its runtime contract, so it is unlikely to be useful on its
own.
